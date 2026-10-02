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
    /// What an empty editor should suggest, in this engine's language.
    /// Absent on an engine older than protocol 3.
    #[serde(default)]
    pub hint: String,
    /// How to read the first rows of one object. See [`render_template`].
    #[serde(default)]
    pub browse: String,
    /// Per-node-kind overrides of `browse`.
    #[serde(default)]
    pub browse_by_kind: Vec<(String, String)>,
    /// The inputs of a structured query bar. EMPTY means this engine is typed
    /// as free text.
    #[serde(default)]
    pub query_form: Vec<QueryFieldSpec>,
    /// The skeleton `query_form`'s fragments are assembled into.
    #[serde(default)]
    pub query_template: String,
    /// Fields that narrow a browse. Absent from an older engine, which is why it
    /// defaults rather than failing: a client that can filter talking to one
    /// that cannot simply offers no filter.
    #[serde(default)]
    pub browse_form: Vec<QueryFieldSpec>,
}

/// One input of a structured query bar, as the engine declares it.
#[derive(Debug, Clone, Deserialize)]
pub struct QueryFieldSpec {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub placeholder: String,
    /// What this field adds to the statement when non-empty; `{value}` is the
    /// typed text.
    #[serde(default)]
    pub fragment: String,
    #[serde(default)]
    pub default: String,
    #[serde(default = "one")]
    pub rows: u8,
}

fn one() -> u8 {
    1
}

impl DriverMeta {
    /// The browse template for an object of this `kind`, falling back to the
    /// general one.
    pub fn browse_for(&self, kind: &str) -> &str {
        self.browse_by_kind
            .iter()
            .find(|(k, _)| k == kind)
            .map(|(_, t)| t.as_str())
            .unwrap_or(&self.browse)
    }

    /// Whether this engine wants a structured bar rather than a free editor.
    pub fn is_structured(&self) -> bool {
        !self.query_form.is_empty() && !self.query_template.is_empty()
    }
}

/// Substitute an engine-supplied statement template.
///
/// `{qualified}`, `{name}`, `{schema}`, `{limit}` and `{fragments}`. The client
/// holds every one of these values, so the substitution happens here rather than
/// over a round trip — but the TEMPLATE is the engine's, which is the point: no
/// client decides what SQL or what command document a driver wants.
pub fn render_template(
    template: &str,
    name: &str,
    schema: Option<&str>,
    limit: usize,
    fragments: &[String],
) -> String {
    render_with(template, name, schema, limit, fragments, &[])
}

/// As `render_template`, plus `{clauses}` — SQL's own kind of fragment.
///
/// `{fragments}` is comma-joined and comma-prefixed, which is right for a JSON
/// document and wrong for SQL: `WHERE x, ORDER BY y` is not a statement. Clauses
/// are joined with a space and collapse to nothing when empty, so
/// `SELECT * FROM t {clauses} LIMIT 200;` is valid with none, one or both.
pub fn render_with(
    template: &str,
    name: &str,
    schema: Option<&str>,
    limit: usize,
    fragments: &[String],
    clauses: &[String],
) -> String {
    let schema = schema.filter(|s| !s.is_empty());
    let qualified = match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    };
    // Empty means the skeleton stands alone and must still be valid, so there is
    // no separator either; one or more are joined and prefixed, so a skeleton is
    // written as `{"find": "{name}"{fragments}}` and works both ways.
    let joined = if fragments.is_empty() {
        String::new()
    } else {
        format!(", {}", fragments.join(", "))
    };
    // A clause list that is empty must leave NO double space behind: the
    // statement is shown to the person who runs it.
    let clause_text = clauses.join(" ");
    let out = template
        .replace("{clauses}", &clause_text)
        .replace("{qualified}", &qualified)
        .replace("{name}", name)
        .replace("{schema}", schema.unwrap_or(""))
        .replace("{limit}", &limit.to_string())
        .replace("{fragments}", &joined);
    collapse_spaces(&out)
}

/// Squeeze runs of spaces, leaving a statement that reads as one written by a
/// person. Only between tokens — never inside a quoted string.
fn collapse_spaces(s: &str) -> String {
    let (mut out, mut quote, mut last_space) = (String::with_capacity(s.len()), None::<char>, false);
    for c in s.chars() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    out.push(c);
                    last_space = false;
                } else if c == ' ' {
                    if !last_space {
                        out.push(c);
                    }
                    last_space = true;
                } else {
                    out.push(c);
                    last_space = false;
                }
            }
        }
    }
    out.replace(" ;", ";").trim().to_string()
}

/// Assemble the clauses a browse form produced, then render the browse.
pub fn render_browse(
    meta: &DriverMeta,
    kind: &str,
    values: &std::collections::HashMap<String, String>,
    name: &str,
    schema: Option<&str>,
    limit: usize,
) -> String {
    let clauses: Vec<String> = meta
        .browse_form
        .iter()
        .filter_map(|f| {
            let v = values.get(&f.key).map(String::as_str).unwrap_or("").trim();
            (!v.is_empty() && !f.fragment.is_empty())
                .then(|| f.fragment.replace("{value}", v))
        })
        .collect();
    render_with(meta.browse_for(kind), name, schema, limit, &[], &clauses)
}

