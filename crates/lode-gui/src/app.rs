//! The application.
//!
//! One rule shapes the behaviour: **the UI thread never waits on a database.**
//! Every request is sent with `Engine::send`, which returns an id, and the
//! answer is picked up in a later frame from `Engine::try_recv`. A `pending`
//! map says what each outstanding id was for. There is no spinner that is
//! secretly a frozen window, and a query against a slow server leaves the
//! editor, the tree and the other tabs fully usable.
//!
//! The only blocking call in the whole client is the first `rpc.hello`, in
//! `App::new`, where there is nothing else to do yet and a failure has to be
//! reportable before a window exists.
//!
//! **Every colour comes from the theme.** There is not one hex value in this
//! file. A surface the design needs and the theme does not have is a role added
//! to the generator in lvim-colorscheme and regenerated across the 48 palettes,
//! not a shade mixed here — otherwise the client's idea of a theme and the
//! generator's drift apart, and the contrast guarantees stop meaning anything.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use egui_extras::{Column as TCol, TableBuilder};
use lode_proto::{cell_text, Column, DriverMeta, Engine, Hello, Message, Node, QueryState, SavedConnection};
use serde_json::{json, Value};

use crate::export::{self, Format};
use crate::font::{self, Fonts, Icons};
use crate::theme::{self, Theme};

/// Rows fetched per page. The engine buffers the whole result and serves slices,
/// so this bounds what the grid holds, never what exists.
const PAGE: usize = 200;

/// Timeout for the few calls that are allowed to block (the handshake).
const BLOCKING: Duration = Duration::from_secs(15);

/// The connection-test stages the engine understands, in the order a connection
/// is actually established — which is the order the form shows them in, so a
/// failure points at the first layer that is wrong rather than the last.
const STAGES: [(&str, &str); 4] = [
    ("endpoint", "Is the host (or the file) there at all?"),
    ("tunnel", "Can the SSH forward be stood up on its own?"),
    ("tls", "Does the link come up encrypted?"),
    ("auth", "Are the credentials accepted?"),
];

