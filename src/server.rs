// lode/server: request dispatch + the live daemon state.
//
// Holds the driver registry, the open connections (each behind its own async
// mutex, addressed by conn_id), and the running/finished calls (addressed by
// call_id). A `query.execute` returns its call_id IMMEDIATELY and runs the
// statement in a background task; when it finishes the task emits a `query.state`
// notification and the Lua side pages the buffered result. Cancellation uses the
// driver's detached cancel handle, so a running query can be stopped even while
// its connection is busy.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{json, Value as Json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex as AsyncMutex;

use crate::driver::{CancelHandle, Connection, ResultStream};
use crate::net::NetContext;
use crate::registry::Registry;
use crate::rpc::{self, Request};
use crate::spec::{AuthSpec, Column, ConnSpec, DriverMeta, ObjRef, ParamType, RowPlan, Value};
use crate::store::Store;

type Conn = Arc<AsyncMutex<Box<dyn Connection>>>;

/// How long `conn.test`'s endpoint stage waits for a TCP socket before calling
/// the host unreachable — long enough for a slow remote, short enough that a
/// blackholed address does not hang the form's Test button.
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// A live connection plus the NetContext that owns its SSH tunnel (if any) — the
/// tunnel stays up as long as this entry lives, and is torn down on disconnect.
struct ConnEntry {
    conn: Conn,
    /// Which driver this connection IS. Recorded rather than taken from a
    /// request, so a write is classified by the engine that will run it and not
    /// by whatever the caller claims it is talking to.
    driver: String,
    _net: NetContext,
}

/// The lifecycle of one executed statement.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CallStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl CallStatus {
    fn as_str(&self) -> &'static str {
        match self {
            CallStatus::Running => "running",
            CallStatus::Done => "done",
            CallStatus::Failed => "failed",
            CallStatus::Cancelled => "cancelled",
        }
    }
}

/// One call's server-side state: its result stream, a growing buffer of pulled
/// rows (so the UI can page backwards), and a cancel handle while it runs.
struct Call {
    status: CallStatus,
    columns: Vec<Column>,
    stream: Option<Box<dyn ResultStream>>,
    buffer: Vec<Vec<Value>>,
    exhausted: bool,
    affected: Option<u64>,
    error: Option<String>,
    cancel: Option<Box<dyn CancelHandle>>,
}

impl Call {
    fn new() -> Self {
        Call {
            status: CallStatus::Running,
            columns: Vec::new(),
            stream: None,
            buffer: Vec::new(),
            exhausted: false,
            affected: None,
            error: None,
            cancel: None,
        }
    }
}

/// The daemon. Cloneable Arc handle shared by every dispatch task.
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Registry,
    conns: Mutex<HashMap<u64, ConnEntry>>,
    calls: Mutex<HashMap<u64, Arc<AsyncMutex<Call>>>>,
    next_conn: AtomicU64,
    next_call: AtomicU64,
    out: UnboundedSender<String>,
    /// The saved-state store, or the reason it could not be opened.
    ///
    /// Opened ONCE at startup rather than per call, and a failure is KEPT rather
    /// than retried: an unwritable data directory does not become writable
    /// between two requests, and a client asking for its connections twenty
    /// times would otherwise get twenty identical disk errors with no way to
    /// tell they are the same one. `store.status` reports it verbatim.
    ///
    /// A failed store does NOT fail the engine: every driver, query and schema
    /// method works without it. Only the saved connections, saved queries and
    /// history are unavailable, which is exactly what the clients degrade to.
    store: Result<Store, String>,
}

impl Server {
    pub fn new(out: UnboundedSender<String>) -> Self {
        let store = Store::open().map_err(|e| e.to_string());
        if let Err(e) = &store {
            let _ = out.send(rpc::notification(
                "daemon.log",
                json!({
                    "level": "warn",
                    "message": format!("saved state unavailable: {e} — connections, saved queries and history are off"),
                }),
            ));
        }
        Server {
            inner: Arc::new(Inner {
                registry: Registry::new(),
                conns: Mutex::new(HashMap::new()),
                calls: Mutex::new(HashMap::new()),
                next_conn: AtomicU64::new(1),
                next_call: AtomicU64::new(1),
                out,
                store,
            }),
        }
    }

