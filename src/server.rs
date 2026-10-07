// lode/server: request dispatch + the live daemon state.
//
// Holds the driver registry, the open connections (each behind its own async
// mutex, addressed by conn_id), and the running/finished calls (addressed by
// call_id). A `query.execute` returns its call_id IMMEDIATELY and runs the
// statement in a background task; when it finishes the task emits a `query.state`
// notification and the Lua side pages the buffered result. Cancellation uses the
// driver's detached cancel handle, so a running query can be stopped even while
// its connection is busy.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Deserialize;
use serde_json::{json, Value as Json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex as AsyncMutex;

use crate::audit;
use crate::dashboard;
use crate::driver::{CancelHandle, Connection, ResultStream};
use crate::net::NetContext;
use crate::registry::Registry;
use crate::rpc::{self, Request};
use crate::spec::{
    AuthSpec, Column, ConnSpec, DriverMeta, ObjRef, ParamType, Policy, RowPlan, Value,
};
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
    /// The spec this connection was opened from, kept so the disconnect hooks
    /// can be given the same context the connect hooks were. Its credentials
    /// are `Secret`s and are never read again — the hook environment is built
    /// from the non-secret fields only.
    spec: ConnSpec,
    /// The SAVED connection's name, when the client sent one.
    ///
    /// Recorded for the audit log: a conn_id is a handle that means nothing an
    /// hour later, and `name` is what somebody will search the log by. Optional
    /// because a connection does not have to be a saved one — a client may open
    /// an ad-hoc spec, and the log then says which driver and leaves the name
    /// empty rather than inventing one.
    name: Option<String>,
    /// What this connection is allowed to do.
    policy: Policy,
    /// Which driver this connection IS. Recorded rather than taken from a
    /// request, so a write is classified by the engine that will run it and not
    /// by whatever the caller claims it is talking to.
    driver: String,
    _net: NetContext,
}

/// One cell, as a line of a table a language model reads.
///
/// Tabs and newlines are flattened because the row separator is a newline and
/// the cell separator is a tab: a value containing either would silently become
/// two rows or two columns, and a model would read the shifted table as the
/// truth. NULL is spelled out rather than left blank, because an empty cell and
/// a missing value are different answers and the blank reads as the first.
fn agent_cell(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Text(s) => s.replace(['\t', '\n'], " "),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        // Not the base64: a model given a megabyte of it would spend its whole
        // context on bytes it cannot read. The length is the useful fact.
        Value::Bytes { len, .. } => format!("<{len} bytes>"),
        Value::Json(j) => j.to_string().replace(['\t', '\n'], " "),
        Value::Timestamp(t) => t.clone(),
        Value::Oid(o) => o.clone(),
        // The readable form, which is what it displays as everywhere else. The
        // extended-JSON key is for writing back, and nothing here writes back.
        Value::Ext { text, .. } => text.replace(['\t', '\n'], " "),
    }
}