/// What an outstanding request was for.
enum Pending {
    StoreStatus,
    Connections,
    Connect { name: String, driver: String },
    Disconnect,
    Structure,
    SavedQueries,
    Execute { tab: u64 },
    Page { tab: u64, from: usize },
    /// A write that changed the SHARED saved state: its answer triggers a
    /// re-read, because another client may have changed something else too.
    StoreWrite(&'static str),
    /// Fire and forget: nothing to update on success, but a failure is still
    /// worth a log line. Releasing a buffer, cancelling, recording history.
    Ignore(&'static str),
    /// One `conn.test` stage from the connection form.
    Test(&'static str),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum RunState {
    Idle,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One editor tab: its statement, the call it produced and the page it holds.
struct Tab {
    id: u64,
    title: String,
    sql: String,
    call_id: Option<u64>,
    state: RunState,
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    from: usize,
    has_more: bool,
    total: Option<usize>,
    affected: Option<u64>,
    ms: u64,
    error: Option<String>,
    /// The row the pointer last chose, as an index into `rows`.
    selected: Option<usize>,
}

impl Tab {
    fn new(id: u64) -> Self {
        Tab {
            id,
            title: format!("Query {id}"),
            sql: String::new(),
            call_id: None,
            state: RunState::Idle,
            columns: Vec::new(),
            rows: Vec::new(),
            from: 0,
            has_more: false,
            total: None,
            affected: None,
            ms: 0,
            error: None,
            selected: None,
        }
    }
}

/// The live connection, if any.
struct Live {
    name: String,
    driver: String,
    conn_id: u64,
    encrypted: bool,
    tunneled: bool,
    nodes: Vec<Node>,
    queries: Vec<lode_proto::SavedQuery>,
}

/// The result of one `conn.test` stage.
#[derive(Clone)]
enum TestState {
    Running,
    Ok(String, u64),
    Failed(String),
}

/// The add/edit connection form.
///
/// The parameter fields are still built entirely from the engine's `DriverMeta`,
/// so a new database type needs no change here. What is NOT in `DriverMeta` —
/// the auth method's own fields, the TLS posture, the SSH tunnel — is the same
/// typed shape for every driver, so it is laid out explicitly. Without it the
/// desktop client could only use connections someone had first configured in
/// Neovim, which rather defeats the point of two clients sharing one engine.
struct ConnForm {
    name: String,
    driver: String,
    params: HashMap<String, String>,
    /// The driver whose fields `params` currently holds.
    ///
    /// Without this the map is never reconciled with the chosen driver, and the
    /// defaults are inserted with `or_insert_with`, which does nothing when the
    /// key is already there. Pick Cassandra, then MongoDB, and the port stays
    /// 9042 — the form shows Cassandra's default under Mongo's label and the
    /// engine is blamed for it. The same applies to every key two drivers
    /// share: `host`, `database`, `user`.
    applied_driver: String,

    auth_kind: String,
    auth_user: String,
    /// A TEMPLATE, never a resolved password. The engine resolves it at connect
    /// time; nothing here ever sees the value.
    auth_secret: String,
    auth_cert: String,
    auth_key: String,
    auth_key_password: String,
    auth_provider: String,
    auth_principal: String,

    tls_mode: String,
    tls_ca: String,
    tls_client_cert: String,
    tls_client_key: String,

    tunnel_on: bool,
    tunnel_host: String,
    tunnel_port: String,
    tunnel_user: String,
    tunnel_auth: String,
    tunnel_key_path: String,
    tunnel_secret: String,

    editing: Option<String>,
    error: Option<String>,
    tests: HashMap<&'static str, TestState>,
}

impl ConnForm {
    fn blank(driver: String) -> Self {
        ConnForm {
            name: String::new(),
            applied_driver: driver.clone(),
            driver,
            params: HashMap::new(),
            auth_kind: "none".into(),
            auth_user: String::new(),
            auth_secret: String::new(),
            auth_cert: String::new(),
            auth_key: String::new(),
            auth_key_password: String::new(),
            auth_provider: String::new(),
            auth_principal: String::new(),
            tls_mode: "prefer".into(),
            tls_ca: String::new(),
            tls_client_cert: String::new(),
            tls_client_key: String::new(),
            tunnel_on: false,
            tunnel_host: String::new(),
            tunnel_port: "22".into(),
            tunnel_user: String::new(),
            tunnel_auth: "agent".into(),
            tunnel_key_path: String::new(),
            tunnel_secret: String::new(),
            editing: None,
            error: None,
            tests: HashMap::new(),
        }
    }

    /// Bring `params` into line with the chosen driver, once per real change.
    ///
    /// On a change the field set is REBUILT from the new driver's declared
    /// parameters and their defaults, rather than kept and topped up. Keeping
    /// them would mean carrying one database's values under another's labels,
    /// and the value most likely to survive is a port — the one field where
    /// being silently wrong looks exactly like being right. A connection to a
    /// different kind of database is a different connection.
    ///
    /// The last test results go too: they were answers about another endpoint,
    /// and a stale green is the same lie this form was just fixed for.
    ///
    /// Returns whether anything was reset, so a caller can say so if it wants.
    fn sync_driver(&mut self, meta: Option<&DriverMeta>) -> bool {
        if self.driver == self.applied_driver {
            return false;
        }
        self.applied_driver = self.driver.clone();
        let Some(m) = meta else {
            // The engine does not know this driver, so there is no field set to
            // rebuild from. Leaving the map alone is the only honest option;
            // the form will show nothing to fill in either way.
            return true;
        };
        self.params = m
            .params
            .iter()
            .map(|p| (p.key.clone(), p.default.clone().unwrap_or_default()))
            .collect();
        // The auth method is the same kind of staleness: a driver that does not
        // offer the selected one would show a method it cannot use.
        if !m.auth.is_empty() && !m.auth.iter().any(|a| *a == self.auth_kind) {
            self.auth_kind = m.auth[0].clone();
        }
        self.tests.clear();
        true
    }

    /// The `ConnSpec` the engine accepts: driver, params, auth, tls, tunnel.
    /// This is what `conn.test` and `conn.connect` both take, so the form tests
    /// exactly what it would connect with — not an approximation of it.
    fn spec(&self) -> Value {
        let mut params = serde_json::Map::new();
        for (k, v) in &self.params {
            if !v.is_empty() {
                params.insert(k.clone(), json!(v));
            }
        }
        let auth = match self.auth_kind.as_str() {
            "password" => json!({ "kind": "password", "user": self.auth_user, "password": self.auth_secret }),
            "client_cert" => json!({
                "kind": "client_cert", "cert": self.auth_cert, "key": self.auth_key,
                "key_password": self.auth_key_password, "user": self.auth_user
            }),
            "provider" => json!({
                "kind": "provider", "provider": self.auth_provider,
                "token": self.auth_secret, "user": self.auth_user
            }),
            "kerberos" => json!({
                "kind": "kerberos",
                "principal": if self.auth_principal.is_empty() { Value::Null } else { json!(self.auth_principal) }
            }),
            _ => json!({ "kind": "none" }),
        };
        let mut tls = serde_json::Map::new();
        tls.insert("mode".into(), json!(self.tls_mode));
        for (k, v) in [
            ("ca", &self.tls_ca),
            ("client_cert", &self.tls_client_cert),
            ("client_key", &self.tls_client_key),
        ] {
            if !v.is_empty() {
                tls.insert(k.into(), json!(v));
            }
        }
        let tunnel = if self.tunnel_on {
            let auth = match self.tunnel_auth.as_str() {
                "key" => json!({ "kind": "key", "path": self.tunnel_key_path, "passphrase": self.tunnel_secret }),
                "password" => json!({ "kind": "password", "password": self.tunnel_secret }),
                _ => json!({ "kind": "agent" }),
            };
            json!({
                "host": self.tunnel_host,
                "port": self.tunnel_port.trim().parse::<u16>().unwrap_or(22),
                "user": self.tunnel_user,
                "auth": auth
            })
        } else {
            Value::Null
        };
        json!({
            "driver": self.driver,
            "params": Value::Object(params),
            "auth": auth,
            "tls": Value::Object(tls),
            "tunnel": tunnel
        })
    }

    /// What is SAVED: the spec without its driver, which the store keeps in its
    /// own column. Secrets are still templates — see `spec`.
    fn stored_spec(&self) -> Value {
        let mut v = self.spec();
        if let Some(m) = v.as_object_mut() {
            m.remove("driver");
        }
        v
    }
}

struct ExportState {
    open: bool,
    path: String,
    format: Format,
    message: Option<String>,
}

pub struct App {
    engine: Option<Engine>,
    fatal: Option<String>,
    hello: Option<Hello>,

    themes: std::collections::BTreeMap<String, PathBuf>,
    theme_name: String,
    theme: Theme,
    theme_dirty: bool,
    scale: f32,
    fonts: Fonts,
    icon: Icons,

    store_ok: bool,
    saved: Vec<SavedConnection>,
    live: Option<Live>,

    tabs: Vec<Tab>,
    active: usize,
    next_tab: u64,

    pending: HashMap<u64, Pending>,
    /// call_id → tab id, so a `query.state` notification finds its tab.
    calls: HashMap<u64, u64>,

    form: Option<ConnForm>,
    export: ExportState,
    log: Vec<(String, String)>,
    status: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let fonts = font::install(&cc.egui_ctx);
        let icon = Icons::for_fonts(&fonts);
        let scale = Theme::scale();
        let themes = theme::discover();
        let want = std::env::var("LODE_THEME").ok();
        let (theme_name, theme) = pick_theme(&themes, want.as_deref());
        theme.apply(&cc.egui_ctx, scale);

        let mut app = App {
            engine: None,
            fatal: None,
            hello: None,
            themes,
            theme_name,
            theme,
            theme_dirty: false,
            scale,
            fonts,
            icon,
            store_ok: false,
            saved: Vec::new(),
            live: None,
            tabs: vec![Tab::new(1)],
            active: 0,
            next_tab: 2,
            pending: HashMap::new(),
            calls: HashMap::new(),
            form: None,
            export: ExportState {
                open: false,
                path: String::new(),
                format: Format::Csv,
                message: None,
            },
            log: Vec::new(),
            status: "starting the engine…".into(),
        };

        match Engine::spawn(None) {
            Ok(engine) => match engine.hello(BLOCKING) {
                Ok(hello) => {
                    app.status = format!(
                        "{} {} — protocol {}, {} drivers",
                        hello.engine.as_deref().unwrap_or("engine"),
                        hello.version.as_deref().unwrap_or("?"),
                        hello.proto,
                        hello.drivers.len()
                    );
                    app.hello = Some(hello);
                    app.engine = Some(engine);
                    app.ask(Pending::StoreStatus, "store.status", json!({}));
                    app.ask(Pending::Connections, "store.connections.list", json!({}));
                }
                Err(e) => app.fatal = Some(e.to_string()),
            },
            Err(e) => app.fatal = Some(e.to_string()),
        }
        app
    }

    /// Queue a request and remember what it was for.
    fn ask(&mut self, what: Pending, method: &str, params: Value) {
        let Some(engine) = &self.engine else { return };
        match engine.send(method, params) {
            Ok(id) => {
                self.pending.insert(id, what);
            }
            Err(e) => {
                // A write to a dead pipe is the engine being gone. Say so once,
                // here, rather than leaving a request pending for ever.
                self.fatal = Some(format!("lost the engine: {e}"));
            }
        }
    }

    fn tab_by_id(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn drivers(&self) -> &[DriverMeta] {
        self.hello.as_ref().map(|h| h.drivers.as_slice()).unwrap_or(&[])
    }

    // ── incoming ─────────────────────────────────────────────────────────────

    /// Drain everything the engine has said since the last frame.
    fn pump(&mut self) -> bool {
        let mut saw = false;
        loop {
            let Some(engine) = &self.engine else { return saw };
            let Some(msg) = engine.try_recv() else { return saw };
            saw = true;
            match msg {
                Message::Closed => {
                    self.fatal = Some(
                        "the engine exited. Its drivers are in a separate process on purpose, so this \
                         window survived it — restart to reconnect."
                            .into(),
                    );
                    self.engine = None;
                    self.live = None;
                    return saw;
                }
                Message::Notification { method, params } => self.on_notification(&method, params),
                Message::Response { id, result } => {
                    let Some(what) = self.pending.remove(&id) else { continue };
                    match result {
                        Ok(v) => self.on_result(what, v),
                        Err(e) => self.on_error(what, e),
                    }
                }
            }
        }
    }

    fn on_notification(&mut self, method: &str, params: Value) {
        match method {
            "query.state" => {
                let Ok(st) = serde_json::from_value::<QueryState>(params) else { return };
                let Some(&tab_id) = self.calls.get(&st.call_id) else { return };
                let (want_page, from) = {
                    let Some(tab) = self.tab_by_id(tab_id) else { return };
                    tab.ms = st.ms;
                    match st.state.as_str() {
                        "done" => {
                            tab.state = RunState::Done;
                            tab.affected = st.affected;
                            tab.error = None;
                            (true, tab.from)
                        }
                        "failed" => {
                            tab.state = RunState::Failed;
                            tab.error = st.error.clone();
                            (false, 0)
                        }
                        "cancelled" => {
                            tab.state = RunState::Cancelled;
                            (false, 0)
                        }
                        _ => (false, 0),
                    }
                };
                if want_page {
                    let call_id = st.call_id;
                    self.ask(
                        Pending::Page { tab: tab_id, from },
                        "query.page",
                        json!({ "call_id": call_id, "from": from, "n": PAGE }),
                    );
                }
                // The history row is written HERE, once the statement is over,
                // carrying the state it reached and the time it took. Writing it
                // at launch instead would mean every row in a shared call log
                // said "running" for ever.
                if matches!(st.state.as_str(), "done" | "failed" | "cancelled") {
                    let sql = self.tabs.iter().find(|t| t.id == tab_id).map(|t| t.sql.trim().to_string());
                    let (conn, driver) = match &self.live {
                        Some(l) => (Some(l.name.clone()), Some(l.driver.clone())),
                        None => (None, None),
                    };
                    if let Some(sql) = sql.filter(|s| !s.is_empty()) {
                        self.ask(
                            Pending::Ignore("history"),
                            "store.history.record",
                            json!({
                                "conn": conn, "driver": driver, "statement": sql,
                                "state": st.state, "ms": st.ms, "rows": st.affected
                            }),
                        );
                    }
                }
            }
            "daemon.log" => {
                let level = params.get("level").and_then(Value::as_str).unwrap_or("info").to_string();
                let message = params.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                self.log.push((level, message));
                if self.log.len() > 500 {
                    self.log.remove(0);
                }
            }
            _ => {}
        }
    }

    fn on_result(&mut self, what: Pending, v: Value) {
        match what {
            Pending::StoreStatus => {
                self.store_ok = v.get("available").and_then(Value::as_bool).unwrap_or(false);
                if !self.store_ok {
                    let why = v.get("error").and_then(Value::as_str).unwrap_or("unknown");
                    self.log.push(("warn".into(), format!("saved state unavailable: {why}")));
                }
            }
            Pending::Connections => {
                self.saved = serde_json::from_value(v.get("connections").cloned().unwrap_or(json!([])))
                    .unwrap_or_default();
            }
            Pending::Connect { name, driver } => {
                let conn_id = v.get("conn_id").and_then(Value::as_u64).unwrap_or(0);
                self.live = Some(Live {
                    name: name.clone(),
                    driver,
                    conn_id,
                    encrypted: v.get("encrypted").and_then(Value::as_bool).unwrap_or(false),
                    tunneled: v.get("tunneled").and_then(Value::as_bool).unwrap_or(false),
                    nodes: Vec::new(),
                    queries: Vec::new(),
                });
                self.status = format!("connected: {name}");
                self.ask(Pending::Structure, "schema.structure", json!({ "conn_id": conn_id }));
                self.ask(Pending::SavedQueries, "store.queries.list", json!({ "conn": name }));
            }
            Pending::Disconnect => {
                self.live = None;
                self.status = "disconnected".into();
            }
            Pending::Structure => {
                if let Some(live) = &mut self.live {
                    live.nodes =
                        serde_json::from_value(v.get("nodes").cloned().unwrap_or(json!([]))).unwrap_or_default();
                }
            }
            Pending::SavedQueries => {
                if let Some(live) = &mut self.live {
                    live.queries = serde_json::from_value(v.get("queries").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                }
            }
            Pending::Execute { tab } => {
                let call_id = v.get("call_id").and_then(Value::as_u64).unwrap_or(0);
                self.calls.insert(call_id, tab);
                if let Some(t) = self.tab_by_id(tab) {
                    t.call_id = Some(call_id);
                    t.state = RunState::Running;
                }
            }
            Pending::Page { tab, from } => {
                let ready = v.get("ready").and_then(Value::as_bool).unwrap_or(false);
                if !ready {
                    return; // still running; the state notification will come
                }
                let page: lode_proto::Page = match serde_json::from_value(v) {
                    Ok(p) => p,
                    Err(e) => {
                        if let Some(t) = self.tab_by_id(tab) {
                            t.error = Some(format!("malformed page: {e}"));
                        }
                        return;
                    }
                };
                if let Some(t) = self.tab_by_id(tab) {
                    t.columns = page.columns;
                    t.rows = page.rows;
                    // The ENGINE's `from` wins over the one we asked for: it
                    // clamps a request past the end of the buffer, and trusting
                    // our own number would label the rows with an offset they
                    // do not have.
                    let _ = from;
                    t.from = page.from;
                    t.has_more = page.has_more;
                    t.total = page.total;
                    t.affected = page.affected.or(t.affected);
                    t.selected = None;
                }
            }
            Pending::StoreWrite(_) => {
                // Re-read rather than patching the local copy: the engine's store
                // is SHARED, so another client may have changed something else in
                // the meantime and a local patch would hide it.
                self.ask(Pending::Connections, "store.connections.list", json!({}));
                if let Some(name) = self.live.as_ref().map(|l| l.name.clone()) {
                    self.ask(Pending::SavedQueries, "store.queries.list", json!({ "conn": name }));
                }
            }
            Pending::Ignore(_) => {}
            Pending::Test(stage) => {
                let detail = v.get("detail").and_then(Value::as_str).unwrap_or("ok").to_string();
                let ms = v.get("ms").and_then(Value::as_u64).unwrap_or(0);
                if let Some(f) = &mut self.form {
                    f.tests.insert(stage, TestState::Ok(detail, ms));
                }
            }
        }
    }

    fn on_error(&mut self, what: Pending, e: String) {
        match what {
            Pending::Execute { tab } | Pending::Page { tab, .. } => {
                if let Some(t) = self.tab_by_id(tab) {
                    t.state = RunState::Failed;
                    t.error = Some(e);
                }
            }
            Pending::Connect { name, .. } => {
                self.status = format!("could not connect to {name}");
                self.log.push(("error".into(), format!("{name}: {e}")));
            }
            // A failed test belongs NEXT TO ITS BUTTON, verbatim: the engine's
            // message names the layer and the reason, and summarising it into
            // the status bar would throw away the only useful part.
            Pending::Test(stage) => {
                if let Some(f) = &mut self.form {
                    f.tests.insert(stage, TestState::Failed(e));
                }
            }
            other => {
                let label = match other {
                    Pending::StoreStatus => "store.status",
                    Pending::Connections => "connections",
                    Pending::Disconnect => "disconnect",
                    Pending::Structure => "schema",
                    Pending::SavedQueries => "saved queries",
                    Pending::StoreWrite(w) | Pending::Ignore(w) => w,
                    _ => "request",
                };
                self.log.push(("error".into(), format!("{label}: {e}")));
            }
        }
    }

    // ── actions ──────────────────────────────────────────────────────────────

    fn connect(&mut self, name: &str) {
        let Some(c) = self.saved.iter().find(|c| c.name == name).cloned() else { return };
        // The stored spec IS the ConnSpec minus its driver; add it back rather
        // than rebuilding, so whatever the form saved is what the engine gets.
        let mut spec = match c.spec {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        spec.insert("driver".into(), json!(c.driver.clone()));
        self.status = format!("connecting to {name}…");
        self.ask(
            Pending::Connect { name: c.name.clone(), driver: c.driver.clone() },
            "conn.connect",
            Value::Object(spec),
        );
    }

    fn disconnect(&mut self) {
        let Some(live) = &self.live else { return };
        let conn_id = live.conn_id;
        self.ask(Pending::Disconnect, "conn.disconnect", json!({ "conn_id": conn_id }));
    }

    fn run_active(&mut self) {
        let Some(live) = &self.live else {
            self.status = "connect to something first".into();
            return;
        };
        let conn_id = live.conn_id;
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let sql = tab.sql.trim().to_string();
        if sql.is_empty() {
            return;
        }
        let tab_id = tab.id;
        let old_call = tab.call_id.take();
        tab.state = RunState::Running;
        tab.error = None;
        tab.rows.clear();
        tab.columns.clear();
        tab.from = 0;
        tab.total = None;
        tab.affected = None;
        tab.selected = None;

        // Free the previous result's buffer on the engine: a browsing session
        // would otherwise grow one buffer per run for the whole process.
        if let Some(old) = old_call {
            self.calls.remove(&old);
            self.ask(Pending::Ignore("release"), "query.release", json!({ "call_id": old }));
        }
        self.ask(
            Pending::Execute { tab: tab_id },
            "query.execute",
            json!({ "conn_id": conn_id, "statement": sql }),
        );
    }

    fn cancel_active(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(call_id) = tab.call_id else { return };
        self.ask(Pending::Ignore("cancel"), "query.cancel", json!({ "call_id": call_id }));
    }

    /// Move the active tab's window by `pages` (negative goes back).
    fn page_by(&mut self, pages: i64) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(call_id) = tab.call_id else { return };
        if tab.state != RunState::Done {
            return;
        }
        let from = if pages >= 0 {
            if !tab.has_more {
                return;
            }
            tab.from + PAGE * pages as usize
        } else {
            tab.from.saturating_sub(PAGE * (-pages) as usize)
        };
        let tab_id = tab.id;
        self.ask(
            Pending::Page { tab: tab_id, from },
            "query.page",
            json!({ "call_id": call_id, "from": from, "n": PAGE }),
        );
    }

    fn run_test(&mut self, stage: &'static str) {
        let Some(form) = &mut self.form else { return };
        form.tests.insert(stage, TestState::Running);
        let spec = form.spec();
        self.ask(Pending::Test(stage), "conn.test", json!({ "stage": stage, "spec": spec }));
    }

    fn save_form(&mut self) {
        let Some(form) = &self.form else { return };
        if form.name.trim().is_empty() {
            if let Some(f) = &mut self.form {
                f.error = Some("a connection needs a name".into());
            }
            return;
        }
        let (name, driver) = (form.name.clone(), form.driver.clone());
        let spec = form.stored_spec();
        let renamed_from = form.editing.clone().filter(|old| *old != name);
        self.ask(
            Pending::StoreWrite("save connection"),
            "store.connections.save",
            json!({ "name": name, "driver": driver, "spec": spec }),
        );
        // A rename is a save under the new name plus a removal of the old one;
        // the store is keyed by name and has no rename of its own.
        if let Some(old) = renamed_from {
            self.ask(
                Pending::StoreWrite("rename connection"),
                "store.connections.remove",
                json!({ "name": old }),
            );
        }
        self.form = None;
    }

    fn open_form(&mut self, edit: Option<&SavedConnection>) {
        let first_driver = self
            .drivers()
            .first()
            .map(|d| d.kind.clone())
            .unwrap_or_else(|| "sqlite".into());
        let mut form = ConnForm::blank(first_driver);
        if let Some(c) = edit {
            form.name = c.name.clone();
            form.driver = c.driver.clone();
            // The saved values ARE this driver's, so the form opens already in
            // sync. Without this the first frame would see a driver change and
            // throw the stored parameters away before they were ever shown.
            form.applied_driver = c.driver.clone();
            form.editing = Some(c.name.clone());
            if let Some(p) = c.spec.get("params").and_then(Value::as_object) {
                for (k, v) in p {
                    form.params
                        .insert(k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
                }
            }
            if let Some(a) = c.spec.get("auth").and_then(Value::as_object) {
                let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                form.auth_kind = a.get("kind").and_then(Value::as_str).unwrap_or("none").to_string();
                form.auth_user = s("user");
                form.auth_secret = a
                    .get("password")
                    .or_else(|| a.get("token"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                form.auth_cert = s("cert");
                form.auth_key = s("key");
                form.auth_key_password = s("key_password");
                form.auth_provider = s("provider");
                form.auth_principal = s("principal");
            }
            if let Some(t) = c.spec.get("tls").and_then(Value::as_object) {
                let s = |k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                form.tls_mode = t.get("mode").and_then(Value::as_str).unwrap_or("prefer").to_string();
                form.tls_ca = s("ca");
                form.tls_client_cert = s("client_cert");
                form.tls_client_key = s("client_key");
            }
            if let Some(t) = c.spec.get("tunnel").and_then(Value::as_object) {
                form.tunnel_on = true;
                form.tunnel_host = t.get("host").and_then(Value::as_str).unwrap_or("").to_string();
                form.tunnel_port = t
                    .get("port")
                    .and_then(Value::as_u64)
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "22".into());
                form.tunnel_user = t.get("user").and_then(Value::as_str).unwrap_or("").to_string();
                if let Some(a) = t.get("auth").and_then(Value::as_object) {
                    form.tunnel_auth = a.get("kind").and_then(Value::as_str).unwrap_or("agent").to_string();
                    form.tunnel_key_path = a.get("path").and_then(Value::as_str).unwrap_or("").to_string();
                    form.tunnel_secret = a
                        .get("passphrase")
                        .or_else(|| a.get("password"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
        self.form = Some(form);
    }
}

/// Pick a theme: the wanted one, else the first discovered, else built in.
fn pick_theme(
    themes: &std::collections::BTreeMap<String, PathBuf>,
    want: Option<&str>,
) -> (String, Theme) {
    if let Some(w) = want {
        if let Some(p) = themes.get(w) {
            if let Ok(t) = theme::load(p) {
                return (w.to_string(), t);
            }
        }
    }
    for (name, path) in themes {
        if let Ok(t) = theme::load(path) {
            return (name.clone(), t);
        }
    }
    let t = Theme::builtin();
    (t.name.clone(), t)
}

// ── the frame ──────────────────────────────────────────────────────────────────

/// What the UI asked for this frame. Collected rather than done inline, because
/// an egui closure holds `&mut` on the widgets it draws and a mutation of the
/// model from inside it would borrow `self` twice.
enum Act {
    Connect(String),
    Disconnect,
    Edit(String),
    Delete(String),
    New,
    Run,
    Cancel,
    Page(i64),
    NewTab,
    CloseTab(u64),
    SetTheme(String),
    ReloadThemes,
    LoadQuery(String),
    SaveQuery,
    OpenExport,
    Refresh,
    Test(&'static str),
    SelectRow(usize),
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.theme_dirty {
            self.theme.apply(ctx, self.scale);
            self.theme_dirty = false;
        }
        let busy = self.pump() || !self.pending.is_empty();
        // Keep polling while anything is outstanding; otherwise sleep until the
        // next interaction. A GUI that repainted at 60fps to watch an idle pipe
        // would be a fan running for nothing.
        if busy {
            ctx.request_repaint_after(Duration::from_millis(30));
        }

        if let Some(fatal) = self.fatal.clone() {
            let theme = self.theme.clone();
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(theme.base()).inner_margin(egui::Margin::same(24)))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.heading(egui::RichText::new("No engine").color(theme.error()));
                        ui.add_space(10.0);
                        ui.label(fatal);
                        ui.add_space(18.0);
                        ui.label(
                            egui::RichText::new(
                                "lode is the database engine this client talks to. Build it with \
                                 `scripts/build` and put `loded` on $PATH, or set $LODE_DAEMON.",
                            )
                            .color(theme.muted()),
                        );
                    });
                });
            return;
        }

        let mut acts: Vec<Act> = Vec::new();
        self.top_bar(ctx, &mut acts);
        self.bottom_bar(ctx);
        self.left_panel(ctx, &mut acts);
        self.central(ctx, &mut acts);
        self.form_window(ctx, &mut acts);
        self.export_window(ctx);

        for a in acts {
            match a {
                Act::Connect(n) => self.connect(&n),
                Act::Disconnect => self.disconnect(),
                Act::Edit(n) => {
                    let c = self.saved.iter().find(|c| c.name == n).cloned();
                    self.open_form(c.as_ref());
                }
                Act::Delete(n) => self.ask(
                    Pending::StoreWrite("delete connection"),
                    "store.connections.remove",
                    json!({ "name": n }),
                ),
                Act::New => self.open_form(None),
                Act::Run => self.run_active(),
                Act::Cancel => self.cancel_active(),
                Act::Page(d) => self.page_by(d),
                Act::Test(stage) => self.run_test(stage),
                Act::SelectRow(i) => {
                    if let Some(t) = self.tabs.get_mut(self.active) {
                        t.selected = Some(i);
                    }
                }
                Act::NewTab => {
                    let id = self.next_tab;
                    self.next_tab += 1;
                    self.tabs.push(Tab::new(id));
                    self.active = self.tabs.len() - 1;
                }
                Act::CloseTab(id) => {
                    if self.tabs.len() > 1 {
                        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
                            if let Some(call) = self.tabs[pos].call_id {
                                self.calls.remove(&call);
                                self.ask(
                                    Pending::Ignore("release"),
                                    "query.release",
                                    json!({ "call_id": call }),
                                );
                            }
                            self.tabs.remove(pos);
                            self.active = self.active.min(self.tabs.len() - 1);
                        }
                    }
                }
                Act::SetTheme(name) => {
                    if let Some(p) = self.themes.get(&name).cloned() {
                        match theme::load(&p) {
                            Ok(t) => {
                                self.theme = t;
                                self.theme_name = name;
                                self.theme_dirty = true;
                            }
                            Err(e) => self.log.push(("error".into(), format!("theme: {e}"))),
                        }
                    }
                }
                Act::ReloadThemes => {
                    self.themes = theme::discover();
                    self.status = format!("{} theme(s) found", self.themes.len());
                }
                Act::LoadQuery(sql) => {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.sql = sql;
                    }
                }
                Act::SaveQuery => {
                    let (Some(live), Some(tab)) = (self.live.as_ref(), self.tabs.get(self.active)) else {
                        continue;
                    };
                    let (conn, name, sql) = (live.name.clone(), tab.title.clone(), tab.sql.clone());
                    self.ask(
                        Pending::StoreWrite("save query"),
                        "store.queries.save",
                        json!({ "conn": conn, "name": name, "sql": sql }),
                    );
                }
                Act::OpenExport => {
                    self.export.open = true;
                    self.export.message = None;
                    if self.export.path.is_empty() {
                        let name = self
                            .tabs
                            .get(self.active)
                            .map(|t| t.title.replace(' ', "-").to_lowercase())
                            .unwrap_or_else(|| "result".into());
                        self.export.path = format!("{name}.{}", self.export.format.extension());
                    }
                }
                Act::Refresh => {
                    if let Some(live) = &self.live {
                        let id = live.conn_id;
                        self.ask(Pending::Structure, "schema.structure", json!({ "conn_id": id }));
                    }
                    self.ask(Pending::Connections, "store.connections.list", json!({}));
                }
            }
        }
    }
}

impl App {
    /// A toolbar button: the glyph and the word together when there is a glyph
    /// set, the word alone otherwise — never a mystery glyph on its own.
    fn tool(&self, ui: &mut egui::Ui, icon: &str, label: &str, enabled: bool) -> egui::Response {
        let text = if self.fonts.nerd {
            format!("{icon}  {label}")
        } else {
            label.to_string()
        };
        ui.add_enabled(enabled, egui::Button::new(text))
    }

    /// The one button in a group that is the obvious thing to press.
    fn primary(&self, ui: &mut egui::Ui, icon: &str, label: &str, enabled: bool) -> egui::Response {
        let text = if self.fonts.nerd {
            format!("{icon}  {label}")
        } else {
            label.to_string()
        };
        ui.add_enabled(
            enabled,
            egui::Button::new(egui::RichText::new(text).color(self.theme.inverted()))
                .fill(self.theme.accent()),
        )
    }

    fn top_bar(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let theme = self.theme.clone();
        let frame = egui::Frame::new()
            .fill(theme.panel())
            .inner_margin(egui::Margin::symmetric(12, 8))
            .stroke(egui::Stroke::new(1.0_f32, theme.border()));
        egui::TopBottomPanel::top("top").frame(frame).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let connected = self.live.is_some();
                let running = self
                    .tabs
                    .get(self.active)
                    .map(|t| t.state == RunState::Running)
                    .unwrap_or(false);

                // ── execution ───────────────────────────────────────────────
                if self.primary(ui, self.icon.run, "Run", connected && !running).clicked() {
                    acts.push(Act::Run);
                }
                if self.tool(ui, self.icon.cancel, "Cancel", running).clicked() {
                    acts.push(Act::Cancel);
                }
                ui.separator();

                // ── tabs ────────────────────────────────────────────────────
                if self.tool(ui, self.icon.new_tab, "New tab", true).clicked() {
                    acts.push(Act::NewTab);
                }
                if self.tool(ui, self.icon.save, "Save query", connected).clicked() {
                    acts.push(Act::SaveQuery);
                }
                ui.separator();

                // ── output ──────────────────────────────────────────────────
                let has_rows = self.tabs.get(self.active).map(|t| !t.rows.is_empty()).unwrap_or(false);
                if self.tool(ui, self.icon.export, "Export", has_rows).clicked() {
                    acts.push(Act::OpenExport);
                }
                ui.separator();

                // ── refresh ─────────────────────────────────────────────────
                if self.tool(ui, self.icon.refresh, "Refresh", true).clicked() {
                    acts.push(Act::Refresh);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(self.icon.refresh).small())
                        .on_hover_text("rescan the theme directories")
                        .clicked()
                    {
                        acts.push(Act::ReloadThemes);
                    }
                    let mut chosen = self.theme_name.clone();
                    egui::ComboBox::from_id_salt("theme")
                        .width(190.0)
                        .selected_text(&chosen)
                        .show_ui(ui, |ui| {
                            for name in self.themes.keys() {
                                if ui.selectable_value(&mut chosen, name.clone(), name).clicked() {
                                    acts.push(Act::SetTheme(name.clone()));
                                }
                            }
                            if self.themes.is_empty() {
                                ui.label(egui::RichText::new("no theme files found").color(theme.muted()));
                            }
                        });
                    ui.label(egui::RichText::new("Theme").color(theme.muted()));
                });
            });
        });