    /// The store, or an error naming why it is not there.
    fn store(&self) -> anyhow::Result<&Store> {
        self.inner
            .store
            .as_ref()
            .map_err(|e| anyhow::anyhow!("saved state unavailable: {e}"))
    }

    fn send(&self, line: String) {
        let _ = self.inner.out.send(line);
    }

    /// Which driver a live connection belongs to.
    fn conn_driver(&self, id: u64) -> anyhow::Result<String> {
        self.inner
            .conns
            .lock()
            .unwrap()
            .get(&id)
            .map(|e| e.driver.clone())
            .ok_or_else(|| anyhow::anyhow!("no such connection {id}"))
    }

    fn conn(&self, id: u64) -> anyhow::Result<Conn> {
        self.inner
            .conns
            .lock()
            .unwrap()
            .get(&id)
            .map(|e| e.conn.clone())
            .ok_or_else(|| anyhow::anyhow!("no such connection {id}"))
    }

    /// Handle one request and emit its response line.
    pub async fn handle(&self, req: Request) {
        let id = req.id;
        // query.execute sends its own response BEFORE spawning its worker (see `execute`), guaranteeing the
        // response precedes any query.state notification for that call.
        if req.method == "query.execute" {
            self.execute(id, req.params).await;
            return;
        }
        let result = self.dispatch(&req.method, req.params).await;
        let line = match result {
            Ok(v) => rpc::response_ok(id, v),
            Err(e) => rpc::response_err(id, &e.to_string()),
        };
        self.send(line);
    }

    async fn dispatch(&self, method: &str, params: Json) -> anyhow::Result<Json> {
        match method {
            "rpc.hello" => self.hello(),
            "conn.connect" => self.connect(params).await,
            "conn.test" => self.test(params).await,
            "conn.disconnect" => self.disconnect(params).await,
            "conn.databases" => self.databases(params).await,
            "conn.switch_database" => self.switch_database(params).await,
            "schema.structure" => self.structure(params).await,
            "schema.columns" => self.columns(params).await,
            "schema.indexes" => self.indexes(params).await,
            "schema.ddl" => self.ddl(params).await,
            "query.page" => self.page(params).await,
            "query.cancel" => self.cancel(params).await,
            "query.release" => self.release(params).await,
            "query.cell" => self.cell(params).await,
            "query.classify" => self.classify(params),
            "row.write" => self.row_write(params).await,
            // ── saved state (proto 2) ───────────────────────────────────────
            "store.status" => self.store_status(),
            "store.connections.list" => self.store_conn_list().await,
            "store.connections.get" => self.store_conn_get(params).await,
            "store.connections.save" => self.store_conn_save(params).await,
            "store.connections.remove" => self.store_conn_remove(params).await,
            "store.queries.list" => self.store_query_list(params).await,
            "store.queries.get" => self.store_query_get(params).await,
            "store.queries.save" => self.store_query_save(params).await,
            "store.queries.remove" => self.store_query_remove(params).await,
            "store.history.record" => self.store_history_record(params).await,
            "store.history.list" => self.store_history_list(params).await,
            "store.import" => self.store_import(params).await,
            other => Err(anyhow::anyhow!("unknown method '{other}'")),
        }
    }

    // ── handshake ────────────────────────────────────────────────────────────

    /// The handshake. `engine` + `version` IDENTIFY the backend: a client that
    /// may be talking either to lode or to an older, plugin-bundled daemon reads
    /// them to report which engine it got (they are absent on the old daemon, so
    /// a missing `engine` means "legacy", never "broken").
    fn hello(&self) -> anyhow::Result<Json> {
        Ok(json!({
            "engine": "lode",
            "version": env!("CARGO_PKG_VERSION"),
            "proto": rpc::PROTO,
            "drivers": self.inner.registry.metas(),
        }))
    }

