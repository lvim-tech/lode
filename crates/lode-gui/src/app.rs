//! The application.
//!
//! One rule shapes everything here: **the UI thread never waits on a database.**
//! Every request is sent with `Engine::send`, which returns an id, and the
//! answer is picked up in a later frame from `Engine::try_recv`. A `pending`
//! map says what each outstanding id was for. There is no spinner that is
//! secretly a frozen window, and a query against a slow server leaves the
//! editor, the tree and the other tabs fully usable.
//!
//! The only blocking call in the whole client is the first `rpc.hello`, in
//! `App::new`, where there is nothing else to do yet and a failure has to be
//! reportable before a window exists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use egui_extras::{Column as TCol, TableBuilder};
use lode_proto::{cell_text, Column, DriverMeta, Engine, Hello, Message, Node, QueryState, SavedConnection};
use serde_json::{json, Value};

use crate::export::{self, Format};
use crate::theme::{self, Theme};

/// Rows fetched per page. The engine buffers the whole result and serves slices,
/// so this bounds what the grid holds, never what exists.
const PAGE: usize = 200;

/// Timeout for the few calls that are allowed to block (the handshake).
const BLOCKING: Duration = Duration::from_secs(15);

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
    StoreWrite(&'static str),
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

/// The add/edit connection form. Built entirely from the engine's `DriverMeta`,
/// so it has no field list of its own and grows a new database type for free.
struct ConnForm {
    name: String,
    driver: String,
    params: HashMap<String, String>,
    auth_kind: String,
    auth_user: String,
    /// A TEMPLATE, never a password. The engine resolves it at connect time.
    auth_secret: String,
    editing: Option<String>,
    error: Option<String>,
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
        let themes = theme::discover();
        // A remembered choice, else the first discovered theme, else built in.
        let want = std::env::var("LODE_THEME").ok();
        let (theme_name, theme) = pick_theme(&themes, want.as_deref());
        theme.apply(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let mut app = App {
            engine: None,
            fatal: None,
            hello: None,
            themes,
            theme_name,
            theme,
            theme_dirty: false,
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
            }
            "daemon.log" => {
                let level = params
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or("info")
                    .to_string();
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
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
                    t.from = page.from.max(from.min(page.from));
                    t.from = page.from;
                    t.has_more = page.has_more;
                    t.total = page.total;
                    t.affected = page.affected.or(t.affected);
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
            other => {
                let label = match other {
                    Pending::StoreStatus => "store.status",
                    Pending::Connections => "connections",
                    Pending::Disconnect => "disconnect",
                    Pending::Structure => "schema",
                    Pending::SavedQueries => "saved queries",
                    Pending::StoreWrite(w) => w,
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
        let conn_name = live.name.clone();
        let driver = live.driver.clone();
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

        // Free the previous result's buffer on the engine: a browsing session
        // would otherwise grow one buffer per run for the whole process.
        if let Some(old) = old_call {
            self.calls.remove(&old);
            self.ask(Pending::StoreWrite("release"), "query.release", json!({ "call_id": old }));
        }
        self.ask(
            Pending::Execute { tab: tab_id },
            "query.execute",
            json!({ "conn_id": conn_id, "statement": sql }),
        );
        // History is the engine's, so the editor client and the GUI share one log.
        self.ask(
            Pending::StoreWrite("history"),
            "store.history.record",
            json!({ "conn": conn_name, "driver": driver, "statement": sql, "state": "running" }),
        );
    }

    fn cancel_active(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(call_id) = tab.call_id else { return };
        self.ask(Pending::StoreWrite("cancel"), "query.cancel", json!({ "call_id": call_id }));
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

    fn save_form(&mut self) {
        let Some(form) = &self.form else { return };
        if form.name.trim().is_empty() {
            if let Some(f) = &mut self.form {
                f.error = Some("a connection needs a name".into());
            }
            return;
        }
        let mut params = serde_json::Map::new();
        for (k, v) in &form.params {
            if !v.is_empty() {
                params.insert(k.clone(), json!(v));
            }
        }
        let auth = match form.auth_kind.as_str() {
            "password" => json!({ "kind": "password", "user": form.auth_user, "password": form.auth_secret }),
            "provider" => json!({ "kind": "provider", "provider": "token", "user": form.auth_user, "token": form.auth_secret }),
            _ => json!({ "kind": "none" }),
        };
        let spec = json!({ "params": params, "auth": auth, "tls": { "mode": "prefer" } });
        let (name, driver) = (form.name.clone(), form.driver.clone());
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
        let mut form = ConnForm {
            name: String::new(),
            driver: first_driver,
            params: HashMap::new(),
            auth_kind: "none".into(),
            auth_user: String::new(),
            auth_secret: String::new(),
            editing: None,
            error: None,
        };
        if let Some(c) = edit {
            form.name = c.name.clone();
            form.driver = c.driver.clone();
            form.editing = Some(c.name.clone());
            if let Some(p) = c.spec.get("params").and_then(Value::as_object) {
                for (k, v) in p {
                    form.params
                        .insert(k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
                }
            }
            if let Some(a) = c.spec.get("auth").and_then(Value::as_object) {
                form.auth_kind = a
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("none")
                    .to_string();
                form.auth_user = a.get("user").and_then(Value::as_str).unwrap_or("").to_string();
                form.auth_secret = a
                    .get("password")
                    .or_else(|| a.get("token"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
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
    LoadQuery(String, String),
    SaveQuery,
    OpenExport,
    Refresh,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.theme_dirty {
            self.theme.apply(ctx);
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
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.heading("No engine");
                    ui.add_space(8.0);
                    ui.label(fatal);
                    ui.add_space(16.0);
                    ui.label(
                        "lode is the database engine this client talks to. Build it with \
                         `scripts/build` and put `loded` on $PATH, or set $LODE_DAEMON.",
                    );
                });
            });
            return;
        }

        let mut acts: Vec<Act> = Vec::new();
        self.top_bar(ctx, &mut acts);
        self.left_panel(ctx, &mut acts);
        self.bottom_bar(ctx);
        self.central(ctx, &mut acts);
        self.form_window(ctx);
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
                                    Pending::StoreWrite("release"),
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
                Act::LoadQuery(_conn, sql) => {
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
    fn top_bar(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong("lode");
                ui.separator();
                if ui.button("Run  (Ctrl+Enter)").clicked() {
                    acts.push(Act::Run);
                }
                let running = self
                    .tabs
                    .get(self.active)
                    .map(|t| t.state == RunState::Running)
                    .unwrap_or(false);
                if ui.add_enabled(running, egui::Button::new("Cancel")).clicked() {
                    acts.push(Act::Cancel);
                }
                ui.separator();
                if ui.button("New tab").clicked() {
                    acts.push(Act::NewTab);
                }
                if ui
                    .add_enabled(self.live.is_some(), egui::Button::new("Save query"))
                    .clicked()
                {
                    acts.push(Act::SaveQuery);
                }
                if ui.button("Export…").clicked() {
                    acts.push(Act::OpenExport);
                }
                ui.separator();
                if ui.button("Refresh").clicked() {
                    acts.push(Act::Refresh);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("⟳").on_hover_text("rescan theme files").clicked() {
                        acts.push(Act::ReloadThemes);
                    }
                    let mut chosen = self.theme_name.clone();
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(&chosen)
                        .show_ui(ui, |ui| {
                            for name in self.themes.keys() {
                                if ui.selectable_value(&mut chosen, name.clone(), name).clicked() {
                                    acts.push(Act::SetTheme(name.clone()));
                                }
                            }
                            if self.themes.is_empty() {
                                ui.label("no theme files found");
                            }
                        });
                    ui.label("Theme");
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
        egui::SidePanel::left("side")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Connections");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("+").on_hover_text("add a connection").clicked() {
                            acts.push(Act::New);
                        }
                    });
                });
                if !self.store_ok {
                    ui.colored_label(
                        self.theme.c(&self.theme.accent.warning, egui::Color32::YELLOW),
                        "the engine has no saved state — connections cannot be stored",
                    );
                }
                ui.separator();

                egui::ScrollArea::vertical()
                    .id_salt("conns")
                    .max_height(220.0)
                    .show(ui, |ui| {
                        if self.saved.is_empty() {
                            ui.label("none saved yet");
                        }
                        for c in &self.saved {
                            let is_live = self.live.as_ref().is_some_and(|l| l.name == c.name);
                            ui.horizontal(|ui| {
                                let label = if is_live {
                                    format!("● {}", c.name)
                                } else {
                                    format!("○ {}", c.name)
                                };
                                if ui
                                    .selectable_label(is_live, label)
                                    .on_hover_text(format!("{} — double-click to connect", c.driver))
                                    .double_clicked()
                                {
                                    acts.push(Act::Connect(c.name.clone()));
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.small_button("✕").on_hover_text("delete").clicked() {
                                        acts.push(Act::Delete(c.name.clone()));
                                    }
                                    if ui.small_button("✎").on_hover_text("edit").clicked() {
                                        acts.push(Act::Edit(c.name.clone()));
                                    }
                                    if !is_live && ui.small_button("▶").on_hover_text("connect").clicked() {
                                        acts.push(Act::Connect(c.name.clone()));
                                    }
                                });
                            });
                        }
                    });

                ui.separator();
                if let Some(live) = &self.live {
                    ui.horizontal(|ui| {
                        ui.strong(&live.name);
                        if ui.small_button("disconnect").clicked() {
                            acts.push(Act::Disconnect);
                        }
                    });
                    let lock = if live.encrypted {
                        if live.tunneled {
                            "🔒 encrypted (SSH tunnel)"
                        } else {
                            "🔒 encrypted (TLS)"
                        }
                    } else {
                        "⚠ PLAINTEXT"
                    };
                    ui.small(lock);
                    ui.separator();

                    if !live.queries.is_empty() {
                        ui.collapsing("Saved queries", |ui| {
                            for q in &live.queries {
                                if ui.selectable_label(false, &q.name).clicked() {
                                    acts.push(Act::LoadQuery(live.name.clone(), q.sql.clone()));
                                }
                            }
                        });
                    }

                    ui.strong("Schema");
                    egui::ScrollArea::vertical().id_salt("tree").show(ui, |ui| {
                        if live.nodes.is_empty() {
                            ui.label("…");
                        }
                        for n in &live.nodes {
                            draw_node(ui, n, &mut |sql| acts.push(Act::LoadQuery(String::new(), sql)));
                        }
                    });
                } else {
                    ui.label("not connected");
                }
            });
    }

    fn bottom_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.small(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(last) = self.log.last() {
                        let col = match last.0.as_str() {
                            "error" => self.theme.c(&self.theme.accent.error, egui::Color32::RED),
                            "warn" => self.theme.c(&self.theme.accent.warning, egui::Color32::YELLOW),
                            _ => self.theme.c(&self.theme.text.muted, egui::Color32::GRAY),
                        };
                        ui.colored_label(col, &last.1);
                    }
                });
            });
        });
    }

    fn central(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // tab strip
            ui.horizontal(|ui| {
                let mut select = None;
                for (i, t) in self.tabs.iter().enumerate() {
                    let on = i == self.active;
                    if ui.selectable_label(on, &t.title).clicked() {
                        select = Some(i);
                    }
                    if on && self.tabs.len() > 1 && ui.small_button("✕").clicked() {
                        acts.push(Act::CloseTab(t.id));
                    }
                }
                if let Some(i) = select {
                    self.active = i;
                }
            });
            ui.separator();

            let muted = self.theme.c(&self.theme.text.muted, egui::Color32::GRAY);
            let null_fg = self.theme.c(&self.theme.grid.null_fg, muted);
            let header_fg = self.theme.c(&self.theme.grid.header_fg, egui::Color32::WHITE);
            let err_col = self.theme.c(&self.theme.accent.error, egui::Color32::RED);

            let Some(tab) = self.tabs.get_mut(self.active) else { return };

            // editor
            let editor_h = (ui.available_height() * 0.35).max(90.0);
            egui::ScrollArea::vertical()
                .id_salt("editor")
                .max_height(editor_h)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut tab.sql)
                            .code_editor()
                            .desired_rows(6)
                            .desired_width(f32::INFINITY)
                            .hint_text("SELECT …    (Ctrl+Enter runs)"),
                    );
                });

            ui.separator();

            // result header
            ui.horizontal(|ui| {
                match tab.state {
                    RunState::Idle => ui.small("no result yet"),
                    RunState::Running => ui.small("running…"),
                    RunState::Cancelled => ui.small("cancelled"),
                    RunState::Failed => ui.colored_label(err_col, "failed"),
                    RunState::Done => {
                        let shown = tab.rows.len();
                        let text = match (tab.total, tab.affected) {
                            (_, Some(a)) if tab.columns.is_empty() => format!("{a} row(s) affected"),
                            (Some(t), _) => format!("{}–{} / {t}", tab.from + 1, tab.from + shown),
                            (None, _) => format!("{}–{} / ?", tab.from + 1, tab.from + shown),
                        };
                        ui.small(format!("{text}   ·   {} ms", tab.ms))
                    }
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_next = tab.state == RunState::Done && tab.has_more;
                    let can_prev = tab.state == RunState::Done && tab.from > 0;
                    if ui.add_enabled(can_next, egui::Button::new("next ▶")).clicked() {
                        acts.push(Act::Page(1));
                    }
                    if ui.add_enabled(can_prev, egui::Button::new("◀ prev")).clicked() {
                        acts.push(Act::Page(-1));
                    }
                });
            });

            if let Some(e) = &tab.error {
                ui.colored_label(err_col, e);
            }

            // the grid
            if tab.columns.is_empty() || tab.rows.is_empty() {
                return;
            }
            let ncols = tab.columns.len();
            let rows = &tab.rows;
            let columns = &tab.columns;
            let first_row = tab.from;

            egui::ScrollArea::horizontal().id_salt("grid").show(ui, |ui| {
                let mut builder = TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .column(TCol::auto());
                for _ in 0..ncols {
                    builder = builder.column(TCol::initial(150.0).at_least(40.0).clip(true));
                }
                builder
                    .header(22.0, |mut header| {
                        header.col(|ui| {
                            ui.colored_label(header_fg, "#");
                        });
                        for c in columns {
                            header.col(|ui| {
                                ui.colored_label(header_fg, &c.name).on_hover_text(&c.type_name);
                            });
                        }
                    })
                    .body(|body| {
                        // Virtualised: egui_extras asks only for the rows on
                        // screen, so the loop cost does not grow with the page.
                        body.rows(18.0, rows.len(), |mut row| {
                            let i = row.index();
                            row.col(|ui| {
                                ui.colored_label(muted, (first_row + i + 1).to_string());
                            });
                            for ci in 0..ncols {
                                row.col(|ui| {
                                    match rows[i].get(ci).and_then(cell_text) {
                                        // A NULL is drawn differently from the
                                        // text "NULL", because they are
                                        // different values.
                                        None => {
                                            ui.colored_label(null_fg, "NULL");
                                        }
                                        Some(s) => {
                                            let one_line = s.replace('\n', "⏎");
                                            ui.label(one_line).on_hover_text(s);
                                        }
                                    };
                                });
                            }
                        });
                    });
            });
        });
    }

    fn form_window(&mut self, ctx: &egui::Context) {
        let Some(form) = &mut self.form else { return };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let drivers: Vec<(String, String)> = self
            .hello
            .as_ref()
            .map(|h| h.drivers.iter().map(|d| (d.kind.clone(), d.display.clone())).collect())
            .unwrap_or_default();
        let meta = self
            .hello
            .as_ref()
            .and_then(|h| h.drivers.iter().find(|d| d.kind == form.driver))
            .cloned();

        egui::Window::new(if form.editing.is_some() {
            "Edit connection"
        } else {
            "New connection"
        })
        .open(&mut open)
        .resizable(true)
        .default_width(420.0)
        .show(ctx, |ui| {
            egui::Grid::new("form").num_columns(2).show(ui, |ui| {
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

                // The fields come from the ENGINE's DriverMeta, not from a list
                // here: that is what makes a new database type need no client
                // change at all.
                if let Some(m) = &meta {
                    for p in &m.params {
                        let label = if p.required {
                            format!("{} *", p.label)
                        } else {
                            p.label.clone()
                        };
                        ui.label(label);
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

                    ui.label("Auth");
                    egui::ComboBox::from_id_salt("auth")
                        .selected_text(&form.auth_kind)
                        .show_ui(ui, |ui| {
                            for a in &m.auth {
                                ui.selectable_value(&mut form.auth_kind, a.clone(), a);
                            }
                        });
                    ui.end_row();

                    if form.auth_kind != "none" {
                        ui.label("User");
                        ui.text_edit_singleline(&mut form.auth_user);
                        ui.end_row();
                        ui.label("Secret");
                        ui.text_edit_singleline(&mut form.auth_secret);
                        ui.end_row();
                    }
                }
            });

            if form.auth_kind != "none" {
                ui.small(
                    "The secret is stored as a TEMPLATE and resolved by the engine at connect time: \
                     {{ env \"PGPASSWORD\" }}, {{ vault \"prod/db\" }}, {{ cmd \"…\" }}. A literal \
                     value here is saved as typed.",
                );
            }
            if let Some(e) = &form.error {
                ui.colored_label(egui::Color32::from_rgb(0xd7, 0x5e, 0x52), e);
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
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
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(format!("{nrows} row(s), {ncols} column(s) — what the grid holds."));
                ui.small(
                    "Only the LOADED page is written. Paging further and exporting again appends \
                     nothing — it writes that page instead.",
                );
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Format");
                    for f in [Format::Csv, Format::Tsv, Format::Json] {
                        if ui
                            .selectable_label(self.export.format == f, f.label())
                            .clicked()
                        {
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
                ui.horizontal(|ui| {
                    ui.label("Path");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.export.path)
                            .desired_width(300.0)
                            .hint_text("result.csv"),
                    );
                });
                if let Some(m) = &self.export.message {
                    ui.label(m);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(nrows > 0 && !self.export.path.is_empty(), egui::Button::new("Write"))
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

/// One schema node and its children. Clicking a table puts a SELECT for it in
/// the editor — the fastest honest thing a tree click can do, and it leaves the
/// statement visible and editable instead of running something the user did not
/// read.
fn draw_node(ui: &mut egui::Ui, n: &Node, on_pick: &mut impl FnMut(String)) {
    if n.children.is_empty() {
        let label = format!("{}  ", n.name);
        if ui.selectable_label(false, label).on_hover_text(&n.kind).clicked() {
            let qualified = match &n.schema {
                Some(s) if !s.is_empty() => format!("{s}.{}", n.name),
                _ => n.name.clone(),
            };
            on_pick(format!("SELECT * FROM {qualified} LIMIT 200;"));
        }
    } else {
        egui::CollapsingHeader::new(&n.name)
            .id_salt(format!("{}-{}", n.kind, n.name))
            .show(ui, |ui| {
                for c in &n.children {
                    draw_node(ui, c, on_pick);
                }
            });
    }
}
