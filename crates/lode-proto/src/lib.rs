//! lode-proto: the client side of the lode protocol.
//!
//! Spawns `loded`, writes requests to its stdin, reads its stdout, and hands
//! every line back as a [`Message`]. That is the whole job. There is no
//! connection state here, no caching and no policy: a client decides what to
//! ask and what to do with the answer, because the editor plugin and the GUI
//! disagree about all of that and agree about none of it except the wire.
//!
//! **Non-blocking by default.** [`Engine::send`] returns the request id
//! immediately and [`Engine::try_recv`] drains whatever has arrived. A GUI calls
//! `try_recv` once per frame and never waits on the database; there is no
//! spinner that is secretly a frozen window.
//!
//! [`Engine::call`] is the blocking convenience for the cases where waiting is
//! the honest thing to do — a test, a one-shot CLI, the first handshake. It
//! never *loses* what it is not waiting for: messages that are not the response
//! it wants go into a holding buffer and come back out of `try_recv` in order.
//!
//! **No tokio.** The engine needs a runtime because it does the I/O; a client
//! does not. One reader thread plus a mutex over the pipe drops into any event
//! loop without making that loop async.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The lowest engine protocol this client can work with.
pub const PROTO_MIN: u32 = 2;

// ── the wire ───────────────────────────────────────────────────────────────────

/// One line from the engine.
#[derive(Debug, Clone)]
pub enum Message {
    /// A reply to the request with this `id`.
    Response {
        id: u64,
        result: Result<Value, String>,
    },
    /// An unsolicited notification: `query.state`, `daemon.log`.
    Notification { method: String, params: Value },
    /// The engine's stdout closed — it exited or crashed. Terminal: nothing
    /// follows it, and every pending request will never be answered.
    Closed,
}

/// What the engine said about itself at `rpc.hello`.
#[derive(Debug, Clone, Deserialize)]
pub struct Hello {
    /// "lode". Absent on the pre-lode daemon that lvim-db used to bundle.
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    pub proto: u32,
    #[serde(default)]
    pub drivers: Vec<DriverMeta>,
}

/// A driver as the engine describes it. This is what a connection form is built
/// FROM — the client does not know the field set of any database.
#[derive(Debug, Clone, Deserialize)]
pub struct DriverMeta {
    pub kind: String,
    pub display: String,
    #[serde(default)]
    pub default_port: Option<u16>,
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    #[serde(default)]
    pub auth: Vec<String>,
    #[serde(default)]
    pub caps: Caps,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParamSpec {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Caps {
    #[serde(default)]
    pub sql: bool,
    #[serde(default)]
    pub schemas: bool,
    #[serde(default)]
    pub cancel: bool,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub tunnel: bool,
    #[serde(default)]
    pub multi_db: bool,
    #[serde(default)]
    pub kv: bool,
    #[serde(default)]
    pub indexes: bool,
    #[serde(default)]
    pub ddl: bool,
}

/// One node of the schema tree.
#[derive(Debug, Clone, Deserialize)]
pub struct Node {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub children: Vec<Node>,
}

/// One column of a result.
#[derive(Debug, Clone, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type", default)]
    pub type_name: String,
}

/// One page of a result, as `query.page` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct Page {
    pub ready: bool,
    #[serde(default)]
    pub columns: Vec<Column>,
    #[serde(default)]
    pub rows: Vec<Vec<Value>>,
    #[serde(default)]
    pub from: usize,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub total: Option<usize>,
    #[serde(default)]
    pub affected: Option<u64>,
}

/// A saved connection, as the engine's store keeps it. `spec` holds the
/// `{ params, auth, tls, tunnel }` template — secrets stay templates, and a
/// client has no business resolving them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedConnection {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub driver: String,
    #[serde(default)]
    pub spec: Value,
    #[serde(default)]
    pub created: Option<i64>,
}

/// A saved query, scoped to a connection by name.
#[derive(Debug, Clone, Deserialize)]
pub struct SavedQuery {
    #[serde(default)]
    pub id: i64,
    pub conn: String,
    pub name: String,
    pub sql: String,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

/// One history row.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryRow {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub conn: Option<String>,
    #[serde(default)]
    pub driver: Option<String>,
    pub statement: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub ms: Option<i64>,
    #[serde(default)]
    pub rows: Option<i64>,
    #[serde(default)]
    pub ts: Option<i64>,
}

/// The lifecycle of one executed statement, from a `query.state` notification.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryState {
    pub call_id: u64,
    pub state: String,
    #[serde(default)]
    pub ms: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub affected: Option<u64>,
}