        // Ctrl+Enter runs. A bare Enter cannot: the editor is a text area and
        // Enter is a newline there.
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter)) {
            acts.push(Act::Run);
        }
    }

    fn left_panel(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let theme = self.theme.clone();
        let icon = self.icon;
        let frame = egui::Frame::new()
            .fill(theme.panel())
            .inner_margin(egui::Margin::symmetric(10, 10));
        egui::SidePanel::left("side")
            .frame(frame)
            .resizable(true)
            .default_width(290.0)
            .width_range(210.0..=460.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    section(ui, &theme, "Connections");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(icon.add).small())
                            .on_hover_text("add a connection")
                            .clicked()
                        {
                            acts.push(Act::New);
                        }
                    });
                });
                if !self.store_ok {
                    ui.label(
                        egui::RichText::new("the engine has no saved state — connections cannot be stored")
                            .small()
                            .color(theme.warning()),
                    );
                }
                ui.add_space(4.0);

                egui::ScrollArea::vertical()
                    .id_salt("conns")
                    .max_height(200.0)
                    .show(ui, |ui| {
                        if self.saved.is_empty() {
                            ui.label(egui::RichText::new("none saved yet").color(theme.muted()));
                        }
                        for c in &self.saved {
                            let is_live = self.live.as_ref().is_some_and(|l| l.name == c.name);
                            ui.horizontal(|ui| {
                                let dot = if is_live { icon.connected } else { icon.offline };
                                let colour = if is_live { theme.fg() } else { theme.muted() };
                                // The BUTTONS are laid out first, from the right,
                                // and the name takes what is left. Drawing the
                                // name first gives it the whole row and the
                                // right-aligned buttons are then painted ON TOP
                                // of a long one — which is exactly what happened
                                // to "prod (postgres, tunnelled)".
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui
                                        .add(egui::Button::new(icon.delete).small())
                                        .on_hover_text("delete this saved connection")
                                        .clicked()
                                    {
                                        acts.push(Act::Delete(c.name.clone()));
                                    }
                                    if ui
                                        .add(egui::Button::new(icon.edit).small())
                                        .on_hover_text("edit — driver, auth, TLS, SSH tunnel")
                                        .clicked()
                                    {
                                        acts.push(Act::Edit(c.name.clone()));
                                    }
                                    if !is_live
                                        && ui
                                            .add(egui::Button::new(icon.connect).small())
                                            .on_hover_text("connect")
                                            .clicked()
                                    {
                                        acts.push(Act::Connect(c.name.clone()));
                                    }
                                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                        if ui
                                            .add(
                                                egui::Button::selectable(
                                                    is_live,
                                                    egui::RichText::new(format!("{dot}  {}", c.name))
                                                        .color(colour),
                                                )
                                                // Truncate rather than overflow:
                                                // the buttons have already taken
                                                // their width from the right, and
                                                // a name longer than the rest of
                                                // the row has to end in an ellipsis
                                                // instead of running into them.
                                                .wrap_mode(egui::TextWrapMode::Truncate),
                                            )
                                            .on_hover_text(format!(
                                                "{} — double-click here, or press the link button, to connect",
                                                c.driver
                                            ))
                                            .double_clicked()
                                        {
                                            acts.push(Act::Connect(c.name.clone()));
                                        }
                                    });
                                });
                            });
                        }
                    });

                ui.add_space(10.0);
                if let Some(live) = &self.live {
                    section(ui, &theme, &live.name);
                    ui.horizontal(|ui| {
                        let (glyph, text, colour) = if live.encrypted {
                            (
                                icon.locked,
                                if live.tunneled { "encrypted (SSH tunnel)" } else { "encrypted (TLS)" },
                                theme.success(),
                            )
                        } else {
                            (icon.unlocked, "PLAINTEXT", theme.error())
                        };
                        ui.label(
                            egui::RichText::new(format!("{glyph}  {text}"))
                                .small()
                                .color(colour),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(egui::Button::new(icon.disconnect).small())
                                .on_hover_text("disconnect")
                                .clicked()
                            {
                                acts.push(Act::Disconnect);
                            }
                        });
                    });

                    if !live.queries.is_empty() {
                        ui.add_space(6.0);
                        egui::CollapsingHeader::new(format!("{}  Saved queries", icon.query))
                            .default_open(false)
                            .show(ui, |ui| {
                                for q in &live.queries {
                                    if ui.selectable_label(false, &q.name).clicked() {
                                        acts.push(Act::LoadQuery(q.sql.clone()));
                                    }
                                }
                            });
                    }

                    ui.add_space(10.0);
                    section(ui, &theme, "Schema");
                    ui.add_space(2.0);
                    egui::ScrollArea::vertical().id_salt("tree").show(ui, |ui| {
                        if live.nodes.is_empty() {
                            ui.label(egui::RichText::new("loading…").color(theme.muted()));
                        }
                        for n in &live.nodes {
                            draw_node(ui, n, &icon, &mut |sql| acts.push(Act::LoadQuery(sql)));
                        }
                    });
                } else {
                    ui.label(egui::RichText::new("not connected").color(theme.muted()));
                }
            });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let icon = self.icon;
        let frame = egui::Frame::new()
            .fill(theme.status())
            .inner_margin(egui::Margin::symmetric(12, 5))
            .stroke(egui::Stroke::new(1.0_f32, theme.border()));
        egui::TopBottomPanel::bottom("status").frame(frame).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let name = |p: &Option<std::path::PathBuf>| {
                    p.as_ref()
                        .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
                        .unwrap_or("egui built-in")
                        .to_string()
                };
                ui.label(egui::RichText::new(&self.status).small().color(theme.muted()))
                    .on_hover_text(format!(
                        "monospace: {}\nproportional: {}\nicons: {}\n\nOverride with                          $LODE_FONT_MONO, $LODE_FONT_UI, $LODE_FONT_SIZE.",
                        name(&self.fonts.mono),
                        name(&self.fonts.ui),
                        if self.fonts.nerd { "Nerd Font glyphs" } else { "words (no Nerd Font found)" },
                    ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(last) = self.log.last() {
                        let (col, glyph) = match last.0.as_str() {
                            "error" => (theme.error(), icon.fail),
                            "warn" => (theme.warning(), icon.fail),
                            _ => (theme.muted(), icon.ok),
                        };
                        ui.label(
                            egui::RichText::new(format!("{glyph}  {}", last.1))
                                .small()
                                .color(col),
                        );
                    }
                });
            });
        });
    }

    fn central(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        let theme = self.theme.clone();
        let icon = self.icon;
        let scale = self.scale;
        let frame = egui::Frame::new()
            .fill(theme.base())
            .inner_margin(egui::Margin::symmetric(12, 10));
        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            // tab strip
            ui.horizontal(|ui| {
                let mut select = None;
                for (i, t) in self.tabs.iter().enumerate() {
                    let on = i == self.active;
                    let dot = match t.state {
                        RunState::Running => icon.run,
                        RunState::Failed => icon.fail,
                        _ => icon.query,
                    };
                    if ui.selectable_label(on, format!("{dot}  {}", t.title)).clicked() {
                        select = Some(i);
                    }
                    if on && self.tabs.len() > 1 && ui.add(egui::Button::new(icon.close).small()).clicked() {
                        acts.push(Act::CloseTab(t.id));
                    }
                }
                if let Some(i) = select {
                    self.active = i;
                }
            });
            ui.add_space(8.0);

            let muted = theme.muted();
            let line_nr = theme.line_nr();
            let sel_bg = theme.selection_bg();
            let sel_fg = theme.selection_fg();
            let null_fg = theme.null_fg();
            let header_fg = theme.header_fg();
            let header_bg = theme.header_bg();
            let err_col = theme.error();
            let base = theme.base();
            let border = theme.border();
            let row_h = Theme::row_height(scale);

            let active = self.active;
            let Some(tab) = self.tabs.get_mut(active) else { return };

            // ── the editor ──────────────────────────────────────────────────
            let editor_h = (ui.available_height() * 0.32).clamp(80.0, 320.0);
            egui::Frame::new()
                .fill(base)
                .stroke(egui::Stroke::new(1.0_f32, border))
                .corner_radius(egui::CornerRadius::ZERO)
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("editor")
                        .max_height(editor_h)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut tab.sql)
                                    .code_editor()
                                    .frame(false)
                                    .desired_rows(5)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("SELECT …        Ctrl+Enter runs"),
                            );
                        });
                });

            ui.add_space(10.0);

            // ── the result header ───────────────────────────────────────────
            ui.horizontal(|ui| {
                match tab.state {
                    RunState::Idle => ui.label(egui::RichText::new("no result yet").small().color(muted)),
                    RunState::Running => ui.label(egui::RichText::new("running…").small().color(muted)),
                    RunState::Cancelled => ui.label(egui::RichText::new("cancelled").small().color(muted)),
                    RunState::Failed => ui.label(egui::RichText::new("failed").small().color(err_col)),
                    RunState::Done => {
                        let shown = tab.rows.len();
                        let text = match (tab.total, tab.affected) {
                            (_, Some(a)) if tab.columns.is_empty() => format!("{a} row(s) affected"),
                            (Some(t), _) => format!("{}–{} of {t}", tab.from + 1, tab.from + shown),
                            (None, _) => format!("{}–{} of ?", tab.from + 1, tab.from + shown),
                        };
                        ui.label(
                            egui::RichText::new(format!("{text}        {} ms", tab.ms))
                                .small()
                                .color(muted),
                        )
                    }
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_next = tab.state == RunState::Done && tab.has_more;
                    let can_prev = tab.state == RunState::Done && tab.from > 0;
                    if ui.add_enabled(can_next, egui::Button::new("Next  ▶")).clicked() {
                        acts.push(Act::Page(1));
                    }
                    if ui.add_enabled(can_prev, egui::Button::new("◀  Prev")).clicked() {
                        acts.push(Act::Page(-1));
                    }
                });
            });

            if let Some(e) = &tab.error {
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(base)
                    .stroke(egui::Stroke::new(1.0_f32, err_col))
                    .corner_radius(egui::CornerRadius::ZERO)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.colored_label(err_col, e);
                    });
            }

            // ── the grid ────────────────────────────────────────────────────
            if tab.columns.is_empty() || tab.rows.is_empty() {
                return;
            }
            ui.add_space(8.0);
            let ncols = tab.columns.len();
            let rows = &tab.rows;
            let columns = &tab.columns;
            let first_row = tab.from;
            let selected = tab.selected;

            egui::ScrollArea::horizontal().id_salt("grid").show(ui, |ui| {
                let mut builder = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(TCol::auto());
                for _ in 0..ncols {
                    builder = builder.column(TCol::initial(160.0).at_least(48.0).clip(true));
                }
                builder
                    .header(row_h + 6.0, |mut header| {
                        // The header strip is painted with the theme's own
                        // header surface: the column names have to read as a
                        // HEADER over the rows, and `header_fg` is the one ink
                        // the generator floored against `header_bg` rather than
                        // against the page.
                        header.col(|ui| {
                            fill(ui, header_bg);
                            pad(ui, false, |ui| {
                                ui.label(egui::RichText::new("#").small().color(header_fg));
                            });
                        });
                        for c in columns {
                            header.col(|ui| {
                                fill(ui, header_bg);
                                pad(ui, false, |ui| {
                                    ui.label(egui::RichText::new(&c.name).color(header_fg).strong())
                                        .on_hover_text(&c.type_name);
                                });
                            });
                        }
                    })
                    .body(|body| {
                        // Virtualised: egui_extras asks only for the rows on
                        // screen, so the loop cost does not grow with the page.
                        body.rows(row_h, rows.len(), |mut row| {
                            let i = row.index();
                            // NOT `row.set_selected`: that paints with egui's one
                            // selection colour, which this theme takes from
                            // `PmenuSel` — a LIST selection. Selected rows of data
                            // are selected TEXT, which the editor draws with
                            // `Visual`, so the grid paints its own.
                            let is_sel = selected == Some(i);
                            row.col(|ui| {
                                if is_sel {
                                    fill(ui, sel_bg);
                                }
                                pad(ui, true, |ui| {
                                    ui.label(
                                        egui::RichText::new((first_row + i + 1).to_string())
                                            .small()
                                            // A row number IS a line number, so it
                                            // takes the gutter's colour and is as
                                            // quiet here as it is in the editor.
                                            .color(if is_sel { sel_fg } else { line_nr }),
                                    );
                                });
                            });
                            for ci in 0..ncols {
                                row.col(|ui| {
                                    if is_sel {
                                        fill(ui, sel_bg);
                                    }
                                    let v = rows[i].get(ci);
                                    // Numbers right, text left: a column of
                                    // figures is read by its last digit, and a
                                    // left-ragged column of numbers cannot be
                                    // compared down the page at all.
                                    let numeric = matches!(v, Some(Value::Number(_)));
                                    pad(ui, numeric, |ui| match v.and_then(cell_text) {
                                        // A NULL is drawn differently from the
                                        // text "NULL", because they are
                                        // different values.
                                        None => {
                                            ui.label(egui::RichText::new("NULL").italics().color(null_fg));
                                        }
                                        Some(s) => {
                                            let one_line = s.replace('\n', "⏎");
                                            let mut t = egui::RichText::new(one_line).monospace();
                                            if is_sel {
                                                t = t.color(sel_fg);
                                            }
                                            ui.label(t).on_hover_text(s);
                                        }
                                    });
                                });
                            }
                            if row.response().clicked() {
                                acts.push(Act::SelectRow(i));
                            }
                        });
                    });
            });
        });
    }

    fn form_window(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        if self.form.is_none() {
            return;
        }
        let theme = self.theme.clone();
        let icon = self.icon;
        let nerd = self.fonts.nerd;
        let drivers: Vec<(String, String)> = self
            .hello
            .as_ref()
            .map(|h| h.drivers.iter().map(|d| (d.kind.clone(), d.display.clone())).collect())
            .unwrap_or_default();
        let meta = self.hello.as_ref().and_then(|h| {
            let kind = self.form.as_ref().map(|f| f.driver.clone())?;
            h.drivers.iter().find(|d| d.kind == kind).cloned()
        });
        // BEFORE anything is drawn: if the driver combo changed last frame, the
        // field set belongs to the previous driver.
        if let Some(f) = &mut self.form {
            f.sync_driver(meta.as_ref());
        }

        let Some(form) = &mut self.form else { return };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;

        let title = if form.editing.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(580.0)
            .frame(
                egui::Frame::new()
                    .fill(theme.overlay())
                    .stroke(egui::Stroke::new(1.0_f32, theme.border()))
                    .corner_radius(egui::CornerRadius::ZERO)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("formscroll")
                    .max_height(520.0)
                    .show(ui, |ui| {
                        // ── identity + the driver's own parameters ──────────
                        section(ui, &theme, "Connection");
                        egui::Grid::new("ident").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                            ui.label("Name");
                            ui.text_edit_singleline(&mut form.name);
                            ui.end_row();
                            ui.label("Driver");
                            egui::ComboBox::from_id_salt("driver")
                                .selected_text(
                                    drivers
                                        .iter()
                                        .find(|(k, _)| *k == form.driver)
                                        .map(|(_, d)| d.clone())
                                        .unwrap_or_else(|| form.driver.clone()),
                                )
                                .show_ui(ui, |ui| {
                                    for (kind, display) in &drivers {
                                        ui.selectable_value(&mut form.driver, kind.clone(), display);
                                    }
                                });
                            ui.end_row();

                            // The parameter fields come from the ENGINE's
                            // DriverMeta, not from a list here: that is what
                            // makes a new database type need no client change.
                            if let Some(m) = &meta {
                                for p in &m.params {
                                    let label = if p.required {
                                        format!("{} *", p.label)
                                    } else {
                                        p.label.clone()
                                    };
                                    ui.label(label);
                                    // `sync_driver` has already rebuilt the map
                                    // for this driver; this only covers a field
                                    // added to a DriverMeta between frames.
                                    let entry = form
                                        .params
                                        .entry(p.key.clone())
                                        .or_insert_with(|| p.default.clone().unwrap_or_default());
                                    if p.kind == "bool" {
                                        let mut on = entry == "true";
                                        if ui.checkbox(&mut on, "").changed() {
                                            *entry = on.to_string();
                                        }
                                    } else {
                                        ui.text_edit_singleline(entry);
                                    }
                                    ui.end_row();
                                }
                            }
                        });
                        stage_row(ui, &theme, icon, nerd, form, "endpoint", acts);

                        // ── auth ────────────────────────────────────────────
                        ui.add_space(10.0);
                        section(ui, &theme, "Authentication");
                        let auth_kinds: Vec<String> = meta
                            .as_ref()
                            .map(|m| m.auth.clone())
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| vec!["none".into(), "password".into()]);
                        egui::Grid::new("auth").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                            ui.label("Method");
                            egui::ComboBox::from_id_salt("authkind")
                                .selected_text(&form.auth_kind)
                                .show_ui(ui, |ui| {
                                    for a in &auth_kinds {
                                        ui.selectable_value(&mut form.auth_kind, a.clone(), a);
                                    }
                                });
                            ui.end_row();
                            match form.auth_kind.as_str() {
                                "password" => {
                                    ui.label("User");
                                    ui.text_edit_singleline(&mut form.auth_user);
                                    ui.end_row();
                                    ui.label("Password");
                                    ui.text_edit_singleline(&mut form.auth_secret);
                                    ui.end_row();
                                }
                                "client_cert" => {
                                    ui.label("User");
                                    ui.text_edit_singleline(&mut form.auth_user);
                                    ui.end_row();
                                    ui.label("Certificate");
                                    ui.text_edit_singleline(&mut form.auth_cert);
                                    ui.end_row();
                                    ui.label("Private key");
                                    ui.text_edit_singleline(&mut form.auth_key);
                                    ui.end_row();
                                    ui.label("Key password");
                                    ui.text_edit_singleline(&mut form.auth_key_password);
                                    ui.end_row();
                                }
                                "provider" => {
                                    ui.label("Provider");
                                    ui.text_edit_singleline(&mut form.auth_provider)
                                        .on_hover_text("aws | oauth | oidc — interpreted per driver");
                                    ui.end_row();
                                    ui.label("User");
                                    ui.text_edit_singleline(&mut form.auth_user);
                                    ui.end_row();
                                    ui.label("Token");
                                    ui.text_edit_singleline(&mut form.auth_secret);
                                    ui.end_row();
                                }
                                "kerberos" => {
                                    ui.label("Principal");
                                    ui.text_edit_singleline(&mut form.auth_principal);
                                    ui.end_row();
                                }
                                _ => {}
                            }
                        });
                        if form.auth_kind != "none" {
                            secret_note(ui, &theme);
                        }
                        stage_row(ui, &theme, icon, nerd, form, "auth", acts);

                        // ── tls ─────────────────────────────────────────────
                        ui.add_space(10.0);
                        section(ui, &theme, "TLS");
                        egui::Grid::new("tls").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                            ui.label("Mode");
                            egui::ComboBox::from_id_salt("tlsmode")
                                .selected_text(&form.tls_mode)
                                .show_ui(ui, |ui| {
                                    for (m, help) in [
                                        ("disable", "no encryption attempted — the only plaintext path"),
                                        ("prefer", "use TLS when the server offers it (the safe default)"),
                                        ("require", "encryption mandatory, certificate unverified"),
                                        ("verify_ca", "mandatory + the chain is verified"),
                                        ("verify_full", "mandatory + chain AND hostname verified"),
                                    ] {
                                        ui.selectable_value(&mut form.tls_mode, m.to_string(), m)
                                            .on_hover_text(help);
                                    }
                                });
                            ui.end_row();
                            ui.label("CA bundle");
                            ui.text_edit_singleline(&mut form.tls_ca);
                            ui.end_row();
                            ui.label("Client certificate");
                            ui.text_edit_singleline(&mut form.tls_client_cert);
                            ui.end_row();
                            ui.label("Client key");
                            ui.text_edit_singleline(&mut form.tls_client_key);
                            ui.end_row();
                        });
                        if form.tls_mode == "disable" {
                            ui.label(
                                egui::RichText::new(
                                    "the link will be PLAINTEXT — the engine warns on every connect",
                                )
                                .small()
                                .color(theme.warning()),
                            );
                        }
                        stage_row(ui, &theme, icon, nerd, form, "tls", acts);

                        // ── ssh tunnel ──────────────────────────────────────
                        ui.add_space(10.0);
                        section(ui, &theme, "SSH tunnel");
                        ui.checkbox(&mut form.tunnel_on, "Front this connection with an SSH tunnel")
                            .on_hover_text(
                                "The engine forwards the database's TCP endpoint through an SSH \
                                 direct-tcpip channel, so it works for drivers that have no TLS of \
                                 their own.",
                            );
                        if form.tunnel_on {
                            egui::Grid::new("tunnel").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                                ui.label("SSH host");
                                ui.text_edit_singleline(&mut form.tunnel_host);
                                ui.end_row();
                                ui.label("SSH port");
                                ui.text_edit_singleline(&mut form.tunnel_port);
                                ui.end_row();
                                ui.label("SSH user");
                                ui.text_edit_singleline(&mut form.tunnel_user);
                                ui.end_row();
                                ui.label("Method");
                                egui::ComboBox::from_id_salt("tunnelauth")
                                    .selected_text(&form.tunnel_auth)
                                    .show_ui(ui, |ui| {
                                        for (m, help) in [
                                            ("agent", "the running ssh-agent"),
                                            ("key", "a private key file"),
                                            ("password", "a password"),
                                        ] {
                                            ui.selectable_value(&mut form.tunnel_auth, m.to_string(), m)
                                                .on_hover_text(help);
                                        }
                                    });
                                ui.end_row();
                                match form.tunnel_auth.as_str() {
                                    "key" => {
                                        ui.label("Key file");
                                        ui.text_edit_singleline(&mut form.tunnel_key_path);
                                        ui.end_row();
                                        ui.label("Passphrase");
                                        ui.text_edit_singleline(&mut form.tunnel_secret);
                                        ui.end_row();
                                    }
                                    "password" => {
                                        ui.label("Password");
                                        ui.text_edit_singleline(&mut form.tunnel_secret);
                                        ui.end_row();
                                    }
                                    _ => {}
                                }
                            });
                            if form.tunnel_auth != "agent" {
                                secret_note(ui, &theme);
                            }
                            stage_row(ui, &theme, icon, nerd, form, "tunnel", acts);
                        }
                    });

                if let Some(e) = &form.error {
                    ui.add_space(6.0);
                    ui.colored_label(theme.error(), e);
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if nerd {
                        format!("{}  Save", icon.save)
                    } else {
                        "Save".to_string()
                    };
                    if ui
                        .add(
                            egui::Button::new(egui::RichText::new(label).color(theme.inverted()))
                                .fill(theme.accent()),
                        )
                        .clicked()
                    {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });

        if save {
            self.save_form();
        } else if cancel || !open {
            self.form = None;
        }
    }

    fn export_window(&mut self, ctx: &egui::Context) {
        if !self.export.open {
            return;
        }
        let theme = self.theme.clone();
        let nerd = self.fonts.nerd;
        let icon = self.icon;
        let mut open = true;
        let mut write = false;
        let mut close = false;
        let (ncols, nrows) = self
            .tabs
            .get(self.active)
            .map(|t| (t.columns.len(), t.rows.len()))
            .unwrap_or((0, 0));

        egui::Window::new("Export the loaded rows")
            .open(&mut open)
            .collapsible(false)
            .default_width(500.0)
            .frame(
                egui::Frame::new()
                    .fill(theme.overlay())
                    .stroke(egui::Stroke::new(1.0_f32, theme.border()))
                    .corner_radius(egui::CornerRadius::ZERO)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                ui.label(format!("{nrows} row(s), {ncols} column(s) — what the grid holds."));
                ui.label(
                    egui::RichText::new(
                        "Only the LOADED page is written. Paging further and exporting again writes \
                         that page instead — it does not append.",
                    )
                    .small()
                    .color(theme.muted()),
                );
                ui.add_space(10.0);
                egui::Grid::new("exportgrid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label("Format");
                    ui.horizontal(|ui| {
                        for f in [Format::Csv, Format::Tsv, Format::Json] {
                            if ui.selectable_label(self.export.format == f, f.label()).clicked() {
                                // Keep the extension honest when the format changes.
                                let old = self.export.format.extension();
                                self.export.format = f;
                                if self.export.path.ends_with(old) {
                                    let stem = self
                                        .export
                                        .path
                                        .trim_end_matches(old)
                                        .trim_end_matches('.')
                                        .to_string();
                                    self.export.path = format!("{stem}.{}", f.extension());
                                }
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("Path");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.export.path)
                            .desired_width(330.0)
                            .hint_text("result.csv"),
                    );
                    ui.end_row();
                });
                if let Some(m) = &self.export.message {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(m).small().color(theme.muted()));
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if nerd {
                        format!("{}  Write", icon.export)
                    } else {
                        "Write".to_string()
                    };
                    if ui
                        .add_enabled(
                            nrows > 0 && !self.export.path.is_empty(),
                            egui::Button::new(egui::RichText::new(label).color(theme.inverted()))
                                .fill(theme.accent()),
                        )
                        .clicked()
                    {
                        write = true;
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

        if write {
            let path = PathBuf::from(&self.export.path);
            let fmt = self.export.format;
            let msg = match self.tabs.get(self.active) {
                Some(t) => match export::write(&path, &t.columns, &t.rows, fmt) {
                    Ok(n) => format!("wrote {n} row(s) to {}", path.display()),
                    Err(e) => format!("could not write {}: {e}", path.display()),
                },
                None => "nothing to export".into(),
            };
            self.export.message = Some(msg.clone());
            self.status = msg;
        }
        self.export.open = open && !close;
    }
}

// ── small shared pieces ────────────────────────────────────────────────────────

/// A section heading: quiet, small, and the only thing that tells a panel's
/// regions apart once they all carry the same ink.
fn section(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    // `Title`, which is what the editor uses for a heading. `text.muted` would
    // be right for a label and wrong for the thing that names a region.
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .small()
            .strong()
            .color(theme.title()),
    );
}

/// The one line that has to be said wherever a credential is typed.
fn secret_note(ui: &mut egui::Ui, theme: &Theme) {
    ui.label(
        egui::RichText::new(
            "Stored as a TEMPLATE and resolved by the engine at connect time — {{ env \"PGPASSWORD\" }}, \
             {{ vault \"prod/db\" }}, {{ cmd \"…\" }}. A literal value is saved exactly as typed.",
        )
        .small()
        .color(theme.muted()),
    );
}

/// One stage's Test button and its last answer.
///
/// The engine dry-runs ONE layer per stage, which is the whole value of it: a
/// failure here names the layer that is wrong instead of handing back whatever
/// error the deepest layer happened to raise. The engine's message is shown
/// verbatim, because summarising it would throw away the only useful part.
fn stage_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    icon: Icons,
    nerd: bool,
    form: &ConnForm,
    stage: &'static str,
    acts: &mut Vec<Act>,
) {
    let help = STAGES.iter().find(|(s, _)| *s == stage).map(|(_, h)| *h).unwrap_or("");
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        let label = if nerd {
            format!("{}  Test", icon.test)
        } else {
            "Test".to_string()
        };
        if ui.button(label).on_hover_text(help).clicked() {
            acts.push(Act::Test(stage));
        }
        match form.tests.get(stage) {
            Some(TestState::Running) => {
                ui.label(egui::RichText::new("testing…").small().color(theme.muted()));
            }
            Some(TestState::Ok(detail, ms)) => {
                ui.label(
                    egui::RichText::new(format!("{}  {detail}  ({ms} ms)", icon.ok))
                        .small()
                        .color(theme.success()),
                );
            }
            Some(TestState::Failed(e)) => {
                ui.label(
                    egui::RichText::new(format!("{}  {e}", icon.fail))
                        .small()
                        .color(theme.error()),
                );
            }
            None => {
                ui.label(egui::RichText::new(help).small().color(theme.muted()));
            }
        }
    });
}