/// Ask every registered driver PROGRAM to describe itself, and register the
/// ones that can.
///
/// Once, before the engine serves anything, with a short ceiling each. A
/// program that cannot describe itself is RECORDED and skipped: it does not
/// stop the engine, because the other fourteen drivers have nothing to do with
/// it, and it is not silently dropped either — `rpc.hello` carries the reason,
/// so an installed driver missing from the list always has a message attached.
async fn load_external_drivers(
    store: &Result<Store, String>,
    registry: &mut Registry,
    out: &UnboundedSender<String>,
) -> Vec<Json> {
    let mut notes = Vec::new();
    let Ok(store) = store.as_ref() else { return notes };
    let profiles = match store.list_profiles("driver".into()).await {
        Ok(p) => p,
        Err(_) => return notes,
    };
    // Taken BEFORE any of them is asked: a program calling itself `postgres`
    // must lose to the built-in one, not race it.
    let built_in: Vec<&'static str> = registry.kinds();
    for p in profiles {
        let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let command = p
            .get("spec")
            .and_then(|x| x.get("command"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let (kind, error) = if command.trim().is_empty() {
            (None, Some("it has no command to run".to_string()))
        } else {
            match crate::external::probe(&command).await {
                Ok(meta) => {
                    let kind = meta.kind;
                    let driver = Arc::new(crate::external::ExternalDriver::new(meta, command));
                    match registry.add_external(driver, &built_in) {
                        Ok(()) => (Some(kind), None),
                        Err(e) => (Some(kind), Some(e.to_string())),
                    }
                }
                Err(e) => (None, Some(e.to_string())),
            }
        };
        if let Some(e) = &error {
            let _ = out.send(rpc::notification(
                "daemon.log",
                json!({ "level": "warn", "message": format!("driver program '{name}': {e}") }),
            ));
        }
        notes.push(json!({ "name": name, "kind": kind, "ok": error.is_none(), "error": error }));
    }
    notes
}

/// What an MCP session may do, on top of the connection's own policy.
///
/// A floor, never a key: the most restrictive of the role and the connection
/// wins, and a role cannot grant what a connection refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// Nothing that changes the database. The default, because the default for
    /// something that is not a person should be the one that cannot do harm.
    Reader,
    /// Writes, subject to the connection's policy — and every dangerous one
    /// still needs a person.
    Writer,
}

impl AgentRole {
    /// `$LODE_MCP_ROLE`. Anything unrecognised is `Reader`: a typo in a
    /// variable must not quietly hand an agent a pen.
    pub fn from_env() -> AgentRole {
        match std::env::var("LODE_MCP_ROLE").as_deref().map(str::trim) {
            Ok("writer") => AgentRole::Writer,
            _ => AgentRole::Reader,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentRole::Reader => "reader",
            AgentRole::Writer => "writer",
        }
    }
}

/// The two namespaces of profile there are. Checked rather than accepted, so a
/// typo makes an empty list instead of a silent new namespace nobody can find.
fn check_profile_kind(kind: &str) -> anyhow::Result<()> {
    match kind {
        "tunnel" | "proxy" | "access" | "driver" => Ok(()),
        other => Err(anyhow::anyhow!(
            "unknown profile kind '{other}' — there are 'tunnel', 'proxy', 'access' and 'driver'"
        )),
    }
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
    /// What became of each registered driver PROGRAM, by profile name: the kind
    /// it claims, or why it did not become a driver. Reported by `rpc.hello`,
    /// because a driver that is installed and absent from the list is otherwise
    /// a mystery with no message attached to it.
    ///
    /// Plain, not behind a lock, because it is filled before the `Server`
    /// exists — see `Server::new`.
    external: Vec<Json>,
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
    /// How much of a statement the audit log keeps. Read ONCE at startup, not
    /// per call: an audit mode that could change between two statements is an
    /// audit mode a running process could be talked out of.
    audit_mode: audit::Mode,
    next_run: AtomicU64,
    /// Scripts that are running, so one can be stopped. The sender fires the
    /// cancel; removing the entry is what marks a run finished.
    runs: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
}

impl Server {
    /// Build the engine.
    ///
    /// ASYNC because the registry is finished here: every registered driver
    /// PROGRAM is asked to describe itself before the `Server` exists at all.
    /// That is the whole reason this is not a method — a registry that could be
    /// added to after requests start being served would need a lock around
    /// every `get`, and the first version of this reached for a raw pointer to
    /// avoid one. Building it first makes "the registry never changes" a fact
    /// about the type rather than a claim in a comment.
    pub async fn new(out: UnboundedSender<String>) -> Self {
        let store = Store::open().map_err(|e| e.to_string());
        let mut registry = Registry::new();
        let external = load_external_drivers(&store, &mut registry, &out).await;
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
                registry,
                conns: Mutex::new(HashMap::new()),
                calls: Mutex::new(HashMap::new()),
                next_conn: AtomicU64::new(1),
                next_call: AtomicU64::new(1),
                out,
                store,
                external,
                audit_mode: audit::Mode::from_env(),
                next_run: AtomicU64::new(1),
                runs: Mutex::new(HashMap::new()),
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

    /// Put one line on the output channel from outside. The MCP transport uses
    /// it, so both protocols leave through the same single writer and cannot
    /// interleave.
    pub fn send_line(&self, line: String) {
        self.send(line);
    }

    /// What a live connection is allowed to do.
    fn conn_policy(&self, id: u64) -> anyhow::Result<Policy> {
        self.inner
            .conns
            .lock()
            .unwrap()
            .get(&id)
            .map(|e| e.policy)
            .ok_or_else(|| anyhow::anyhow!("no such connection {id}"))
    }

    /// Start an audit record for a statement on a live connection: the
    /// connection's name, driver and policy as they are RIGHT NOW, the shape of
    /// the statement, and the statement itself through whatever `Mode` the
    /// engine was started with.
    ///
    /// Built here rather than at each call site so none of the doors can produce
    /// a half-filled row, and so the only place a raw statement turns into a
    /// stored one is this function.
    fn audit_for(&self, conn_id: u64, path: &str, statement: &str, confirmed: bool, outcome: &str) -> audit::Record {
        let (name, driver, policy) = self
            .inner
            .conns
            .lock()
            .unwrap()
            .get(&conn_id)
            .map(|e| (e.name.clone(), Some(e.driver.clone()), Some(e.policy.label().to_string())))
            .unwrap_or((None, None, None));
        let mut r = audit::Record::new(path, outcome);
        r.conn = name;
        r.driver = driver;
        r.policy = policy;
        r.confirmed = confirmed;
        // `about` fingerprints the ORIGINAL text — so `shape` and `off` modes
        // still group runs of the same statement — renders the column through
        // the mode, and keeps the credentials it removed so `because` can take
        // them out of the engine's error message too.
        r.about(self.inner.audit_mode, statement);
        r
    }

    /// Append one audit row.
    ///
    /// Never fails the operation it is describing: a failure to LOG is reported
    /// and does not become a failure to work, because refusing to run something
    /// on the grounds that the record could not be written is a worse outcome
    /// than a gap in the record that says, loudly, that it is a gap. The
    /// complaint goes out on `daemon.log` at `error` level, which is the channel
    /// both clients already show.
    async fn audit(&self, r: audit::Record) {
        let store = match self.inner.store.as_ref() {
            Ok(s) => s,
            // No store means no log, and `store.status` already says why. The
            // engine warned about that once at startup; warning again per
            // statement would bury the one message that mattered.
            Err(_) => return,
        };
        if let Err(e) = store.record_audit(r).await {
            self.send(rpc::notification(
                "daemon.log",
                json!({
                    "level": "error",
                    "message": format!("the audit record could not be written: {e} — this operation is NOT in the log"),
                }),
            ));
        }
    }

    /// Hold a statement against the connection\u{2019}s policy.
    ///
    /// Returns the reason to refuse, or `None` to go ahead. This runs on EVERY
    /// path that can change data — the console, a tab, the builder, a row
    /// edit — because a check that lives on one path is a check that the next
    /// path forgets.
    fn allowed(
        &self,
        conn_id: u64,
        driver: &dyn crate::driver::Driver,
        statement: &str,
        confirmed: bool,
    ) -> anyhow::Result<()> {
        let policy = self.conn_policy(conn_id)?;
        if policy == Policy::Open {
            return Ok(());
        }
        if !driver.writes(statement) {
            return Ok(());
        }
        if policy == Policy::ReadOnly {
            anyhow::bail!(
                "this connection is read only \u{2014} it refuses anything that changes the \
                 database, including statements that are not dangerous"
            );
        }
        // Policy::Confirm: only what classify NAMES has to be confirmed. Asking
        // about every INSERT would make the question a reflex, and a reflex is
        // not a decision.
        if let Some(reason) = driver.classify(statement) {
            if !confirmed {
                anyhow::bail!("needs confirming: {reason}");
            }
        }
        Ok(())
    }

    /// Hold a statement against the policy for a caller that is NOT A PERSON.
    ///
    /// The same `classify`, the same `writes`, the same connection policy —
    /// this calls `allowed` and then takes things AWAY. There is no second,
    /// softer path, because a second path is the one that gets used.
    ///
    /// Two differences, and both only ever restrict:
    ///
    /// 1. A `reader` role refuses every write, whatever the connection allows.
    ///    The role is a floor, not a key.
    /// 2. `Policy::Open` is treated as `Confirm`. "Unrestricted" is a decision
    ///    a person made ABOUT THEMSELVES — it means "stop asking me", not "let
    ///    anything that can speak JSON-RPC drop a table". A permission granted
    ///    to a person is not inherited by a machine acting in their name.
    ///
    /// `confirmed` is never true because an agent said so. It is true only when
    /// a person decided it, out of band, and the approval was spent.
    fn allowed_for_agent(
        &self,
        conn_id: u64,
        driver: &dyn crate::driver::Driver,
        statement: &str,
        role: AgentRole,
        confirmed: bool,
    ) -> anyhow::Result<()> {
        let writes = driver.writes(statement);
        if role == AgentRole::Reader && writes {
            anyhow::bail!(
                "this MCP session is a reader \u{2014} it refuses anything that changes the \
                 database, whatever the connection itself allows"
            );
        }
        // The connection's own rule first, so a read-only connection refuses in
        // its own words rather than in the agent's.
        self.allowed(conn_id, driver, statement, confirmed)?;
        // And then the part `allowed` lets through for a person: on an
        // unrestricted connection it returns Ok before classify is ever called.
        if writes && self.conn_policy(conn_id)? == Policy::Open {
            if let Some(reason) = driver.classify(statement) {
                if !confirmed {
                    anyhow::bail!("needs confirming: {reason}");
                }
            }
        }
        Ok(())
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

    /// Reach the ordinary dispatch from another transport.
    ///
    /// The MCP server uses this, which is the point: it goes through the SAME
    /// methods a client uses, so there is nothing for a second implementation
    /// to get wrong or to be given more of. `query.execute` is deliberately NOT
    /// reachable this way — see `agent_query`.
    pub async fn dispatch_public(&self, method: &str, params: Json) -> anyhow::Result<Json> {
        self.dispatch(method, params).await
    }

    /// Run a statement for something that is NOT A PERSON, and wait for it.
    ///
    /// Synchronous, unlike `query.execute`: a language model has no event loop
    /// to be told later in, and an answer that arrives as a notification is an
    /// answer it will never read.
    ///
    /// The whole approval dance lives here:
    ///
    ///   * `allowed_for_agent` holds it against the role AND the connection.
    ///   * A refusal that is not "needs confirming" is final, and said.
    ///   * "Needs confirming" FILES an approval for a person and returns what
    ///     was filed. It does not run. Asking again before they answer files
    ///     nothing new — an agent retrying in a loop must not fill somebody's
    ///     queue with one question.
    ///   * Once a person has approved it, the next attempt SPENDS that approval
    ///     and runs. Spent, because they said yes to a DELETE, not to every
    ///     DELETE from now on.
    pub async fn agent_query(
        &self,
        conn_id: u64,
        statement: &str,
        role: AgentRole,
        limit: usize,
    ) -> anyhow::Result<String> {
        let kind = self.conn_driver(conn_id)?;
        let driver = self.inner.registry.get(&kind)?;
        let conn_name = self
            .inner
            .conns
            .lock()
            .unwrap()
            .get(&conn_id)
            .and_then(|e| e.name.clone());

        // Is there already a person's YES for exactly this?
        let approved = match self.store() {
            Ok(store) => {
                let pending = store.list_approvals(Some("approved".into()), 200).await.unwrap_or_default();
                pending.iter().find_map(|a| {
                    let same = a.get("statement").and_then(|x| x.as_str()) == Some(statement)
                        && a.get("conn").and_then(|x| x.as_str()) == conn_name.as_deref();
                    if same { a.get("id").and_then(|x| x.as_i64()) } else { None }
                })
            }
            Err(_) => None,
        };

        match self.allowed_for_agent(conn_id, driver.as_ref(), statement, role, approved.is_some()) {
            Ok(()) => {}
            Err(e) => {
                let why = e.to_string();
                let mut r = self.audit_for(conn_id, "mcp.query", statement, false, "refused");
                r.because(&why);
                self.audit(r).await;
                if let Some(reason) = why.strip_prefix("needs confirming: ") {
                    // Filed, not run. The agent is told exactly what is waiting
                    // on whom.
                    let store = self.store()?;
                    let (id, state) = store
                        .ask_approval(
                            "mcp".into(),
                            conn_name.clone(),
                            statement.to_string(),
                            audit::fingerprint(statement),
                            Some(reason.to_string()),
                        )
                        .await?;
                    if state == "denied" {
                        // A denial STANDS. Filing a fresh ask after one would
                        // mean "no" lasted until the next attempt, and the
                        // person would answer the same question for ever.
                        anyhow::bail!(
                            "NOT RUN. {reason}\n\nA person has already refused this exact \
                             statement (approval #{id}). Asking again will not change that. \
                             If it should be allowed, they have to say so themselves."
                        );
                    }
                    anyhow::bail!(
                        "NOT RUN. {reason}\n\nThis needs a person. It has been filed as approval \
                         #{id} ({state}) and is waiting in their client. Asking again before they \
                         answer files nothing new. If they approve it, run exactly the same \
                         statement again and it will go through once."
                    );
                }
                return Err(e);
            }
        }

        // Spend the approval BEFORE running. If the run then fails, the
        // approval is still gone — which is right: they approved an attempt,
        // and a second attempt is a second decision.
        if let Some(id) = approved {
            let store = self.store()?;
            if !store.spend_approval(id, conn_name.clone(), statement.to_string()).await? {
                anyhow::bail!("that approval is no longer valid — ask again");
            }
        }

        let conn = self.conn(conn_id)?;
        let started = Instant::now();
        let mut guard = conn.lock().await;
        let outcome = guard.execute(statement).await;
        drop(guard);
        let ms = started.elapsed().as_millis() as i64;

        let writes = driver.writes(statement);
        match outcome {
            Err(e) => {
                if writes {
                    let mut r = self.audit_for(conn_id, "mcp.query", statement, approved.is_some(), "failed");
                    r.ms = Some(ms);
                    r.because(e.to_string());
                    self.audit(r).await;
                }
                Err(e)
            }
            Ok(mut stream) => {
                let cols: Vec<String> = stream.columns().iter().map(|c| c.name.clone()).collect();
                let rows: Vec<Vec<Value>> = stream.next_page(limit).await.ok().flatten().unwrap_or_default();
                let affected = stream.affected();
                if writes {
                    let mut r = self.audit_for(conn_id, "mcp.query", statement, approved.is_some(), "applied");
                    r.ms = Some(ms);
                    r.affected = affected.map(|a| a as i64);
                    self.audit(r).await;
                }
                // Tab-separated, with a count. A model reads this; JSON of the
                // same thing is twice the tokens for the same table.
                let mut out = String::new();
                if !cols.is_empty() {
                    out.push_str(&cols.join("\t"));
                    out.push('\n');
                }
                for row in &rows {
                    let cells: Vec<String> = row.iter().map(agent_cell).collect();
                    out.push_str(&cells.join("\t"));
                    out.push('\n');
                }
                out.push_str(&format!(
                    "\n({} row(s) shown{}, {ms} ms)",
                    rows.len(),
                    affected.map(|a| format!(", {a} affected")).unwrap_or_default()
                ));
                Ok(out)
            }
        }
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
            "instance.metrics" => self.metrics(params).await,
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
            "store.audit.list" => self.store_audit_list(params).await,
            "store.charts.list" => self.store_chart_list(params).await,
            "store.charts.save" => self.store_chart_save(params).await,
            "store.charts.remove" => self.store_chart_remove(params).await,
            "store.dashboards.list" => self.store_dashboard_list(params).await,
            "store.dashboards.get" => self.store_dashboard_get(params).await,
            "store.dashboards.save" => self.store_dashboard_save(params).await,
            "store.dashboards.remove" => self.store_dashboard_remove(params).await,
            "store.profiles.list" => self.store_profile_list(params).await,
            "store.profiles.get" => self.store_profile_get(params).await,
            "store.profiles.save" => self.store_profile_save(params).await,
            "store.profiles.remove" => self.store_profile_remove(params).await,
            "approvals.list" => self.approval_list(params).await,
            "approvals.decide" => self.approval_decide(params).await,
            "access.list" => self.access_list(params).await,
            "access.test" => self.access_test(params).await,
            "access.import" => self.access_import(params).await,
            "store.scripts.list" => self.store_script_list(params).await,
            "store.scripts.save" => self.store_script_save(params).await,
            "store.scripts.remove" => self.store_script_remove(params).await,
            "script.run" => self.script_run(params).await,
            "script.cancel" => self.script_cancel(params).await,
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
            // Which driver PROGRAMS were found and what became of each. A
            // driver that is installed and not in `drivers` has its reason
            // here; without it, an absent driver is a mystery.
            "external": self.inner.external,
        }))
    }

    // ── connections ──────────────────────────────────────────────────────────

    async fn connect(&self, params: Json) -> anyhow::Result<Json> {
        let mut spec: ConnSpec = serde_json::from_value(params)?;
        // A named tunnel or proxy becomes the real thing BEFORE anything is
        // dialled. A missing profile fails the connect; it never silently
        // becomes a direct link.
        self.resolve_profiles(&mut spec).await?;
        let driver = self.inner.registry.get(&spec.driver)?;
        let meta = driver.meta();
        // WHAT this connection points at, in words with no credential in them
        // (see `ConnSpec::target`). Computed before the dial so the FAILED
        // attempt can be logged too — a run of refused connections to a host
        // is the shape of something worth seeing, and only the successes being
        // in the log is how it stays invisible.
        let target = spec.target(meta);
        // BEFORE the dial. A PreConnect hook that fails stops the connection,
        // which is the only reason to have one before rather than after.
        self.run_hooks(&spec, spec.name.as_deref(), crate::spec::HookEvent::PreConnect)
            .await?;
        let net = NetContext::with_proxy(spec.tunnel.clone(), spec.proxy.clone());
        let started = Instant::now();
        let conn = match driver.connect(&spec, net.clone()).await {
            Ok(c) => c,
            Err(e) => {
                let mut r = audit::Record::new("conn.connect", "failed");
                r.conn = spec.name.clone();
                r.driver = Some(spec.driver.clone());
                r.policy = Some(spec.policy.label().to_string());
                // The target is not a statement and has no shape worth
                // fingerprinting; it goes in whole, in every mode, because it
                // holds nothing a mode is there to withhold.
                r.statement = target.clone();
                r.ms = Some(started.elapsed().as_millis() as i64);
                // The spec's own credential values come out of the message
                // before it is written OR returned. A driver's failure is
                // assembled by somebody else — an SSH library, a database — and
                // quotes back whatever it choked on.
                let guard = spec.sensitive();
                r.guarding(guard.clone());
                r.because(e.to_string());
                self.audit(r).await;
                // The CLIENT is told the same scrubbed thing. A log that is
                // clean while the window shows the value in a toast is not
                // clean; it is one place fixed out of two.
                return Err(anyhow::anyhow!("{}", audit::scrub(&e.to_string(), &guard)));
            }
        };
        // Encryption status: native TLS negotiated OR the link rides an SSH tunnel.
        let encrypted = conn.encrypted() || net.tunneled();
        let tunneled = net.tunneled();
        let id = self.inner.next_conn.fetch_add(1, Ordering::Relaxed);
        self.inner.conns.lock().unwrap().insert(
            id,
            ConnEntry {
                conn: Arc::new(AsyncMutex::new(conn)),
                spec: spec.clone(),
                name: spec.name.clone(),
                policy: spec.policy,
                driver: spec.driver.clone(),
                _net: net,
            },
        );
        let mut r = audit::Record::new("conn.connect", "connected");
        r.conn = spec.name.clone();
        r.driver = Some(spec.driver.clone());
        r.policy = Some(spec.policy.label().to_string());
        // Whether the link ended up encrypted belongs in the record: it is a
        // fact about this session that cannot be read back off the spec, since
        // `prefer` can go either way.
        r.statement = format!("{target} encrypted={encrypted}");
        r.ms = Some(started.elapsed().as_millis() as i64);
        self.audit(r).await;
        // AFTER it is open, and a failure here does NOT take it down again: the
        // connection exists, somebody asked for it, and closing it because a
        // notification script could not run would be the hook deciding
        // something that is not its decision.
        let _ = self
            .run_hooks(&spec, spec.name.as_deref(), crate::spec::HookEvent::PostConnect)
            .await;
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
        let mut p: P = serde_json::from_value(params)?;
        // The same resolution the real connect does, so a stage tests the spec
        // that would actually be used rather than the one that was typed.
        self.resolve_profiles(&mut p.spec).await?;
        let meta = self
            .inner
            .registry
            .metas()
            .into_iter()
            .find(|m| m.kind == p.spec.driver)
            .ok_or_else(|| anyhow::anyhow!("unknown driver '{}'", p.spec.driver))?;

        let started = Instant::now();
        // Every stage goes through one place that takes the spec's own
        // credential values out of whatever comes back. A test dials the same
        // things a connect does and fails in the same words — including an SSH
        // library quoting the key path it could not open.
        let guard = p.spec.sensitive();
        let outcome = match p.stage.as_str() {
            "proxy" => Self::test_proxy(&p.spec).await,
            "endpoint" => Self::test_endpoint(&p.spec, meta).await,
            "tunnel" => Self::test_tunnel(&p.spec, meta).await,
            "tls" | "auth" => {
                // These two dial the whole way, so they ride the tunnel. When
                // the tunnel is what is broken, the connect fails with the
                // tunnel's words — `cannot load ssh key …` under a button
                // labelled "Test auth" — and nothing says the fault is a
                // different section of the form.
                //
                // So the earlier stage is checked FIRST and named when it is
                // the one that failed. A later stage cannot pass if an earlier
                // one does not, and saying which one fell over is the whole
                // point of having stages.
                if p.spec.tunnel.is_some() {
                    if let Err(e) = Self::test_tunnel(&p.spec, meta).await {
                        Err(anyhow::anyhow!("the TUNNEL stage failed first, so {} was not reached: {e}", p.stage))
                    } else {
                        self.test_connect(&p.spec, &p.stage).await
                    }
                } else {
                    self.test_connect(&p.spec, &p.stage).await
                }
            }
            other => Err(anyhow::anyhow!("unknown test stage '{other}'")),
        };
        let detail = match outcome {
            Ok(d) => audit::scrub(&d, &guard),
            Err(e) => return Err(anyhow::anyhow!("{}", audit::scrub(&e.to_string(), &guard))),
        };
        Ok(json!({
            "ok": true,
            "ms": started.elapsed().as_millis() as u64,
            "detail": detail,
        }))
    }

    /// Proxy stage — is the proxy there, and does it speak what it was told to?
    ///
    /// Deliberately dials the proxy and asks it for a KNOWN-UNREACHABLE
    /// endpoint rather than for the database. The question of this stage is
    /// "does the proxy work", and asking it for the database would fold two
    /// answers into one: a refusal would then mean either a broken proxy or a
    /// database that is down, which is the blame-sharing the staged tests exist
    /// to prevent. A proxy that answers "host unreachable" has WORKED — it
    /// completed its handshake and gave a verdict.
    async fn test_proxy(spec: &ConnSpec) -> anyhow::Result<String> {
        let Some(p) = &spec.proxy else {
            return Err(anyhow::anyhow!("no proxy is configured on this connection"));
        };
        // RFC 6761 reserves .invalid, and it never resolves anywhere.
        match crate::proxy::dial(p, "lode-proxy-probe.invalid", 1).await {
            // It connected to a name that cannot exist. That is not a working
            // proxy, it is a proxy that lies, and it is worth saying so.
            Ok(_) => Ok(format!(
                "the {} proxy at {}:{} answered — but it CONNECTED to a name that cannot exist, \
                 so whatever it is doing, it is not resolving names honestly",
                p.kind.label(),
                p.host,
                p.port
            )),
            Err(e) => {
                let text = e.to_string();
                // The handshake got as far as a verdict: the proxy works.
                if text.contains("refused to connect") || text.contains("the proxy wants authentication") {
                    Ok(format!(
                        "the {} proxy at {}:{} is reachable and answered a request ({text})",
                        p.kind.label(),
                        p.host,
                        p.port
                    ))
                } else {
                    Err(e)
                }
            }
        }
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
            let path = spec.param_path(fp.key)?;
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
        let net = NetContext::with_proxy(spec.tunnel.clone(), spec.proxy.clone());
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
        let net = NetContext::with_proxy(Some(t), spec.proxy.clone());
        let addr = net.resolve(host, port).await?;
        Ok(format!(
            "ssh {user}@{shost}:{sport} authenticated over {} — forwarding {addr} → {host}:{port}",
            net.path()
        ))
    }

    /// TLS / auth stages — a REAL driver connect (tunnel, TLS handshake and
    /// credentials all exercised end to end), closed again immediately. The two
    /// stages differ only in what they report: the encryption posture, or the
    /// accepted identity.
    async fn test_connect(&self, spec: &ConnSpec, stage: &str) -> anyhow::Result<String> {
        let driver = self.inner.registry.get(&spec.driver)?;
        let net = NetContext::with_proxy(spec.tunnel.clone(), spec.proxy.clone());
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
        // Before anything is taken down, while the link is still there — which
        // is what makes a PreDisconnect hook able to use it at all.
        // Cloned out of the map and the guard DROPPED before any await: a
        // std MutexGuard held across one makes the whole dispatch future
        // non-Send, and the compiler is right that it would also be a lock held
        // for as long as somebody's hook takes to run.
        let pre = self
            .inner
            .conns
            .lock()
            .unwrap()
            .get(&p.conn_id)
            .map(|e| e.spec.clone());
        if let Some(spec) = pre {
            let name = spec.name.clone();
            let _ = self
                .run_hooks(&spec, name.as_deref(), crate::spec::HookEvent::PreDisconnect)
                .await;
        }
        let entry = self.inner.conns.lock().unwrap().remove(&p.conn_id);
        if let Some(entry) = entry {
            let mut r = audit::Record::new("conn.disconnect", "disconnected");
            r.conn = entry.name.clone();
            r.driver = Some(entry.driver.clone());
            r.policy = Some(entry.policy.label().to_string());
            self.audit(r).await;
            // Reclaim the Box out of the Arc<Mutex> to call the by-value close().
            // Dropping `entry._net` afterwards tears down any SSH tunnel.
            if let Ok(mutex) = Arc::try_unwrap(entry.conn) {
                let boxed = mutex.into_inner();
                let _ = boxed.close().await;
            }
            let name = entry.spec.name.clone();
            let _ = self
                .run_hooks(&entry.spec, name.as_deref(), crate::spec::HookEvent::PostDisconnect)
                .await;
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
            /// The caller has asked about this statement and been told yes.
            #[serde(default)]
            confirm: bool,
        }
        let p: P = match serde_json::from_value(params) {
            Ok(p) => p,
            Err(e) => return self.send(rpc::response_err(id, &e.to_string())),
        };
        let conn = match self.conn(p.conn_id) {
            Ok(c) => c,
            Err(e) => return self.send(rpc::response_err(id, &e.to_string())),
        };
        // The policy is held HERE, not in whatever is calling. A client that
        // skips its own confirmation dialog still cannot get a dangerous
        // statement past this.
        // WHETHER THIS CHANGES THE DATABASE is what decides whether it is
        // audited. The same question `Policy::ReadOnly` asks, and the same
        // answer: a SELECT has no business in a log of changes, and an unknown
        // verb counts as a write (see `Driver::writes`), because an unlogged
        // change is worse than a logged read.
        let writes = self
            .conn_driver(p.conn_id)
            .and_then(|k| self.inner.registry.get(&k))
            .map(|d| d.writes(&p.statement))
            .unwrap_or(false);
        match self
            .conn_driver(p.conn_id)
            .and_then(|k| self.inner.registry.get(&k))
            .and_then(|d| self.allowed(p.conn_id, d.as_ref(), &p.statement, p.confirm))
        {
            Ok(()) => {}
            Err(e) => {
                // A REFUSED write is the most important thing in the log: it is
                // the record of what somebody TRIED, which is the question
                // asked after something goes wrong.
                if writes {
                    let mut r = self.audit_for(p.conn_id, "execute", &p.statement, p.confirm, "refused");
                    r.because(e.to_string());
                    self.audit(r).await;
                }
                return self.send(rpc::response_err(id, &e.to_string()));
            }
        }
        // The row for a statement that was ALLOWED is written when it is OVER,
        // from the task below. Writing it here would be a log of what the engine
        // set out to do: "applied" stamped before the statement had reached the
        // database, with no way to tell apart the ones that failed on the way.
        let pending = if writes {
            Some(self.audit_for(p.conn_id, "execute", &p.statement, p.confirm, "applied"))
        } else {
            None
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
                drop(c);
                // A cancelled write still reached the database and may have
                // changed something before it was stopped — "cancelled" is the
                // honest outcome, and either "applied" or "refused" would be a
                // guess about data nobody has read back.
                if let Some(mut r) = pending {
                    r.outcome = "cancelled".into();
                    r.ms = Some(ms as i64);
                    server.audit(r).await;
                }
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
                    drop(c);
                    if let Some(mut r) = pending {
                        r.affected = affected.map(|a| a as i64);
                        r.ms = Some(ms as i64);
                        server.audit(r).await;
                    }
                }
                Err(e) => {
                    c.status = CallStatus::Failed;
                    c.error = Some(e.to_string());
                    server.notify_state(call_id, CallStatus::Failed, ms, Some(e.to_string()), None);
                    drop(c);
                    // A write that was allowed and then FAILED belongs in the log
                    // as much as one that went through: it is the record of an
                    // attempt, and the reason it failed is the useful half.
                    if let Some(mut r) = pending {
                        r.outcome = "failed".into();
                        r.ms = Some(ms as i64);
                        r.because(e.to_string());
                        server.audit(r).await;
                    }
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
        // A read-only connection refuses here as well, and before the plan is
        // ever offered — the same rule, reached by a different door.
        if let Err(e) = self.allowed(p.conn_id, driver.as_ref(), &statement, p.confirm) {
            let mut r = self.audit_for(p.conn_id, "row.write", &statement, p.confirm, "refused");
            r.because(e.to_string());
            self.audit(r).await;
            return Err(e);
        }
        let reason = driver.classify(&statement);
        if !p.confirm {
            return Ok(json!({
                "applied": false,
                "statement": statement,
                "dangerous": reason.is_some(),
                "reason": reason,
            }));
        }
        let started = Instant::now();
        let out = match conn.lock().await.write_row(&p.plan).await {
            Ok(o) => o,
            Err(e) => {
                // A write that was ALLOWED and then failed — the key matched
                // three rows, the column refused the value — is as much part of
                // the record as one that went through.
                let mut r = self.audit_for(p.conn_id, "row.write", &statement, p.confirm, "failed");
                r.ms = Some(started.elapsed().as_millis() as i64);
                r.because(e.to_string());
                self.audit(r).await;
                return Err(e);
            }
        };
        // `out.statement` is the driver's own parameterised statement, so what
        // lands in the log is `… SET name = ?1 WHERE id = ?2` — the shape of the
        // write without the values written. That is the driver's doing, not the
        // redactor's, and it is why a row edit is the one path whose log line
        // carries nothing to redact.
        let mut r = self.audit_for(p.conn_id, "row.write", &out.statement, p.confirm, "applied");
        r.affected = Some(out.affected as i64);
        r.ms = Some(started.elapsed().as_millis() as i64);
        self.audit(r).await;
        Ok(json!({
            "applied": true,
            "statement": out.statement,
            "affected": out.affected,
            "dangerous": reason.is_some(),
            "reason": reason,
        }))
    }

    /// What the server on the other end says about itself.
    ///
    /// Each driver asks its OWN system views — `pg_stat_*`, `SHOW STATUS`,
    /// `INFO`, a handful of PRAGMAs — because what is available and what it is
    /// called differ completely between engines, and a client assembling this
    /// would be a list of driver names in a client.
    ///
    /// Every call is one round trip, made only when asked for. There is no timer
    /// in here: repeating this against somebody\u{2019}s production database is a
    /// decision for the person looking at it, not a default.
    async fn metrics(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn_id: u64,
        }
        let p: P = serde_json::from_value(params)?;
        let kind = self.conn_driver(p.conn_id)?;
        let driver = self.inner.registry.get(&kind)?;
        if !driver.meta().caps.metrics {
            anyhow::bail!("the {kind} driver has nothing to report about its server");
        }
        let conn = self.conn(p.conn_id)?;
        let metrics = conn.lock().await.metrics().await?;
        Ok(json!({ "metrics": metrics }))
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

    /// The audit log, newest first. `conn` narrows it to one connection by name.
    ///
    /// There is no matching remove: a log a client can delete from is a log of
    /// what that client chose to keep. `mode` comes back with the rows so a
    /// client can SAY why a statement column is empty — a blank statement with
    /// no explanation reads as a bug, and this one is a setting.
    async fn store_audit_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            limit: Option<i64>,
            #[serde(default)]
            conn: Option<String>,
        }
        let p: P = serde_json::from_value(params)?;
        let audit = self.store()?.list_audit(p.limit.unwrap_or(200), p.conn).await?;
        Ok(json!({ "audit": audit, "mode": self.inner.audit_mode.label() }))
    }

    // ── charts ───────────────────────────────────────────────────────────────

    async fn store_chart_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "charts": self.store()?.list_charts(p.conn).await? }))
    }

    async fn store_chart_save(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
            sql: String,
            #[serde(default)]
            measure: String,
        }
        let p: P = serde_json::from_value(params)?;
        if p.name.trim().is_empty() {
            return Err(anyhow::anyhow!("a saved chart needs a name"));
        }
        self.store()?.save_chart(p.conn, p.name, p.sql, p.measure).await?;
        Ok(json!({ "saved": true }))
    }

    // ── approvals ────────────────────────────────────────────────────────────

    /// What something that is not a person has asked a person to allow.
    ///
    /// The human-facing half of the MCP approval flow: the agent files, the
    /// person reads this in their own client and answers.
    async fn approval_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            state: Option<String>,
            #[serde(default)]
            limit: Option<i64>,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({
            "approvals": self.store()?.list_approvals(p.state, p.limit.unwrap_or(100)).await?
        }))
    }

    /// A person's answer, and it goes in the audit log.
    ///
    /// The decision is as much a fact about what happened as the statement is:
    /// "who let this through" is the question asked immediately after "what
    /// ran", and a log that holds the second without the first answers half.
    async fn approval_decide(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            id: i64,
            approve: bool,
        }
        let p: P = serde_json::from_value(params)?;
        let store = self.store()?;
        let before = store.list_approvals(None, 500).await?;
        let row = before
            .iter()
            .find(|a| a.get("id").and_then(|x| x.as_i64()) == Some(p.id))
            .ok_or_else(|| anyhow::anyhow!("there is no approval #{}", p.id))?
            .clone();
        let decided = store.decide_approval(p.id, p.approve).await?;
        if decided {
            let mut r = audit::Record::new("approval", if p.approve { "approved" } else { "denied" });
            r.conn = row.get("conn").and_then(|x| x.as_str()).map(str::to_string);
            r.confirmed = p.approve;
            r.about(
                self.inner.audit_mode,
                row.get("statement").and_then(|x| x.as_str()).unwrap_or(""),
            );
            r.because(format!(
                "a person {} approval #{} asked for by {}",
                if p.approve { "allowed" } else { "refused" },
                p.id,
                row.get("asker").and_then(|x| x.as_str()).unwrap_or("something")
            ));
            self.audit(r).await;
        }
        Ok(json!({ "decided": decided }))
    }

    // ── access providers ─────────────────────────────────────────────────────

    /// Everything `~/.aws/config` and `~/.aws/credentials` describe.
    ///
    /// Each entry says WHAT IT IS (`static`, `process`, `sso`, `assume_role`,
    /// `inherited`) and whether this engine can use it, so a client can show the
    /// whole list and mark the ones that will not work — rather than offering
    /// eight profiles of which three fail only when somebody picks them.
    ///
    /// No credential is read here. Listing is reading section names.
    async fn access_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            config: Option<String>,
            #[serde(default)]
            credentials: Option<String>,
        }
        let p: P = serde_json::from_value(params)?;
        let (cfg, creds) = Self::access_paths(&p.config, &p.credentials);
        let found = crate::access::profiles_from(cfg.as_deref(), creds.as_deref());
        let rows: Vec<Json> = found
            .iter()
            .map(|x| {
                json!({
                    "name": x.name,
                    "source": x.source,
                    "kind": x.kind(),
                    "usable": x.usable(),
                    "region": x.get("region"),
                })
            })
            .collect();
        Ok(json!({
            "profiles": rows,
            // Where it looked, so an empty list is a fact rather than a mystery.
            "config": cfg.map(|p| p.display().to_string()),
            "credentials": creds.map(|p| p.display().to_string()),
        }))
    }

    fn access_paths(
        config: &Option<String>,
        credentials: &Option<String>,
    ) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
        (
            config
                .as_ref()
                .map(|s| std::path::PathBuf::from(crate::spec::expand_tilde(s)))
                .or_else(crate::access::config_path),
            credentials
                .as_ref()
                .map(|s| std::path::PathBuf::from(crate::spec::expand_tilde(s)))
                .or_else(crate::access::credentials_path),
        )
    }

    /// Resolve one profile and say WHETHER it worked — never what it produced.
    ///
    /// The answer is the access key id's last four characters and nothing else.
    /// Enough to tell two profiles apart, not enough to be a credential; the
    /// whole id is half of one, and a test button has no business handing it
    /// back to be screenshotted.
    async fn access_test(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            profile: String,
            #[serde(default)]
            config: Option<String>,
            #[serde(default)]
            credentials: Option<String>,
        }
        let p: P = serde_json::from_value(params)?;
        let (cfg, creds) = Self::access_paths(&p.config, &p.credentials);
        let found = crate::access::profiles_from(cfg.as_deref(), creds.as_deref());
        let c = crate::access::resolve(&found, &p.profile, 0).await?;
        let tail: String = c.access_key_id.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
        Ok(json!({
            "ok": true,
            "key_ends_with": tail,
            "has_session_token": c.session_token.is_some(),
            "region": c.region,
        }))
    }

    /// Save an AWS profile as a lode access profile, BY NAME.
    ///
    /// By name, not by value: the credential stays in the AWS files, where the
    /// AWS tools rotate it. Copying it here would mean a key that works today
    /// and a connection that fails next month for a reason nobody connects to
    /// the rotation.
    async fn access_import(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            /// The AWS profile's name, and the lode profile's unless `as_name`
            /// says otherwise.
            profile: String,
            #[serde(default)]
            as_name: Option<String>,
            #[serde(default)]
            config: Option<String>,
            #[serde(default)]
            credentials: Option<String>,
        }
        let p: P = serde_json::from_value(params)?;
        let (cfg, creds) = Self::access_paths(&p.config, &p.credentials);
        let found = crate::access::profiles_from(cfg.as_deref(), creds.as_deref());
        let aws = found
            .iter()
            .find(|x| x.name == p.profile)
            .ok_or_else(|| anyhow::anyhow!("there is no AWS profile called '{}'", p.profile))?;
        // Refused on the way IN, with what it is. Importing a profile that
        // cannot produce credentials would make a lode profile that fails at
        // every connect for a reason stored somewhere else.
        if !aws.usable() {
            anyhow::bail!(
                "the AWS profile '{}' is a '{}' profile and this engine cannot get credentials from it —                  importing it would make something that fails at every connect",
                p.profile,
                aws.kind()
            );
        }
        let name = p.as_name.unwrap_or_else(|| p.profile.clone());
        let spec = json!({ "provider": "aws", "profile": p.profile, "region": aws.get("region") });
        self.store()?
            .save_profile("access".into(), name.clone(), serde_json::to_string(&spec)?)
            .await?;
        Ok(json!({ "saved": true, "name": name, "kind": aws.kind() }))
    }

    // ── scripts and hooks ────────────────────────────────────────────────────

    /// What a hook or a script is told about the connection it runs around.
    ///
    /// CONTEXT and never a credential. A hook that needs a password fetches it
    /// the same way the engine does, from the keyring, under its own name.
    /// Putting a resolved secret in here would publish it to `ps`, to
    /// `/proc/<pid>/environ`, and to everything the child goes on to start —
    /// which is why `auth`, `tls` and the tunnel's own credentials are not read
    /// at all when this is built, rather than read and then filtered.
    fn hook_env(spec: &ConnSpec, name: Option<&str>, event: &str) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("LODE_EVENT".into(), event.to_string());
        env.insert("LODE_DRIVER".into(), spec.driver.clone());
        if let Some(n) = name {
            env.insert("LODE_CONN".into(), n.to_string());
        }
        // Only the three params every driver spells the same way, and only
        // because the engine already reads them generically elsewhere. The rest
        // of `params` is driver-interpreted free text, and copying all of it
        // into an environment is how something nobody meant to export gets
        // exported.
        for key in ["host", "port", "database"] {
            if let Some(v) = spec.param_opt(key) {
                env.insert(format!("LODE_{}", key.to_uppercase()), v.to_string());
            }
        }
        env
    }

    /// Run the hooks for one moment.
    ///
    /// Every one is audited, whatever it is and whether it worked — a hook is
    /// somebody's own code running against their database on the engine's
    /// initiative, and there is no reading of "what happened here" that leaves
    /// that out. A PreConnect failure is returned as an error, which stops the
    /// connection; the other three are reported and do not.
    async fn run_hooks(&self, spec: &ConnSpec, name: Option<&str>, event: crate::spec::HookEvent) -> anyhow::Result<()> {
        let hooks: Vec<_> = spec.hooks.iter().filter(|h| h.on == event).collect();
        if hooks.is_empty() {
            return Ok(());
        }
        let env = Self::hook_env(spec, name, event.label());
        for h in hooks {
            // Preparing a hook can fail too — a named script that is gone, an
            // inline one with no language. That goes down the SAME path as a
            // hook that ran and failed, so a PreConnect hook which could not
            // even be assembled stops the connection and says so in the same
            // words. The first version let it out through `?`, and the message
            // named the missing script but not what had happened to the
            // connection.
            let prepared: anyhow::Result<(Option<crate::runner::Lang>, String)> = match h.kind {
                crate::spec::HookKind::Command => Ok((None, h.source.clone())),
                crate::spec::HookKind::Inline => h
                    .lang
                    .ok_or_else(|| {
                        anyhow::anyhow!("an inline {} hook must say which language it is", event.label())
                    })
                    .map(|l| (Some(l), h.source.clone())),
                crate::spec::HookKind::Script => {
                    (|| async {
                    let store = self.store()?;
                    let (lang, body) = store
                        .get_script(name.unwrap_or_default().to_string(), h.source.clone())
                        .await?
                        .ok_or_else(|| {
                            // By NAME, so the message can say which. A hook that
                            // quietly did nothing because its script was deleted
                            // is a hook that stopped protecting anything.
                            anyhow::anyhow!(
                                "the {} hook names the saved script '{}', which does not exist",
                                event.label(),
                                h.source
                            )
                        })?;
                    let lang: crate::runner::Lang = serde_json::from_value(json!(lang))
                        .map_err(|_| anyhow::anyhow!("the saved script '{}' has an unknown language", h.source))?;
                    Ok((Some(lang), body))
                    })()
                    .await
                }
            };
            let (lang, source) = match prepared {
                Ok(v) => v,
                Err(e) => {
                    let mut r = audit::Record::new(&format!("hook.{}", event.label()), "failed");
                    r.conn = name.map(str::to_string);
                    r.driver = Some(spec.driver.clone());
                    r.about(self.inner.audit_mode, &h.source);
                    r.because(e.to_string());
                    self.audit(r).await;
                    if event.blocking() {
                        return Err(anyhow::anyhow!("{e} — the connection was NOT opened"));
                    }
                    self.send(rpc::notification(
                        "daemon.log",
                        json!({ "level": "warn", "message": e.to_string() }),
                    ));
                    continue;
                }
            };
            let timeout = h
                .timeout_ms
                .map(std::time::Duration::from_millis)
                .unwrap_or(crate::runner::DEFAULT_TIMEOUT);
            let run_id = self.inner.next_run.fetch_add(1, Ordering::Relaxed);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::runner::Line>();
            let me = self.clone();
            let label = event.label().to_string();
            // The output of a hook is shown as it arrives, like a script's: a
            // PreConnect hook that is going to stop the connection should be
            // readable WHILE it runs, not only once it has failed.
            let pump = tokio::spawn(async move {
                while let Some(l) = rx.recv().await {
                    me.send(rpc::notification(
                        "script.output",
                        json!({ "run_id": run_id, "stream": l.stream, "text": l.text, "hook": label }),
                    ));
                }
            });
            let outcome = crate::runner::run(lang, &source, &env, timeout, tx).await;
            let _ = pump.await;

            let mut r = audit::Record::new(&format!("hook.{}", event.label()), "applied");
            r.conn = name.map(str::to_string);
            r.driver = Some(spec.driver.clone());
            r.policy = Some(spec.policy.label().to_string());
            // Through the same redaction as a statement: a command line is
            // where somebody writes `psql --password=…` without thinking.
            r.about(self.inner.audit_mode, &source);
            match &outcome {
                Ok(o) if o.ok() => {
                    r.ms = Some(o.ms as i64);
                    self.audit(r).await;
                }
                Ok(o) => {
                    r.outcome = "failed".into();
                    r.ms = Some(o.ms as i64);
                    r.because(o.why());
                    self.audit(r).await;
                    let why = format!("the {} hook failed: {}", event.label(), o.why());
                    if event.blocking() {
                        return Err(anyhow::anyhow!("{why} — the connection was NOT opened"));
                    }
                    self.send(rpc::notification(
                        "daemon.log",
                        json!({ "level": "warn", "message": why }),
                    ));
                }
                Err(e) => {
                    r.outcome = "failed".into();
                    r.because(e.to_string());
                    self.audit(r).await;
                    let why = format!("the {} hook could not run: {e}", event.label());
                    if event.blocking() {
                        return Err(anyhow::anyhow!("{why} — the connection was NOT opened"));
                    }
                    self.send(rpc::notification(
                        "daemon.log",
                        json!({ "level": "warn", "message": why }),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn store_script_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            conn: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "scripts": self.store()?.list_scripts(p.conn).await? }))
    }

    async fn store_script_save(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            conn: String,
            name: String,
            lang: crate::runner::Lang,
            #[serde(default)]
            body: String,
        }
        let p: P = serde_json::from_value(params)?;
        if p.name.trim().is_empty() {
            return Err(anyhow::anyhow!("a script needs a name"));
        }
        self.store()?
            .save_script(p.conn, p.name, p.lang.label().into(), p.body)
            .await?;
        Ok(json!({ "saved": true }))
    }

    async fn store_script_remove(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            conn: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "removed": self.store()?.remove_script(p.conn, p.name).await? }))
    }

    /// Run a script, answering with its id NOW and streaming its output.
    ///
    /// The same shape as `query.execute`, and for the same reason: a script that
    /// takes a minute must not hold the pipe, and its output is worth watching
    /// while it happens rather than in one lump at the end.
    async fn script_run(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            #[serde(default)]
            conn: String,
            /// A saved script by name, or…
            #[serde(default)]
            name: Option<String>,
            /// …a body typed here and now.
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            lang: Option<crate::runner::Lang>,
            #[serde(default)]
            timeout_ms: Option<u64>,
        }
        let p: P = serde_json::from_value(params)?;
        let (lang, body, what) = match (&p.name, &p.body) {
            (Some(n), None) => {
                let (lang, body) = self
                    .store()?
                    .get_script(p.conn.clone(), n.clone())
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("there is no saved script called '{n}'"))?;
                let lang: crate::runner::Lang = serde_json::from_value(json!(lang))
                    .map_err(|_| anyhow::anyhow!("the saved script '{n}' has an unknown language"))?;
                (lang, body, n.clone())
            }
            (None, Some(b)) => (
                p.lang
                    .ok_or_else(|| anyhow::anyhow!("a script typed here has to say which language it is"))?,
                b.clone(),
                "(unsaved)".to_string(),
            ),
            // Both or neither: refused rather than picked between, because
            // either choice would silently run something other than what was
            // asked for.
            _ => {
                return Err(anyhow::anyhow!(
                    "say either the NAME of a saved script or a body to run, not both and not neither"
                ))
            }
        };

        let run_id = self.inner.next_run.fetch_add(1, Ordering::Relaxed);
        let timeout = p
            .timeout_ms
            .map(std::time::Duration::from_millis)
            .unwrap_or(crate::runner::DEFAULT_TIMEOUT);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::runner::Line>();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.inner.runs.lock().unwrap().insert(run_id, cancel_tx);

        let me = self.clone();
        let conn = p.conn.clone();
        let what2 = what.clone();
        tokio::spawn(async move {
            let pumping = {
                let me = me.clone();
                tokio::spawn(async move {
                    while let Some(l) = rx.recv().await {
                        me.send(rpc::notification(
                            "script.output",
                            json!({ "run_id": run_id, "stream": l.stream, "text": l.text }),
                        ));
                    }
                })
            };
            // A script typed in a client gets no connection context: it was not
            // run around a connection, so there is nothing true to tell it.
            let env = BTreeMap::new();
            let running = crate::runner::run(Some(lang), &body, &env, timeout, tx);
            let outcome = tokio::select! {
                o = running => o,
                _ = cancel_rx => Ok(crate::runner::Outcome { code: None, ms: 0, timed_out: false }),
            };
            let _ = pumping.await;
            me.inner.runs.lock().unwrap().remove(&run_id);

            let mut r = audit::Record::new("script", "applied");
            r.conn = if conn.is_empty() { None } else { Some(conn) };
            r.about(me.inner.audit_mode, &body);
            let (state, ms) = match &outcome {
                Ok(o) if o.ok() => ("done", o.ms),
                Ok(o) => {
                    r.outcome = "failed".into();
                    r.because(o.why());
                    ("failed", o.ms)
                }
                Err(e) => {
                    r.outcome = "failed".into();
                    r.because(e.to_string());
                    ("failed", 0)
                }
            };
            r.ms = Some(ms as i64);
            me.audit(r).await;
            me.send(rpc::notification(
                "script.state",
                json!({
                    "run_id": run_id,
                    "name": what2,
                    "state": state,
                    "ms": ms,
                    "code": outcome.as_ref().ok().and_then(|o| o.code),
                    "error": outcome.as_ref().err().map(|e| e.to_string())
                        .or_else(|| outcome.as_ref().ok().filter(|o| !o.ok()).map(|o| o.why())),
                }),
            ));
        });
        Ok(json!({ "run_id": run_id, "name": what }))
    }

    async fn script_cancel(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            run_id: u64,
        }
        let p: P = serde_json::from_value(params)?;
        let sender = self.inner.runs.lock().unwrap().remove(&p.run_id);
        Ok(json!({ "cancelled": sender.map(|s| s.send(()).is_ok()).unwrap_or(false) }))
    }

    // ── profiles ─────────────────────────────────────────────────────────────

    /// Resolve the profiles a spec NAMES into the spec itself.
    ///
    /// Done here, in the engine, every time a connection is opened or tested —
    /// so a profile edited once is edited for every connection that names it,
    /// including the ones in the other client, including the ones that are open
    /// right now and will pick it up on their next connect.
    ///
    /// A named profile that is missing is an error with the NAME in it. The
    /// alternative — carrying on without the tunnel — is the one outcome that
    /// must never happen: a connection whose author asked for a bastion and got
    /// a direct link instead.
    async fn resolve_profiles(&self, spec: &mut ConnSpec) -> anyhow::Result<()> {
        spec.check_profiles()?;
        if spec.tunnel_profile.is_none() && spec.proxy_profile.is_none() {
            return Ok(());
        }
        let store = self.store().map_err(|e| {
            anyhow::anyhow!("this connection names a saved profile, and {e}")
        })?;
        if let Some(name) = spec.tunnel_profile.clone() {
            let raw = store.get_profile("tunnel".into(), name.clone()).await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "there is no saved tunnel profile called '{name}' — this connection will NOT \
                     fall back to a direct link"
                )
            })?;
            spec.tunnel = Some(serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("the saved tunnel profile '{name}' cannot be read: {e}")
            })?);
        }
        if let Some(name) = spec.proxy_profile.clone() {
            let raw = store.get_profile("proxy".into(), name.clone()).await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "there is no saved proxy profile called '{name}' — this connection will NOT \
                     fall back to a direct link"
                )
            })?;
            spec.proxy = Some(serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("the saved proxy profile '{name}' cannot be read: {e}")
            })?);
        }
        Ok(())
    }

    async fn store_profile_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            kind: String,
        }
        let p: P = serde_json::from_value(params)?;
        check_profile_kind(&p.kind)?;
        Ok(json!({ "profiles": self.store()?.list_profiles(p.kind).await? }))
    }

    async fn store_profile_get(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            kind: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        check_profile_kind(&p.kind)?;
        let raw = self.store()?.get_profile(p.kind, p.name).await?;
        Ok(json!({ "spec": raw.and_then(|r| serde_json::from_str::<Json>(&r).ok()) }))
    }

    /// Save a profile, after PARSING it as the thing it claims to be.
    ///
    /// Parsed rather than stored as opaque JSON, so a malformed tunnel is
    /// refused here and not at three in the morning when a connection that
    /// names it will not open. The parse is the check: `TunnelSpec` has
    /// required fields, and a document missing one does not become a tunnel.
    async fn store_profile_save(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            kind: String,
            name: String,
            spec: Json,
        }
        let p: P = serde_json::from_value(params)?;
        check_profile_kind(&p.kind)?;
        if p.name.trim().is_empty() {
            return Err(anyhow::anyhow!("a profile needs a name"));
        }
        let canonical = match p.kind.as_str() {
            // An access profile is a REFERENCE — `{ provider, profile }` — and
            // the thing it refers to lives in the AWS files. There is no
            // struct to parse it into here, and inventing one would be a second
            // model of somebody else's format.
            // An access profile is a REFERENCE and a driver profile is a
            // command line. Neither has a struct here to parse into, and
            // inventing one would be a second model of somebody else's format.
            "access" | "driver" => serde_json::to_string(&p.spec)?,
            "tunnel" => {
                let t: crate::spec::TunnelSpec = serde_json::from_value(p.spec)
                    .map_err(|e| anyhow::anyhow!("that is not a tunnel: {e}"))?;
                serde_json::to_string(&t)?
            }
            _ => {
                let x: crate::spec::ProxySpec = serde_json::from_value(p.spec)
                    .map_err(|e| anyhow::anyhow!("that is not a proxy: {e}"))?;
                serde_json::to_string(&x)?
            }
        };
        self.store()?.save_profile(p.kind, p.name, canonical).await?;
        Ok(json!({ "saved": true }))
    }

    /// Remove a profile, and SAY which connections were using it.
    ///
    /// It is removed either way — it is the person's data, and refusing would
    /// leave them unable to clean up. But a removal that takes twenty
    /// connections down silently is a surprise, not a choice, so the names come
    /// back with the answer.
    async fn store_profile_remove(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            kind: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        check_profile_kind(&p.kind)?;
        let store = self.store()?;
        let users = store.profile_users(p.kind.clone(), p.name.clone()).await?;
        let removed = store.remove_profile(p.kind, p.name).await?;
        Ok(json!({ "removed": removed, "was_used_by": users }))
    }

    // ── dashboards ───────────────────────────────────────────────────────────

    async fn store_dashboard_list(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "dashboards": self.store()?.list_dashboards(p.conn).await? }))
    }

    /// One board, re-validated on the way OUT as well as on the way in.
    ///
    /// Not belt and braces: a board is legal when it is saved and can stop being
    /// legal afterwards, because a saved chart it names can be deleted from the
    /// other client while this one has the board open. The error says which
    /// chart, which is the thing somebody can act on; returning the layout and
    /// letting the client draw an empty square would say only that something is
    /// missing.
    async fn store_dashboard_get(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        let store = self.store()?;
        let Some(text) = store.get_dashboard(p.conn.clone(), p.name).await? else {
            return Ok(json!({ "dashboard": Json::Null }));
        };
        let mut board: dashboard::Dashboard = serde_json::from_str(&text)?;
        let charts = store.chart_names(p.conn).await?;
        let stale = board.validate(&charts).err().map(|e| e.to_string());
        Ok(json!({
            "dashboard": board,
            // The board comes back EITHER WAY, with the complaint beside it: a
            // layout somebody spent time on should be openable and repairable,
            // not withheld because one of its charts went away.
            "stale": stale,
            "columns": dashboard::COLUMNS,
        }))
    }

    async fn store_dashboard_save(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
            dashboard: dashboard::Dashboard,
        }
        let p: P = serde_json::from_value(params)?;
        if p.name.trim().is_empty() {
            return Err(anyhow::anyhow!("a dashboard needs a name"));
        }
        let store = self.store()?;
        let charts = store.chart_names(p.conn.clone()).await?;
        // Validated HERE, in the engine, before a single byte is written. A
        // client could check the same things; the point is that it does not
        // have to be trusted to, because a board saved wrong is a board that is
        // wrong every time it is opened, by every client, for as long as it
        // exists.
        let mut board = p.dashboard;
        board.validate(&charts)?;
        store
            .save_dashboard(p.conn, p.name, serde_json::to_string(&board)?)
            .await?;
        Ok(json!({ "saved": true, "dashboard": board }))
    }

    async fn store_dashboard_remove(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "removed": self.store()?.remove_dashboard(p.conn, p.name).await? }))
    }

    async fn store_chart_remove(&self, params: Json) -> anyhow::Result<Json> {
        #[derive(Deserialize)]
        struct P {
            conn: String,
            name: String,
        }
        let p: P = serde_json::from_value(params)?;
        Ok(json!({ "removed": self.store()?.remove_chart(p.conn, p.name).await? }))
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