// ── the engine process ─────────────────────────────────────────────────────────

/// A spawned engine and the pipe to it.
pub struct Engine {
    child: Child,
    /// `Option` so [`Drop`] can TAKE it: dropping the `ChildStdin` is what
    /// closes the pipe, and closing the pipe is how the engine is asked to
    /// exit. A `Mutex<ChildStdin>` could only be killed, never asked.
    stdin: Mutex<Option<ChildStdin>>,
    rx: Receiver<Message>,
    /// Messages pulled off `rx` by a blocking [`Engine::call`] that were not the
    /// response it was waiting for. They are handed back by `try_recv` BEFORE
    /// anything new, so a blocking call never swallows a notification.
    held: Mutex<Vec<Message>>,
    next_id: AtomicU64,
    binary: PathBuf,
}

impl Engine {
    /// Where to look for `loded`, in probe order: `$LODE_DAEMON`, `loded` on
    /// `$PATH`, then the build and target directories of a checkout found by
    /// walking up from this executable.
    pub fn find_binary() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("LODE_DAEMON") {
            let p = PathBuf::from(p);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let c = dir.join("loded");
                if c.is_file() {
                    return Some(c);
                }
            }
        }
        // A development layout: the GUI binary sits in target/{debug,release}/
        // of the same workspace the engine builds into.
        let exe = std::env::current_exe().ok()?;
        let mut dir = exe.parent()?;
        for _ in 0..5 {
            for c in [dir.join("loded"), dir.join("build/loded")] {
                if c.is_file() {
                    return Some(c);
                }
            }
            dir = dir.parent()?;
        }
        None
    }

    /// Spawn the engine. `path` overrides the probe.
    pub fn spawn(path: Option<&Path>) -> anyhow::Result<Self> {
        let binary = match path {
            Some(p) => p.to_path_buf(),
            None => Self::find_binary().ok_or_else(|| {
                anyhow::anyhow!(
                    "no lode engine found — build it (scripts/build) and put `loded` on $PATH \
                     or set $LODE_DAEMON"
                )
            })?,
        };
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is left INHERITED on purpose: the engine only writes there
            // when something is badly wrong, and a piped stderr nobody reads is
            // a deadlock waiting for a full pipe buffer.
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start '{}': {e}", binary.display()))?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        let (tx, rx) = channel();
        std::thread::Builder::new()
            .name("lode-reader".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(line) else {
                        // A line we cannot parse is the engine's problem, not a
                        // reason to tear the client down; skip it.
                        continue;
                    };
                    let msg = if let Some(id) = v.get("id").and_then(Value::as_u64) {
                        let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        let result = if ok {
                            Ok(v.get("result").cloned().unwrap_or(Value::Null))
                        } else {
                            Err(v
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown error")
                                .to_string())
                        };
                        Message::Response { id, result }
                    } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                        Message::Notification {
                            method: method.to_string(),
                            params: v.get("params").cloned().unwrap_or(Value::Null),
                        }
                    } else {
                        continue;
                    };
                    if tx.send(msg).is_err() {
                        return; // the client dropped the Engine
                    }
                }
                let _ = tx.send(Message::Closed);
            })?;

        Ok(Engine {
            child,
            stdin: Mutex::new(Some(stdin)),
            rx,
            held: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            binary,
        })
    }

    /// The binary this engine was started from.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Queue one request and return its id. Does not wait.
    pub fn send(&self, method: &str, params: Value) -> anyhow::Result<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let line = json!({ "id": id, "method": method, "params": params }).to_string();
        let mut guard = self
            .stdin
            .lock()
            .map_err(|_| anyhow::anyhow!("engine stdin lock poisoned"))?;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("the engine's input is closed"))?;
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(id)
    }

    /// Whatever has arrived, oldest first; `None` when nothing has.
    pub fn try_recv(&self) -> Option<Message> {
        if let Ok(mut held) = self.held.lock() {
            if !held.is_empty() {
                return Some(held.remove(0));
            }
        }
        match self.rx.try_recv() {
            Ok(m) => Some(m),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Message::Closed),
        }
    }

    /// Send a request and WAIT for its response. Messages that arrive first are
    /// held, not dropped, so a caller mixing `call` and `try_recv` still sees
    /// every notification.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        let id = self.send(method, params)?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(anyhow::anyhow!("'{method}' timed out after {timeout:?}"));
            }
            match self.rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(Message::Response { id: got, result }) if got == id => {
                    return result.map_err(|e| anyhow::anyhow!("{method}: {e}"));
                }
                Ok(Message::Closed) => {
                    if let Ok(mut held) = self.held.lock() {
                        held.push(Message::Closed);
                    }
                    return Err(anyhow::anyhow!("the engine exited while waiting for '{method}'"));
                }
                Ok(other) => {
                    if let Ok(mut held) = self.held.lock() {
                        held.push(other);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow::anyhow!("the engine exited while waiting for '{method}'"));
                }
            }
        }
    }

    /// The handshake, checked. An engine older than [`PROTO_MIN`] is refused
    /// HERE rather than failing later on a missing method, because "your engine
    /// is too old, build a newer one" is an answer and "unknown method
    /// 'store.status'" is a puzzle.
    pub fn hello(&self, timeout: Duration) -> anyhow::Result<Hello> {
        let v = self.call("rpc.hello", json!({}), timeout)?;
        let hello: Hello = serde_json::from_value(v)?;
        if hello.proto < PROTO_MIN {
            return Err(anyhow::anyhow!(
                "engine protocol {} is too old (need {PROTO_MIN} or newer) — rebuild lode",
                hello.proto
            ));
        }
        Ok(hello)
    }
}