/// Assemble a structured query bar's values into a statement, using only what
/// the engine declared.
pub fn render_query(
    meta: &DriverMeta,
    values: &std::collections::HashMap<String, String>,
    name: &str,
    schema: Option<&str>,
    limit: usize,
) -> String {
    let fragments: Vec<String> = meta
        .query_form
        .iter()
        .filter_map(|f| {
            let v = values.get(&f.key).map(String::as_str).unwrap_or("").trim();
            if v.is_empty() || f.fragment.is_empty() {
                None
            } else {
                Some(f.fragment.replace("{value}", v))
            }
        })
        .collect();
    render_template(&meta.query_template, name, schema, limit, &fragments)
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

    fn meta_from(json: Value) -> DriverMeta {
        serde_json::from_value(json).unwrap()
    }

    /// The statement a click on a table produces is the ENGINE's template with
    /// values put in — not a format string in the client. These are the real
    /// templates the drivers declare.
    /// A browse with nothing filled in must be exactly the statement it was
    /// before this existed — no stray spaces, no clause, no change.
    #[test]
    fn an_empty_browse_form_leaves_the_statement_alone() {
        let out = render_with(
            "SELECT * FROM {qualified} {clauses} LIMIT {limit};",
            "people",
            Some("main"),
            200,
            &[],
            &[],
        );
        assert_eq!(out, "SELECT * FROM main.people LIMIT 200;");
    }

    #[test]
    fn clauses_join_with_a_space_and_land_before_the_limit() {
        let out = render_with(
            "SELECT * FROM {qualified} {clauses} LIMIT {limit};",
            "people",
            None,
            50,
            &[],
            &["WHERE balance < 0".into(), "ORDER BY balance DESC".into()],
        );
        assert_eq!(
            out,
            "SELECT * FROM people WHERE balance < 0 ORDER BY balance DESC LIMIT 50;"
        );
    }

    /// Spaces inside a quoted value are the value's, not the statement's.
    #[test]
    fn a_quoted_value_keeps_its_spaces() {
        let out = render_with(
            "SELECT * FROM {name} {clauses};",
            "t",
            None,
            1,
            &[],
            &["WHERE note = 'two  spaces'".into()],
        );
        assert_eq!(out, "SELECT * FROM t WHERE note = 'two  spaces';");
    }

    #[test]
    fn a_browse_statement_is_the_engines_template() {
        assert_eq!(
            render_template("SELECT * FROM {qualified} LIMIT {limit};", "ratings", Some("public"), 200, &[]),
            "SELECT * FROM public.ratings LIMIT 200;"
        );
        // No schema: the qualified name is the bare one, with no stray dot.
        assert_eq!(
            render_template("SELECT * FROM {qualified} LIMIT {limit};", "t", None, 50, &[]),
            "SELECT * FROM t LIMIT 50;"
        );
        assert_eq!(
            render_template("SELECT * FROM {qualified} LIMIT {limit};", "t", Some(""), 50, &[]),
            "SELECT * FROM t LIMIT 50;",
            "an empty schema is no schema"
        );
        // T-SQL says it differently, which is exactly why this is per driver.
        assert_eq!(
            render_template("SELECT TOP {limit} * FROM {qualified};", "t", Some("dbo"), 100, &[]),
            "SELECT TOP 100 * FROM dbo.t;"
        );
        // MongoDB is not SQL at all.
        assert_eq!(
            render_template("{\"find\": \"{name}\", \"limit\": {limit}}", "ratings", Some("schoolexams"), 200, &[]),
            "{\"find\": \"ratings\", \"limit\": 200}"
        );
        // Redis: the command depends on what the key IS.
        let redis = meta_from(json!({
            "kind": "redis", "display": "Redis",
            "browse": "TYPE {name}",
            "browse_by_kind": [["string", "GET {name}"], ["hash", "HGETALL {name}"]],
        }));
        assert_eq!(redis.browse_for("string"), "GET {name}");
        assert_eq!(redis.browse_for("hash"), "HGETALL {name}");
        assert_eq!(redis.browse_for("zset"), "TYPE {name}", "an unlisted kind falls back");
    }

    /// The structured bar assembles a statement out of the engine's own
    /// fragments, and must produce something valid with nothing filled in.
    #[test]
    fn a_structured_query_is_assembled_from_declared_fragments() {
        let mongo = meta_from(json!({
            "kind": "mongodb", "display": "MongoDB",
            "query_template": "{\"find\": \"{name}\"{fragments}}",
            "query_form": [
                { "key": "filter", "label": "Filter", "fragment": "\"filter\": {value}" },
                { "key": "sort", "label": "Sort", "fragment": "\"sort\": {value}" },
                { "key": "limit", "label": "Limit", "fragment": "\"limit\": {value}", "default": "50" }
            ]
        }));
        assert!(mongo.is_structured());

        let mut v = std::collections::HashMap::new();
        // Nothing filled in: the skeleton alone has to be valid JSON.
        let empty = render_query(&mongo, &v, "ratings", Some("schoolexams"), 200);
        assert_eq!(empty, "{\"find\": \"ratings\"}");
        serde_json::from_str::<Value>(&empty).expect("a bare skeleton must still parse");

        v.insert("limit".into(), "50".into());
        v.insert("filter".into(), "{\"name\": 1}".into());
        v.insert("sort".into(), "   ".into()); // whitespace is empty
        let out = render_query(&mongo, &v, "ratings", None, 200);
        assert_eq!(out, "{\"find\": \"ratings\", \"filter\": {\"name\": 1}, \"limit\": 50}");
        serde_json::from_str::<Value>(&out).expect("a filled form must parse too");
        assert!(!out.contains("sort"), "a blank field contributes nothing");
    }

    /// An engine older than protocol 3 sends none of this, and the client has to
    /// cope rather than show an empty hint and a broken click.
    #[test]
    fn a_pre_protocol_3_driver_meta_still_parses() {
        let old = meta_from(json!({ "kind": "postgres", "display": "PostgreSQL" }));
        assert!(old.hint.is_empty());
        assert!(old.browse.is_empty());
        assert!(!old.is_structured(), "no form declared means the free editor");
        assert_eq!(old.browse_for("table"), "");
    }

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