    // ── connections ──────────────────────────────────────────────────────────

    async fn connect(&self, params: Json) -> anyhow::Result<Json> {
        let spec: ConnSpec = serde_json::from_value(params)?;
        let driver = self.inner.registry.get(&spec.driver)?;
        let net = NetContext::new(spec.tunnel.clone());
        let conn = driver.connect(&spec, net.clone()).await?;
        // Encryption status: native TLS negotiated OR the link rides an SSH tunnel.
        let encrypted = conn.encrypted() || net.tunneled();
        let tunneled = net.tunneled();
        let id = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);
        self.inner.conns.lock().unwrap().insert(
            id,
            ConnEntry {
                conn: Arc::new(AsyncMutex::new(conn)),
                driver: spec.driver.clone(),
                _net: net,
            },
        );
        // Surface a clear warning when a network link ended up UNENCRYPTED (never
        // silent): the client shows it and health flags it.
        if !encrypted {
            self.send(rpc::notification(
                "daemon.log",
                json!({
                    "level": "warn",
                    "message": format!("connection {id} to driver '{}' is NOT encrypted (no TLS negotiated and no SSH tunnel)", spec.driver),
                }),
            ));
        }
        Ok(json!({ "conn_id": id, "encrypted": encrypted, "tunneled": tunneled }))
    }

    /// Dry-run ONE stage of a connection spec, without keeping anything open —
    /// what the form's per-tab Test button calls. Each stage exercises the REAL
    /// machinery of the layer it names (no simulation): the endpoint stage dials
    /// the TCP socket (through the SSH tunnel when one is configured), the tunnel
    /// stage stands the SSH forward up on its own, and the tls/auth stages open a
    /// full driver connection and close it again. An unusable spec surfaces as an
    /// RPC error carrying the underlying message — the client shows it verbatim.
    async fn test(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            stage: String,
            spec: ConnSpec,
        }
        let p: P = serde_json::from_value(params)?;
        let meta = self
            .inner
            .registry
            .metas()
            .into_iter()
            .find(|m| m.kind == p.spec.driver)
            .ok_or_else(|| anyhow::anyhow!("unknown driver '{}'", p.spec.driver))?;

        let started = Instant::now();
        let detail = match p.stage.as_str() {
            "endpoint" => Self::test_endpoint(&p.spec, meta).await?,
            "tunnel" => Self::test_tunnel(&p.spec, meta).await?,
            "tls" | "auth" => self.test_connect(&p.spec, &p.stage).await?,
            other => return Err(anyhow::anyhow!("unknown test stage '{other}'")),
        };
        Ok(json!({
            "ok": true,
            "ms": started.elapsed().as_millis() as u64,
            "detail": detail,
        }))
    }

    /// Endpoint stage — is the thing we would dial actually THERE? A file-backed
    /// driver (its meta declares a `File` param) checks the path; a TCP driver
    /// asks `NetContext::probe`, which dials the server directly when there is no
    /// tunnel and asks the SSH server to reach it when there is. A driver with
    /// neither (an HTTPS API like Snowflake) has nothing to probe short of a real
    /// connect, and says so.
    ///
    /// This used to call `net.resolve` and connect to what came back. On a
    /// tunnelled spec that is the LOCAL forward's listener, so it answered yes as
    /// soon as the SSH session was up and the far side had not been asked at all:
    /// a MongoDB connection to 127.0.0.1:9042 with nothing listening passed in
    /// 251 ms. The probe now goes to the far end, which is what this stage claims
    /// to be about.
    async fn test_endpoint(spec: &ConnSpec, meta: &'static DriverMeta) -> anyhow::Result<String> {
        if let Some(fp) = meta.params.iter().find(|p| matches!(p.kind, ParamType::File)) {
            let raw = spec.param(fp.key)?;
            let path = crate::spec::expand_tilde(raw);
            let md = std::fs::metadata(&path).map_err(|e| anyhow::anyhow!("cannot stat '{path}': {e}"))?;
            if !md.is_file() {
                return Err(anyhow::anyhow!("'{path}' is not a regular file"));
            }
            std::fs::File::open(&path).map_err(|e| anyhow::anyhow!("cannot open '{path}' for reading: {e}"))?;
            return Ok(format!("{path} — readable, {} bytes", md.len()));
        }

        let Some(host) = spec.param_opt("host") else {
            return Err(anyhow::anyhow!(
                "'{}' has no TCP endpoint to probe — use the Auth tab's test (a full connect)",
                meta.kind
            ));
        };
        let port = spec.port(meta.default_port.unwrap_or(0));
        if port == 0 {
            return Err(anyhow::anyhow!("no port set and '{}' declares no default", meta.kind));
        }
        let net = NetContext::new(spec.tunnel.clone());
        net.probe(host, port, DIAL_TIMEOUT).await
    }

    /// Tunnel stage — stand the SSH session + local forward up on their own, so a
    /// tunnel problem (host, user, key, passphrase) is reported as a TUNNEL error
    /// rather than hiding behind a driver connect failure. The forward is torn
    /// down when `net` drops at the end of this call.
    async fn test_tunnel(spec: &ConnSpec, meta: &'static DriverMeta) -> anyhow::Result<String> {
        let Some(t) = spec.tunnel.clone() else {
            return Err(anyhow::anyhow!("no SSH tunnel configured on this connection"));
        };
        // The forward needs a target: the DB endpoint the tunnel would front.
        let host = spec
            .param_opt("host")
            .ok_or_else(|| anyhow::anyhow!("a tunnel needs a host to forward to — fill the Connection tab first"))?;
        let port = spec.port(meta.default_port.unwrap_or(0));
        if port == 0 {
            return Err(anyhow::anyhow!("no port set and '{}' declares no default", meta.kind));
        }
        let user = t.user.clone();
        let (shost, sport) = (t.host.clone(), t.port);
        let net = NetContext::new(Some(t));
        let addr = net.resolve(host, port).await?;
        Ok(format!(
            "ssh {user}@{shost}:{sport} authenticated — forwarding {addr} → {host}:{port}"
        ))
    }

    /// TLS / auth stages — a REAL driver connect (tunnel, TLS handshake and
    /// credentials all exercised end to end), closed again immediately. The two
    /// stages differ only in what they report: the encryption posture, or the
    /// accepted identity.
    async fn test_connect(&self, spec: &ConnSpec, stage: &str) -> anyhow::Result<String> {
        let driver = self.inner.registry.get(&spec.driver)?;
        let net = NetContext::new(spec.tunnel.clone());
        let conn = driver.connect(spec, net.clone()).await?;
        let native = conn.encrypted();
        let tunneled = net.tunneled();
        let _ = conn.close().await;

        if stage == "tls" {
            return Ok(match (native, tunneled) {
                (true, true) => "encrypted — native TLS negotiated, and the link also rides the SSH tunnel".into(),
                (true, false) => "encrypted — native TLS negotiated".into(),
                (false, true) => "encrypted — no native TLS, but the link rides the SSH tunnel".into(),
                (false, false) => {
                    return Err(anyhow::anyhow!(
                        "connected but the link is PLAINTEXT — no TLS negotiated and no SSH tunnel"
                    ))
                }
            });
        }
        let who = match &spec.auth {
            AuthSpec::None => "no credentials (anonymous)".to_string(),
            AuthSpec::Password { user, .. } => format!("password auth accepted for '{user}'"),
            AuthSpec::ClientCert { user, .. } if !user.is_empty() => {
                format!("client-certificate auth accepted for '{user}'")
            }
            AuthSpec::ClientCert { .. } => "client-certificate auth accepted".to_string(),
            AuthSpec::Provider { provider, user, .. } if !user.is_empty() => {
                format!("{provider} token accepted for '{user}'")
            }
            AuthSpec::Provider { provider, .. } => format!("{provider} token accepted"),
            AuthSpec::Kerberos { principal } => match principal {
                Some(p) => format!("kerberos accepted for '{p}'"),
                None => "kerberos accepted".to_string(),
            },
        };
        Ok(format!(
            "connected — {who}; link {}",
            if native || tunneled { "encrypted" } else { "PLAINTEXT" }
        ))
    }

    async fn disconnect(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
        }
        let p: P = serde_json::from_value(params)?;
        let entry = self.inner.conns.lock().unwrap().remove(&p.conn_id);
        if let Some(entry) = entry {
            // Reclaim the Box out of the Arc<Mutex> to call the by-value close().
            // Dropping `entry._net` afterwards tears down any SSH tunnel.
            if let Ok(mutex) = Arc::try_unwrap(entry.conn) {
                let boxed = mutex.into_inner();
                let _ = boxed.close().await;
            }
        }
        Ok(json!({}))
    }

    async fn databases(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
        }
        let p: P = serde_json::from_value(params)?;
        let conn = self.conn(p.conn_id)?;
        let dbs = conn.lock().await.databases().await?;
        Ok(json!({ "databases": dbs }))
    }

    async fn switch_database(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
            database: String,
        }
        let p: P = serde_json::from_value(params)?;
        let conn = self.conn(p.conn_id)?;
        conn.lock().await.switch_database(&p.database).await?;
        Ok(json!({}))
    }

    // ── schema ───────────────────────────────────────────────────────────────

    async fn structure(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
        }
        let p: P = serde_json::from_value(params)?;
        let conn = self.conn(p.conn_id)?;
        let nodes = conn.lock().await.structure().await?;
        Ok(json!({ "nodes": nodes }))
    }

    async fn columns(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
            object: ObjRef,
        }
        let p: P = serde_json::from_value(params)?;
        let conn = self.conn(p.conn_id)?;
        let cols = conn.lock().await.columns(&p.object).await?;
        Ok(json!({ "columns": cols }))
    }

    async fn indexes(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
            object: ObjRef,
        }
        let p: P = serde_json::from_value(params)?;
        let conn = self.conn(p.conn_id)?;
        let idx = conn.lock().await.indexes(&p.object).await?;
        Ok(json!({ "indexes": idx }))
    }

    async fn ddl(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
            object: ObjRef,
        }
        let p: P = serde_json::from_value(params)?;
        let conn = self.conn(p.conn_id)?;
        // `null` when the engine has no server-side CREATE to give (the trait default). The client shows the
        // helper only where `Caps::ddl` is set, so this is a belt on a driver that lies about its caps.
        let sql = conn.lock().await.ddl(&p.object).await?;
        Ok(json!({ "ddl": sql }))
    }

    // ── query ────────────────────────────────────────────────────────────────

    /// query.execute is special: it sends its OWN response (the call_id) BEFORE spawning the worker, so the
    /// response provably precedes any `query.state` notification that worker emits on the single-writer
    /// channel — otherwise a microsecond statement could beat the response, and the Lua watcher (registered
    /// in the response callback) would miss the state and show the call "running" forever.
    async fn execute(&self, id: u64, params: Json) {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
            statement: String,
        }
        let p: P = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return self.send(rpc::response_err(id, &e.to_string())),
        };
        let conn = match self.conn(p.conn_id) {
            Ok(c) => c,
            Err(e) => return self.send(rpc::response_err(id, &e.to_string())),
        };

        let call_id = self.inner.next_call.fetch_add(1, Ordering::Relaxed);
        let call = Arc::new(AsyncMutex::new(Call::new()));
        self.inner.calls.lock().unwrap().insert(call_id, call.clone());

        // Respond with the call_id NOW — before the worker below can emit a state notification.
        self.send(rpc::response_ok(id, json!({ "call_id": call_id })));

        // Run the statement in the background.
        let server = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let mut guard = conn.lock().await;
            // Register a cancel handle before running, so query.cancel can stop it.
            if let Some(tok) = guard.cancel_token() {
                call.lock().await.cancel = Some(tok);
            }
            let outcome = guard.execute(&p.statement).await;
            drop(guard);

            let ms = started.elapsed().as_millis() as u64;
            let mut c = call.lock().await;
            // Drop the cancel handle the moment the call leaves the running state — a stale handle taken by a
            // late query.cancel would fire a connection-level cancel against whatever that connection runs NEXT.
            c.cancel = None;
            if c.status == CallStatus::Cancelled {
                server.notify_state(call_id, CallStatus::Cancelled, ms, None, None);
                return;
            }
            match outcome {
                Ok(stream) => {
                    c.columns = stream.columns().to_vec();
                    c.affected = stream.affected();
                    c.stream = Some(stream);
                    c.status = CallStatus::Done;
                    let affected = c.affected;
                    server.notify_state(call_id, CallStatus::Done, ms, None, affected);
                }
                Err(e) => {
                    c.status = CallStatus::Failed;
                    c.error = Some(e.to_string());
                    server.notify_state(call_id, CallStatus::Failed, ms, Some(e.to_string()), None);
                }
            }
        });
    }

    fn notify_state(&self, call_id: u64, status: CallStatus, ms: u64, error: Option<String>, affected: Option<u64>) {
        let mut params = json!({
            "call_id": call_id,
            "state": status.as_str(),
            "ms": ms,
        });
        if let Some(e) = error {
            params["error"] = json!(e);
        }
        if let Some(a) = affected {
            params["affected"] = json!(a);
        }
        self.send(rpc::notification("query.state", params));
    }

    fn call(&self, id: u64) -> anyhow::Result<Arc<AsyncMutex<Call>>> {
        self.inner
            .calls
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such call {id}"))
    }

    async fn page(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            call_id: u64,
            #[serde(default)]
            from: usize,
            #[serde(default = "default_n")]
            n: usize,
        }
        let p: P = serde_json::from_value(params)?;
        let call = self.call(p.call_id)?;
        let mut c = call.lock().await;

        match c.status {
            CallStatus::Running => return Ok(json!({ "ready": false })),
            CallStatus::Failed => {
                return Err(anyhow::anyhow!(c
                    .error
                    .clone()
                    .unwrap_or_else(|| "query failed".into())))
            }
            CallStatus::Cancelled => return Err(anyhow::anyhow!("query was cancelled")),
            CallStatus::Done => {}
        }

        // Pull from the stream into the buffer until it holds `from + n` rows
        // (or the stream is exhausted) — this supports backward paging too.
        let need = p.from + p.n;
        while !c.exhausted && c.buffer.len() < need {
            let want = need - c.buffer.len();
            let pulled = match c.stream.as_mut() {
                Some(s) => s.next_page(want).await?,
                None => break,
            };
            match pulled {
                Some(mut rows) => c.buffer.append(&mut rows),
                None => c.exhausted = true,
            }
        }

        // Total, best source first: a driver's UP-FRONT count (e.g. Mongo `count_documents` for a find),
        // else the exact buffered length once the stream has been read to the end, else unknown.
        let total = c
            .stream
            .as_ref()
            .and_then(|s| s.total())
            .or(if c.exhausted { Some(c.buffer.len()) } else { None });
        let start = p.from.min(c.buffer.len());
        let end = need.min(c.buffer.len());
        let rows = &c.buffer[start..end];
        let has_more = !c.exhausted || end < c.buffer.len();

        Ok(json!({
            "ready": true,
            "columns": c.columns,
            "rows": rows,
            "from": start,
            "has_more": has_more,
            "total": total,
            "affected": c.affected,
        }))
    }

    async fn cancel(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            call_id: u64,
        }
        let p: P = serde_json::from_value(params)?;
        let call = self.call(p.call_id)?;
        let handle = {
            let mut c = call.lock().await;
            if c.status == CallStatus::Running {
                c.status = CallStatus::Cancelled;
            }
            c.cancel.take()
        };
        if let Some(h) = handle {
            h.cancel().await?;
        }
        Ok(json!({}))
    }

    /// Drop a finished call's server-side buffer (all its paged rows). The Lua client releases the call_id it
    /// replaces on the next run and the temporary count call, so the calls map does not grow without bound
    /// across a long browsing session. The log's rerun re-executes by STATEMENT, never by old call_id.
    async fn release(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            call_id: u64,
        }
        let p: P = serde_json::from_value(params)?;
        self.inner.calls.lock().unwrap().remove(&p.call_id);
        Ok(json!({}))
    }

    /// Is this statement destructive, and why?
    ///
    /// Asked BEFORE running one, so a client can confirm. It takes a driver
    /// kind rather than a connection: the question is about the language, the
    /// answer is the same whether or not anything is open, and a console should
    /// be able to ask it before it connects.
    ///
    /// The reason is prose, not a flag, because "DELETE with no WHERE clause
    /// removes EVERY row" is what makes someone stop and `dangerous: true` is
    /// not.
    fn classify(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            driver: String,
            statement: String,
        }
        let p: P = serde_json::from_value(params)?;
        let driver = self.inner.registry.get(&p.driver)?;
        let reason = driver.classify(&p.statement);
        Ok(json!({ "dangerous": reason.is_some(), "reason": reason }))
    }

    /// Change or remove ONE row, named by its key.
    ///
    /// Three things stand between a click in a grid and a changed table, and all
    /// three live HERE rather than in a client, because a client that forgets
    /// one is a client that writes silently:
    ///
    /// 1. The object must HAVE an identifying key, and the key sent must be
    ///    exactly it — not a subset, and not the other columns' values standing
    ///    in for one. A table with no key is not editable and says so.
    /// 2. Nothing is written without `confirm`. Called without it, this returns
    ///    the statement the driver would run and why it is dangerous, and
    ///    touches nothing. A client cannot skip the question by not asking it.
    /// 3. The driver binds every value and rolls the write back if it turned
    ///    out to match anything other than one row.
    async fn row_write(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
            #[serde(flatten)]
            plan: RowPlan,
            #[serde(default)]
            confirm: bool,
        }
        let p: P = serde_json::from_value(params)?;
        if !p.plan.delete && p.plan.set.is_empty() {
            anyhow::bail!("nothing to change");
        }
        let driver_kind = self.conn_driver(p.conn_id)?;
        let driver = self.inner.registry.get(&driver_kind)?;
        if !driver.meta().caps.edit {
            anyhow::bail!(
                "the {driver_kind} driver does not edit rows \u{2014} it cannot promise to bind \
                 values and undo a write that hit more than one row, and without both the \
                 feature is a hazard rather than a convenience"
            );
        }

        let conn = self.conn(p.conn_id)?;
        let cols = conn.lock().await.columns(&p.plan.obj).await?;
        let key_cols: Vec<&str> = cols
            .iter()
            .filter(|c| c.primary)
            .map(|c| c.name.as_str())
            .collect();
        if key_cols.is_empty() {
            anyhow::bail!(
                "{} has no primary key, so one of its rows cannot be addressed exactly. \
                 Matching on the other columns' values would be a guess, and a guess that \
                 matches three rows writes three rows",
                p.plan.obj.name
            );
        }
        let sent: Vec<&str> = p.plan.key.iter().map(|(c, _)| c.as_str()).collect();
        let mut want = key_cols.clone();
        let mut got = sent.clone();
        want.sort_unstable();
        got.sort_unstable();
        if want != got {
            anyhow::bail!(
                "the key must be exactly ({}) \u{2014} got ({})",
                key_cols.join(", "),
                sent.join(", ")
            );
        }
        // Changing a key column would move the row rather than edit it, and the
        // WHERE that found it would no longer describe it. That is a different
        // operation and it is not this one.
        if let Some((c, _)) = p
            .plan
            .set
            .iter()
            .find(|(c, _)| key_cols.contains(&c.as_str()))
        {
            anyhow::bail!(
                "{c} is part of the key: changing it moves the row rather than editing it"
            );
        }

        let statement = conn.lock().await.plan_row(&p.plan).await?;
        let reason = driver.classify(&statement);
        if !p.confirm {
            return Ok(json!({
                "applied": false,
                "statement": statement,
                "dangerous": reason.is_some(),
                "reason": reason,
            }));
        }
        let out = conn.lock().await.write_row(&p.plan).await?;
        Ok(json!({
            "applied": true,
            "statement": out.statement,
            "affected": out.affected,
            "dangerous": reason.is_some(),
            "reason": reason,
        }))
    }

    async fn cell(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            call_id: u64,
            row: usize,
            col: usize,
        }
        let p: P = serde_json::from_value(params)?;
        let call = self.call(p.call_id)?;
        let c = call.lock().await;
        let value = c.buffer.get(p.row).and_then(|r| r.get(p.col)).cloned();
        match value {
            Some(v) => Ok(json!({ "value": v })),
            None => Err(anyhow::anyhow!("cell out of range")),
        }
    }

    // ── saved state ──────────────────────────────────────────────────────────
    //
    // These methods are the reason the engine is shareable. The saved
    // connections, the saved queries and the history used to live in the
    // client, which meant the editor plugin and the GUI each had their own set;
    // now they live here and both see one. Everything below is a thin shell over
    // `store.rs`: decode the params, call the store, hand back JSON.
    //
    // `store.status` is the ONE method that answers even when the store failed
    // to open — a client needs to be able to ask "is this off, and why" without
    // getting an error it has to parse.

    fn store_status(&self) -> anyhow::Result<Json> {
        Ok(match &self.inner.store {
            Ok(s) => json!({ "available": true, "path": s.path().display().to_string(), "error": Json::Null }),
            Err(e) => json!({ "available": false, "path": Json::Null, "error": e }),
        })
    }

    async fn store_conn_list(&self) -> anyhow::Result<Json> {
        Ok(json!({ "connections": self.store()?.list_connections().await? }))
    }

    async fn store_conn_get(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "connection": self.store()?.get_connection(p.name).await? }))
    }

    async fn store_conn_save(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            name: String,
            driver: String,
            #[serde(default)]
            spec: Json,
        }
        let p: P = serde_json::from_value(params)?;
        if p.name.trim().is_empty() {
            return Err(anyhow::anyhow!("a connection needs a name"));
        }
        self.store()?.save_connection(p.name, p.driver, p.spec).await?;
        Ok(json!({ "saved": true }))
    }

    async fn store_conn_remove(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "removed": self.store()?.remove_connection(p.name).await? }))
    }

    async fn store_query_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "queries": self.store()?.list_queries(p.conn).await? }))
    }

    async fn store_query_get(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "query": self.store()?.get_query(p.conn, p.name).await? }))
    }

    async fn store_query_save(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
            sql: String,
        }
        let p: P = serde_json::from_value(params)?;
        if p.name.trim().is_empty() {
            return Err(anyhow::anyhow!("a saved query needs a name"));
        }
        self.store()?.save_query(p.conn, p.name, p.sql).await?;
        Ok(json!({ "saved": true }))
    }

    async fn store_query_remove(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "removed": self.store()?.remove_query(p.conn, p.name).await? }))
    }

    async fn store_history_record(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            conn: Option<String>,
            #[serde(default)]
            driver: Option<String>,
            statement: String,
            #[serde(default)]
            state: Option<String>,
            #[serde(default)]
            ms: Option<i64>,
            #[serde(default)]
            rows: Option<i64>,
        }
        let p: P = serde_json::from_value(params)?;
        self.store()?
            .record_history(p.conn, p.driver, p.statement, p.state, p.ms, p.rows)
            .await?;
        Ok(json!({ "recorded": true }))
    }

    async fn store_history_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize, Default)]
        struct P {
            #[serde(default)]
            limit: Option<i64>,
        }
        let p: P = serde_json::from_value(params).unwrap_or_default();
        Ok(json!({ "history": self.store()?.list_history(p.limit).await? }))
    }

    async fn store_import(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            path: String,
        }
        let p: P = serde_json::from_value(params)?;
        let path = std::path::PathBuf::from(crate::spec::expand_tilde(&p.path));
        self.store()?.import_from(path).await
    }
}

fn default_n() -> usize {
    200
}