/// Paint a table cell's whole background. `max_rect` is the cell, expanded by
/// the table's item spacing so a header reads as one strip rather than a row of
/// chips with gaps between them.
fn fill(ui: &egui::Ui, color: egui::Color32) {
    let rect = ui.max_rect().expand2(ui.spacing().item_spacing);
    ui.painter().rect_filled(rect, 0.0, color);
}

/// Cell contents with breathing room, left or right aligned.
fn pad<R>(ui: &mut egui::Ui, right: bool, add: impl FnOnce(&mut egui::Ui) -> R) {
    let layout = if right {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.with_layout(layout, |ui| {
        ui.add_space(6.0);
        add(ui);
    });
}

/// One schema node and its children. Clicking a table puts a SELECT for it in
/// the editor — the fastest honest thing a tree click can do, and it leaves the
/// statement visible and editable instead of running something nobody read.
fn draw_node(ui: &mut egui::Ui, n: &Node, icon: &Icons, on_pick: &mut impl FnMut(String)) {
    let glyph = match n.kind.as_str() {
        "database" => icon.database,
        "schema" => icon.schema,
        "table" => icon.table,
        "view" => icon.view,
        "column" => icon.column,
        _ => icon.table,
    };
    if n.children.is_empty() {
        if ui
            .selectable_label(false, format!("{glyph}  {}", n.name))
            .on_hover_text(&n.kind)
            .clicked()
        {
            let qualified = match &n.schema {
                Some(s) if !s.is_empty() => format!("{s}.{}", n.name),
                _ => n.name.clone(),
            };
            on_pick(format!("SELECT * FROM {qualified} LIMIT 200;"));
        }
    } else {
        egui::CollapsingHeader::new(format!("{glyph}  {}", n.name))
            .id_salt(format!("{}-{}", n.kind, n.name))
            .show(ui, |ui| {
                for c in &n.children {
                    draw_node(ui, c, icon, on_pick);
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled_form() -> ConnForm {
        let mut f = ConnForm::blank("postgres".into());
        f.name = "prod".into();
        f.params.insert("host".into(), "db.example".into());
        f.params.insert("port".into(), "5432".into());
        f.params.insert("empty".into(), String::new());
        f.auth_kind = "password".into();
        f.auth_user = "app".into();
        f.auth_secret = "{{ vault \"prod/db\" }}".into();
        f.tls_mode = "verify_full".into();
        f.tls_ca = "/etc/ssl/ca.pem".into();
        f.tunnel_on = true;
        f.tunnel_host = "bastion.example".into();
        f.tunnel_port = "2222".into();
        f.tunnel_user = "jump".into();
        f.tunnel_auth = "key".into();
        f.tunnel_key_path = "~/.ssh/id_ed25519".into();
        f.tunnel_secret = "{{ env \"KEYPASS\" }}".into();
        f
    }

    /// The form must produce exactly the ConnSpec the engine deserialises —
    /// this is the shape `conn.test` and `conn.connect` both take.
    #[test]
    fn the_form_builds_the_engines_connspec() {
        let s = filled_form().spec();
        assert_eq!(s["driver"], "postgres");
        assert_eq!(s["params"]["host"], "db.example");
        assert!(s["params"].get("empty").is_none(), "empty params are left out");
        assert_eq!(s["auth"]["kind"], "password");
        assert_eq!(s["auth"]["user"], "app");
        assert_eq!(s["tls"]["mode"], "verify_full");
        assert_eq!(s["tls"]["ca"], "/etc/ssl/ca.pem");
        assert!(s["tls"].get("client_cert").is_none());
        assert_eq!(s["tunnel"]["host"], "bastion.example");
        assert_eq!(s["tunnel"]["port"], 2222);
        assert_eq!(s["tunnel"]["auth"]["kind"], "key");
        assert_eq!(s["tunnel"]["auth"]["path"], "~/.ssh/id_ed25519");
    }

    /// A secret typed into the form is stored as the TEMPLATE it was typed as.
    /// Nothing in the client resolves one, and nothing may rewrite one.
    #[test]
    fn secrets_stay_templates() {
        let s = filled_form().spec();
        assert_eq!(s["auth"]["password"], "{{ vault \"prod/db\" }}");
        assert_eq!(s["tunnel"]["auth"]["passphrase"], "{{ env \"KEYPASS\" }}");
    }

    #[test]
    fn no_tunnel_means_null_not_an_empty_object() {
        // TunnelSpec has required fields; an empty object would be a
        // deserialisation error on the engine, where `null` is "no tunnel".
        let mut f = filled_form();
        f.tunnel_on = false;
        assert_eq!(f.spec()["tunnel"], Value::Null);
    }

    #[test]
    fn the_stored_spec_drops_the_driver_but_keeps_everything_else() {
        // The store keeps the driver in its own column; `connect` puts it back.
        let f = filled_form();
        let stored = f.stored_spec();
        assert!(stored.get("driver").is_none());
        assert_eq!(stored["auth"]["user"], "app");
        assert_eq!(stored["tunnel"]["user"], "jump");
    }

    /// A round trip through what the store would hold: save the form, read it
    /// back into a form, and the spec must be identical. A field the editor
    /// writes and the loader drops is how a tunnel quietly disappears.
    #[test]
    fn a_saved_connection_reopens_identically() {
        let original = filled_form();
        let saved = SavedConnection {
            id: 1,
            name: original.name.clone(),
            driver: original.driver.clone(),
            spec: original.stored_spec(),
            created: None,
        };

        let mut reopened = ConnForm::blank("sqlite".into());
        // Mirrors App::open_form; kept in step by this test failing if it is not.
        reopened.name = saved.name.clone();
        reopened.driver = saved.driver.clone();
        if let Some(p) = saved.spec.get("params").and_then(Value::as_object) {
            for (k, v) in p {
                reopened
                    .params
                    .insert(k.clone(), v.as_str().map(str::to_string).unwrap_or_default());
            }
        }
        let a = saved.spec.get("auth").and_then(Value::as_object).unwrap();
        reopened.auth_kind = a["kind"].as_str().unwrap().into();
        reopened.auth_user = a["user"].as_str().unwrap().into();
        reopened.auth_secret = a["password"].as_str().unwrap().into();
        let t = saved.spec.get("tls").and_then(Value::as_object).unwrap();
        reopened.tls_mode = t["mode"].as_str().unwrap().into();
        reopened.tls_ca = t["ca"].as_str().unwrap().into();
        let n = saved.spec.get("tunnel").and_then(Value::as_object).unwrap();
        reopened.tunnel_on = true;
        reopened.tunnel_host = n["host"].as_str().unwrap().into();
        reopened.tunnel_port = n["port"].as_u64().unwrap().to_string();
        reopened.tunnel_user = n["user"].as_str().unwrap().into();
        let na = n["auth"].as_object().unwrap();
        reopened.tunnel_auth = na["kind"].as_str().unwrap().into();
        reopened.tunnel_key_path = na["path"].as_str().unwrap().into();
        reopened.tunnel_secret = na["passphrase"].as_str().unwrap().into();

        assert_eq!(original.spec(), reopened.spec());
    }

    /// Build a DriverMeta the way the engine sends one.
    fn meta(kind: &str, auth: &[&str], params: &[(&str, &str)]) -> DriverMeta {
        serde_json::from_value(json!({
            "kind": kind,
            "display": kind,
            "params": params.iter().map(|(k, d)| json!({
                "key": k, "label": k, "type": "string", "required": true,
                "secret": false, "default": d
            })).collect::<Vec<_>>(),
            "auth": auth,
            "caps": {}
        }))
        .unwrap()
    }

    /// The reported bug: pick Cassandra, then MongoDB, and the port stays 9042.
    /// `or_insert_with` only fills a key that is ABSENT, and nothing ever
    /// cleared the map, so every key two drivers share kept the first driver's
    /// value under the second driver's label.
    #[test]
    fn changing_the_driver_reloads_that_drivers_defaults() {
        let cassandra = meta("cassandra", &["none"], &[("host", "127.0.0.1"), ("port", "9042"), ("keyspace", "")]);
        let mongodb = meta("mongodb", &["none", "password"], &[("host", "127.0.0.1"), ("port", "27017"), ("database", "")]);

        let mut f = ConnForm::blank("cassandra".into());
        assert!(!f.sync_driver(Some(&cassandra)), "the opening driver is not a change");
        // What the form would hold after the user filled it in.
        f.params.insert("port".into(), "9042".into());
        f.params.insert("keyspace".into(), "events".into());
        f.params.insert("host".into(), "cass.internal".into());

        f.driver = "mongodb".into();
        assert!(f.sync_driver(Some(&mongodb)), "a real change resets");

        assert_eq!(f.params.get("port").map(String::as_str), Some("27017"), "the NEW driver's port");
        assert!(!f.params.contains_key("keyspace"), "a field mongodb has no idea about must go");
        assert!(f.params.contains_key("database"), "and mongodb's own field must appear");
        assert_eq!(
            f.params.get("host").map(String::as_str),
            Some("127.0.0.1"),
            "a shared key takes the new driver's default rather than carrying the old value"
        );
    }

    /// The auth method is the same staleness in another field.
    #[test]
    fn changing_the_driver_clamps_an_auth_method_it_does_not_offer() {
        let pg = meta("postgres", &["none", "password", "client_cert"], &[("host", "h")]);
        let sqlite = meta("sqlite", &["none"], &[("file", ":memory:")]);
        let mut f = ConnForm::blank("postgres".into());
        f.auth_kind = "client_cert".into();
        f.driver = "sqlite".into();
        f.sync_driver(Some(&sqlite));
        assert_eq!(f.auth_kind, "none", "sqlite offers nothing else");

        // …and a method the new driver DOES offer survives.
        let mut g = ConnForm::blank("sqlite".into());
        g.auth_kind = "none".into();
        g.driver = "postgres".into();
        g.sync_driver(Some(&pg));
        assert_eq!(g.auth_kind, "none");
    }

    /// A stale green from the previous driver is the same lie the endpoint stage
    /// was just fixed for.
    #[test]
    fn changing_the_driver_drops_the_old_test_results() {
        let a = meta("postgres", &["none"], &[("host", "h")]);
        let b = meta("mysql", &["none"], &[("host", "h")]);
        let mut f = ConnForm::blank("postgres".into());
        f.tests.insert("endpoint", TestState::Ok("reachable".into(), 3));
        let _ = a;
        f.driver = "mysql".into();
        f.sync_driver(Some(&b));
        assert!(f.tests.is_empty(), "those answers were about another database");
    }

    /// Opening a SAVED connection must not look like a driver change, or the
    /// stored parameters are thrown away before they are ever shown.
    #[test]
    fn opening_a_saved_connection_keeps_its_stored_values() {
        let mongodb = meta("mongodb", &["none"], &[("host", "127.0.0.1"), ("port", "27017")]);
        let mut f = ConnForm::blank("mongodb".into());
        // what App::open_form does for an edit
        f.driver = "mongodb".into();
        f.applied_driver = "mongodb".into();
        f.params.insert("host".into(), "db.internal".into());
        f.params.insert("port".into(), "31017".into());

        assert!(!f.sync_driver(Some(&mongodb)), "opening is not a change");
        assert_eq!(f.params.get("port").map(String::as_str), Some("31017"));
        assert_eq!(f.params.get("host").map(String::as_str), Some("db.internal"));
    }

    #[test]
    fn every_stage_the_form_offers_is_one_the_engine_knows() {
        // The engine rejects an unknown stage; these four are its whole set.
        let names: Vec<&str> = STAGES.iter().map(|(s, _)| *s).collect();
        assert_eq!(names, vec!["endpoint", "tunnel", "tls", "auth"]);
        for (_, help) in STAGES {
            assert!(!help.is_empty(), "a stage with no explanation is a mystery button");
        }
    }
}