impl Drop for Engine {
    /// Close stdin, which is how the engine is asked to exit, then reap it.
    /// Killing it is the fallback, not the first move: a clean exit closes the
    /// database connections and flushes the store.
    fn drop(&mut self) {
        // Take and drop the pipe: that is the EOF the engine shuts down on.
        if let Ok(mut guard) = self.stdin.lock() {
            if let Some(mut stdin) = guard.take() {
                let _ = stdin.flush();
            }
        }
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── typed convenience over the raw calls ───────────────────────────────────────
//
// Only for the methods a client calls the same way every time. Anything where a
// client would want the raw shape (conn.connect's spec, conn.test's stages) stays
// a `call` with a `Value`, because wrapping it would mean this crate deciding
// what a connection form looks like.

impl Engine {
    pub fn store_available(&self, timeout: Duration) -> anyhow::Result<bool> {
        let v = self.call("store.status", json!({}), timeout)?;
        Ok(v.get("available").and_then(Value::as_bool).unwrap_or(false))
    }

    pub fn connections(&self, timeout: Duration) -> anyhow::Result<Vec<SavedConnection>> {
        let v = self.call("store.connections.list", json!({}), timeout)?;
        Ok(serde_json::from_value(
            v.get("connections").cloned().unwrap_or_else(|| json!([])),
        )?)
    }

    pub fn save_connection(
        &self,
        name: &str,
        driver: &str,
        spec: &Value,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        self.call(
            "store.connections.save",
            json!({ "name": name, "driver": driver, "spec": spec }),
            timeout,
        )?;
        Ok(())
    }

    pub fn remove_connection(&self, name: &str, timeout: Duration) -> anyhow::Result<()> {
        self.call("store.connections.remove", json!({ "name": name }), timeout)?;
        Ok(())
    }

    pub fn saved_queries(&self, conn: &str, timeout: Duration) -> anyhow::Result<Vec<SavedQuery>> {
        let v = self.call("store.queries.list", json!({ "conn": conn }), timeout)?;
        Ok(serde_json::from_value(
            v.get("queries").cloned().unwrap_or_else(|| json!([])),
        )?)
    }

    pub fn save_query(&self, conn: &str, name: &str, sql: &str, timeout: Duration) -> anyhow::Result<()> {
        self.call(
            "store.queries.save",
            json!({ "conn": conn, "name": name, "sql": sql }),
            timeout,
        )?;
        Ok(())
    }

    pub fn remove_query(&self, conn: &str, name: &str, timeout: Duration) -> anyhow::Result<()> {
        self.call(
            "store.queries.remove",
            json!({ "conn": conn, "name": name }),
            timeout,
        )?;
        Ok(())
    }

    pub fn history(&self, limit: i64, timeout: Duration) -> anyhow::Result<Vec<HistoryRow>> {
        let v = self.call("store.history.list", json!({ "limit": limit }), timeout)?;
        Ok(serde_json::from_value(
            v.get("history").cloned().unwrap_or_else(|| json!([])),
        )?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_history(
        &self,
        conn: Option<&str>,
        driver: Option<&str>,
        statement: &str,
        state: &str,
        ms: i64,
        rows: Option<i64>,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        self.call(
            "store.history.record",
            json!({
                "conn": conn, "driver": driver, "statement": statement,
                "state": state, "ms": ms, "rows": rows
            }),
            timeout,
        )?;
        Ok(())
    }

    pub fn structure(&self, conn_id: u64, timeout: Duration) -> anyhow::Result<Vec<Node>> {
        let v = self.call("schema.structure", json!({ "conn_id": conn_id }), timeout)?;
        Ok(serde_json::from_value(
            v.get("nodes").cloned().unwrap_or_else(|| json!([])),
        )?)
    }

    pub fn page(&self, call_id: u64, from: usize, n: usize, timeout: Duration) -> anyhow::Result<Page> {
        let v = self.call(
            "query.page",
            json!({ "call_id": call_id, "from": from, "n": n }),
            timeout,
        )?;
        Ok(serde_json::from_value(v)?)
    }
}

/// Render one result cell as the text a grid shows.
///
/// The engine sends cells as plain JSON, so a NULL and the string "NULL" are
/// genuinely different values here — and a grid that printed both as `NULL`
/// would be lying about the data. `None` means SQL NULL; the caller styles it
/// differently instead of spelling it the same.
pub fn cell_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_text_separates_null_from_the_word() {
        assert_eq!(cell_text(&Value::Null), None);
        assert_eq!(cell_text(&json!("NULL")), Some("NULL".to_string()));
        assert_eq!(cell_text(&json!(12)), Some("12".to_string()));
        assert_eq!(cell_text(&json!(true)), Some("true".to_string()));
    }

    /// The whole round trip against a REAL engine, when one is to hand: spawn,
    /// handshake, connect to a throwaway SQLite file, run a statement, page it.
    /// Skipped (not failed) when no engine is built, so the crate still tests on
    /// a machine that has not built one.
    #[test]
    fn round_trip_against_a_real_engine() {
        let Some(bin) = Engine::find_binary() else {
            eprintln!("skipped: no loded binary found");
            return;
        };
        let t = Duration::from_secs(10);
        let dir = std::env::temp_dir().join(format!("lode-proto-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("LODE_DATA_DIR", &dir);
        let db = dir.join("t.sqlite");

        let eng = Engine::spawn(Some(&bin)).expect("spawn");
        let hello = eng.hello(t).expect("hello");
        assert_eq!(hello.engine.as_deref(), Some("lode"));
        assert!(hello.proto >= PROTO_MIN);
        assert!(hello.drivers.iter().any(|d| d.kind == "sqlite"));

        let conn = eng
            .call(
                "conn.connect",
                json!({ "driver": "sqlite", "params": { "file": db.to_str().unwrap() } }),
                t,
            )
            .expect("connect");
        let conn_id = conn["conn_id"].as_u64().unwrap();

        for stmt in [
            "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)",
            "INSERT INTO t(name) VALUES ('a'),('b'),('c')",
        ] {
            let r = eng
                .call("query.execute", json!({ "conn_id": conn_id, "statement": stmt }), t)
                .expect("execute");
            wait_done(&eng, r["call_id"].as_u64().unwrap());
        }

        let r = eng
            .call(
                "query.execute",
                json!({ "conn_id": conn_id, "statement": "SELECT id, name FROM t ORDER BY id" }),
                t,
            )
            .expect("execute select");
        let call_id = r["call_id"].as_u64().unwrap();
        wait_done(&eng, call_id);

        let page = eng.page(call_id, 0, 2, t).expect("page");
        assert!(page.ready);
        assert_eq!(page.columns.len(), 2);
        assert_eq!(page.rows.len(), 2);
        assert!(page.has_more);
        assert_eq!(cell_text(&page.rows[0][1]), Some("a".to_string()));

        let rest = eng.page(call_id, 2, 10, t).expect("page 2");
        assert_eq!(rest.rows.len(), 1);
        assert_eq!(rest.total, Some(3));

        // The store is reachable too, in the throwaway data dir.
        assert!(eng.store_available(t).unwrap());
        eng.save_connection("probe", "sqlite", &json!({ "params": { "file": "x" } }), t)
            .unwrap();
        assert!(eng.connections(t).unwrap().iter().any(|c| c.name == "probe"));
        eng.remove_connection("probe", t).unwrap();
    }

    /// Drain until the named call reports a terminal state.
    fn wait_done(eng: &Engine, call_id: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(Message::Notification { method, params }) = eng.try_recv() {
                if method == "query.state" {
                    let st: QueryState = serde_json::from_value(params).unwrap();
                    if st.call_id == call_id {
                        assert_eq!(st.state, "done", "{:?}", st.error);
                        return;
                    }
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        panic!("call {call_id} never finished");
    }
}
