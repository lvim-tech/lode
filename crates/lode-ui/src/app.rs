//! The workspace view.
//!
//! Layout, left to right: an icon rail, a panel (connections and schema), and
//! the work area. Above the work area a row of result tabs, below it the
//! background-task list and a status strip. That arrangement is dbflux's and is
//! deliberate: the rail changes what the PANEL shows, not what is on the right.
//!
//! The thing this client exists to do, and the previous one did not: **clicking
//! a collection or a table opens a tab and shows its rows.** No statement is
//! typed, and none is invented here — the statement comes from the driver's own
//! `browse` template, which the engine sends in `rpc.hello`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{
    div, prelude::*, px, size, uniform_list, Bounds, Context, Entity, MouseButton, Rgba,
    SharedString, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowHandle, WindowOptions,
};
use lode_proto::{
    cell_text, DriverMeta, Engine, Hello, HistoryRow, Message, Node, QueryState, SavedConnection,
    SavedQuery,
};
use serde_json::{json, Value};

use crate::export::{self, Format};
use crate::form::ConnectionForm;
use crate::input::{InputColors, InputEvent, TextInput};
use crate::session::{self, Session, SessionTab};
use crate::theme::{self, Theme};
use crate::workspace::{
    fmt_duration, preview, type_badge, Live, ResultView, RunState, SubTab, Tab, Task,
};

/// Rows fetched per page.
const PAGE: usize = 200;
const BLOCKING: Duration = Duration::from_secs(15);

/// What the icon rail selects. It changes the PANEL, never the work area.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rail {
    Connections,
    /// Every statement this engine has run, from either client — the history is
    /// the ENGINE's, so the editor's runs are in here too.
    History,
    Tasks,
    Settings,
}

impl Rail {
    fn glyph(self) -> &'static str {
        match self {
            // Font Awesome codepoints, carried by every Nerd Font patch.
            Rail::Connections => "\u{f1c0}",
            Rail::History => "\u{f1da}",
            Rail::Tasks => "\u{f0ae}",
            Rail::Settings => "\u{f013}",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Rail::Connections => "Connections",
            Rail::History => "History",
            Rail::Tasks => "Tasks",
            Rail::Settings => "Settings",
        }
    }
}

/// What an outstanding request was for.
enum Pending {
    StoreStatus,
    Connections,
    Connect { name: String, driver: String },
    Disconnect { name: String },
    Structure { name: String },
    Execute { tab: u64, task: u64 },
    Page { tab: u64 },
    Ignore(&'static str),
    /// A console line, waiting to hear whether it destroys something.
    Classify { conn: String, statement: String },
    /// One `conn.test` stage asked from the connection window.
    FormTest { stage: &'static str, form: WeakEntity<ConnectionForm> },
    /// A connection saved from that window.
    FormSave,
    History,
    SavedQueries { conn: String },
}

pub struct Workspace {
    theme: Theme,
    theme_name: String,

    engine: Option<Engine>,
    hello: Option<Hello>,
    fatal: Option<String>,
    store_ok: bool,

    saved: Vec<SavedConnection>,
    conns: Vec<Live>,
    /// Which schema branches are open, keyed by connection + path.
    open_nodes: HashSet<String>,

    tabs: Vec<Tab>,
    active: usize,
    next_tab: u64,

    pending: HashMap<u64, Pending>,
    /// call_id → tab id.
    calls: HashMap<u64, u64>,
    /// call_id → task id, so a finished call closes its task.
    call_tasks: HashMap<u64, u64>,

    tasks: Vec<Task>,
    next_task: u64,
    tasks_open: bool,

    rail: Rail,
    status: String,
    log: Vec<(String, String)>,

    /// What was open last time, held until its connections come back: a tab can
    /// only be reopened once its connection has a schema again, which is several
    /// round trips after start.
    restore: Session,

    /// The sidebar's filter box.
    filter: Entity<TextInput>,
    /// Every field of every tab's query bar, keyed by (tab, field). The empty
    /// key is the statement box a typed driver gets instead of a form.
    inputs: HashMap<(u64, String), Entity<TextInput>>,

    /// The console: a line typed straight at a connection.
    console: Entity<TextInput>,
    console_open: bool,
    history: Vec<HistoryRow>,
    /// The export row, while it is open, and where it would write.
    export_open: bool,
    export_format: Format,
    export_path: Entity<TextInput>,
    /// The connection window, while one is open. A WINDOW, not a layer: the
    /// window manager owns it like any other.
    form_window: Option<WindowHandle<ConnectionForm>>,
    /// A statement the ENGINE called destructive, held with its reason until
    /// someone says yes. Nothing runs from here without that.
    confirm: Option<(String, String, String)>,
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (theme_name, theme) = theme::pick();
        let colors = input_colors(&theme);
        let filter = cx.new(|cx| TextInput::new(cx, "Filter connections and schema…", colors));
        cx.subscribe(&filter, |_this, _input, _ev: &InputEvent, cx| {
            // Typed into the filter: the panel is rebuilt from it each frame,
            // so there is nothing to do but ask for that frame.
            cx.notify();
        })
        .detach();
        let export_path = cx.new(|cx| TextInput::new(cx, "result.csv", colors));
        let console = cx.new(|cx| {
            TextInput::new(cx, "Type a command and press Enter — it runs against the open connection", colors)
        });
        cx.subscribe(&console, |this, input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Submit) {
                let line = input.read(cx).text().trim().to_string();
                if !line.is_empty() {
                    this.console_submit(line, cx);
                }
            }
            cx.notify();
        })
        .detach();
        let mut w = Workspace {
            theme,
            theme_name,
            engine: None,
            hello: None,
            fatal: None,
            store_ok: false,
            saved: Vec::new(),
            conns: Vec::new(),
            open_nodes: HashSet::new(),
            tabs: Vec::new(),
            active: 0,
            next_tab: 1,
            pending: HashMap::new(),
            calls: HashMap::new(),
            call_tasks: HashMap::new(),
            tasks: Vec::new(),
            next_task: 1,
            tasks_open: true,
            rail: Rail::Connections,
            status: "starting the engine…".into(),
            log: Vec::new(),
            restore: session::load(),
            filter,
            inputs: HashMap::new(),
            console,
            console_open: true,
            history: Vec::new(),
            export_open: false,
            export_format: Format::Csv,
            export_path,
            form_window: None,
            confirm: None,
        };

        w.rail = match w.restore.rail.as_str() {
            "history" => Rail::History,
            "tasks" => Rail::Tasks,
            "settings" => Rail::Settings,
            _ => Rail::Connections,
        };
        // The history panel is empty until it is asked for, and if that is the
        // panel being restored it should not open blank.
        let want_history = w.rail == Rail::History;

        match Engine::spawn(None) {
            Ok(engine) => match engine.hello(BLOCKING) {
                Ok(hello) => {
                    w.status = format!(
                        "{} {} — protocol {}, {} drivers",
                        hello.engine.as_deref().unwrap_or("engine"),
                        hello.version.as_deref().unwrap_or("?"),
                        hello.proto,
                        hello.drivers.len()
                    );
                    w.hello = Some(hello);
                    w.engine = Some(engine);
                    w.ask(Pending::StoreStatus, "store.status", json!({}));
                    w.ask(Pending::Connections, "store.connections.list", json!({}));
                    if want_history {
                        w.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                    }
                }
                Err(e) => w.fatal = Some(e.to_string()),
            },
            Err(e) => w.fatal = Some(e.to_string()),
        }

        // The engine answers on its own schedule, so the view polls it. A timer
        // rather than a blocking read: the window stays live while a query runs,
        // which is the whole point of the engine being another process.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(40)).await;
            if this.update(cx, |w: &mut Workspace, cx| w.pump(cx)).is_err() {
                break;
            }
        })
        .detach();

        w
    }

    // ── engine ───────────────────────────────────────────────────────────────

    fn ask(&mut self, what: Pending, method: &str, params: Value) {
        let Some(engine) = &self.engine else { return };
        match engine.send(method, params) {
            Ok(id) => {
                self.pending.insert(id, what);
            }
            Err(e) => self.fatal = Some(format!("lost the engine: {e}")),
        }
    }

    fn meta(&self, driver: &str) -> Option<&DriverMeta> {
        self.hello.as_ref()?.drivers.iter().find(|d| d.kind == driver)
    }

    fn live(&self, name: &str) -> Option<&Live> {
        self.conns.iter().find(|c| c.name == name)
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        loop {
            let Some(engine) = &self.engine else { break };
            let Some(msg) = engine.try_recv() else { break };
            changed = true;
            match msg {
                Message::Closed => {
                    self.fatal = Some(
                        "the engine exited. Its drivers run in a separate process, so this window \
                         survived it — restart to reconnect."
                            .into(),
                    );
                    self.engine = None;
                    self.conns.clear();
                    break;
                }
                Message::Notification { method, params } => self.on_notification(&method, params),
                Message::Response { id, result } => {
                    let Some(what) = self.pending.remove(&id) else { continue };
                    match result {
                        Ok(v) => self.on_result(what, v, cx),
                        Err(e) => self.on_error(what, e, cx),
                    }
                }
            }
        }
        // A running task's clock moves even when nothing arrived, so the
        // duration beside it is not frozen at the moment it started.
        if changed || self.tasks.iter().any(|t| t.running()) {
            cx.notify();
        }
    }

    fn on_notification(&mut self, method: &str, params: Value) {
        match method {
            "query.state" => {
                let Ok(st) = serde_json::from_value::<QueryState>(params) else { return };
                let Some(&tab_id) = self.calls.get(&st.call_id) else { return };
                let want_page = {
                    let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
                    tab.ms = st.ms;
                    match st.state.as_str() {
                        "done" => {
                            tab.state = RunState::Done;
                            tab.affected = st.affected;
                            tab.error = None;
                            true
                        }
                        "failed" => {
                            tab.state = RunState::Failed;
                            tab.error = st.error.clone();
                            false
                        }
                        "cancelled" => {
                            tab.state = RunState::Cancelled;
                            false
                        }
                        _ => false,
                    }
                };
                if let Some(&task_id) = self.call_tasks.get(&st.call_id) {
                    if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                        t.finish(Duration::from_millis(st.ms), st.error.clone());
                    }
                }
                if want_page {
                    let call_id = st.call_id;
                    self.ask(
                        Pending::Page { tab: tab_id },
                        "query.page",
                        json!({ "call_id": call_id, "from": 0, "n": PAGE }),
                    );
                }
                // Record it HERE, once it is over, with the state it reached and
                // the time it took. The history is the engine's, so what this
                // client runs shows up in the editor's log as well — and writing
                // it at launch instead would mean every row said "running".
                if matches!(st.state.as_str(), "done" | "failed" | "cancelled") {
                    let found = self
                        .tabs
                        .iter()
                        .find(|t| t.id == tab_id)
                        .map(|t| (t.sql.trim().to_string(), t.conn_name.clone(), t.driver.clone()));
                    if let Some((sql, conn, driver)) = found {
                        if !sql.is_empty() {
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
            }
            "daemon.log" => {
                let level = params.get("level").and_then(Value::as_str).unwrap_or("info").to_string();
                let message = params.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                self.log.push((level, message));
                if self.log.len() > 200 {
                    self.log.remove(0);
                }
            }
            _ => {}
        }
    }

    fn on_result(&mut self, what: Pending, v: Value, cx: &mut Context<Self>) {
        match what {
            Pending::StoreStatus => {
                self.store_ok = v.get("available").and_then(Value::as_bool).unwrap_or(false);
            }
            Pending::Connections => {
                self.saved =
                    serde_json::from_value(v.get("connections").cloned().unwrap_or(json!([]))).unwrap_or_default();
                // Nothing saved at all: the only useful thing a database client
                // can do on a first run is offer to describe a database. Shown
                // once, and only when the store is working — an unreachable
                // store is a different problem and a form would not fix it.
                if self.saved.is_empty() && self.store_ok && self.form_window.is_none() {
                    self.open_form(None, cx);
                }
                // Reopen what was open. Only now: before the saved list arrived
                // there was nothing to look a name up in.
                let want: Vec<String> = self
                    .restore
                    .connections
                    .iter()
                    .filter(|n| self.saved.iter().any(|s| &&s.name == n))
                    .cloned()
                    .collect();
                for name in want {
                    if !self.conns.iter().any(|c| c.name == name) {
                        self.connect(&name);
                    }
                }
            }
            Pending::Connect { name, driver } => {
                let conn_id = v.get("conn_id").and_then(Value::as_u64).unwrap_or(0);
                self.conns.retain(|c| c.name != name);
                self.conns.push(Live {
                    name: name.clone(),
                    driver,
                    conn_id,
                    encrypted: v.get("encrypted").and_then(Value::as_bool).unwrap_or(false),
                    tunneled: v.get("tunneled").and_then(Value::as_bool).unwrap_or(false),
                    nodes: Vec::new(),
                    queries: Vec::new(),
                });
                self.status = format!("connected: {name}");
                self.remember();
                self.ask(
                    Pending::Structure { name },
                    "schema.structure",
                    json!({ "conn_id": conn_id }),
                );
            }
            Pending::Disconnect { name } => {
                self.conns.retain(|c| c.name != name);
                self.status = format!("disconnected: {name}");
                self.remember();
            }
            Pending::Structure { name } => {
                let nodes: Vec<Node> =
                    serde_json::from_value(v.get("nodes").cloned().unwrap_or(json!([]))).unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == name) {
                    c.nodes = nodes;
                }
                self.reopen_tabs_for(&name, cx);
            }
            Pending::Execute { tab, task } => {
                let call_id = v.get("call_id").and_then(Value::as_u64).unwrap_or(0);
                self.calls.insert(call_id, tab);
                self.call_tasks.insert(call_id, task);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.call_id = Some(call_id);
                    t.state = RunState::Running;
                }
            }
            Pending::Page { tab } => {
                if !v.get("ready").and_then(Value::as_bool).unwrap_or(false) {
                    return;
                }
                let Ok(page) = serde_json::from_value::<lode_proto::Page>(v) else { return };
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.columns = page.columns;
                    t.rows = page.rows;
                    t.from = page.from;
                    t.has_more = page.has_more;
                    t.total = page.total;
                    t.affected = page.affected.or(t.affected);
                }
            }
            Pending::Ignore(_) => {}
            Pending::FormTest { stage, form } => {
                let detail = v.get("detail").and_then(Value::as_str).unwrap_or("ok").to_string();
                let ms = v.get("ms").and_then(Value::as_u64).unwrap_or(0);
                let _ = form.update(cx, |f, cx| {
                    f.test_answer(stage, Ok(format!("{detail} ({ms} ms)")), cx)
                });
            }
            Pending::History => {
                self.history =
                    serde_json::from_value(v.get("history").cloned().unwrap_or(json!([]))).unwrap_or_default();
            }
            Pending::SavedQueries { conn } => {
                let qs: Vec<SavedQuery> =
                    serde_json::from_value(v.get("queries").cloned().unwrap_or(json!([]))).unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == conn) {
                    c.queries = qs;
                }
            }
            Pending::FormSave => {
                self.ask(Pending::Connections, "store.connections.list", json!({}));
            }
            Pending::Classify { conn, statement } => {
                match v.get("reason").and_then(Value::as_str) {
                    // The engine named a reason, so it is held and shown. The
                    // reason is what makes someone stop; a yes/no box would not.
                    Some(reason) => self.confirm = Some((conn, statement, reason.to_string())),
                    None => {
                        self.run_statement(&conn, &statement, cx);
                        self.console.update(cx, |i, cx| i.set_text("", cx));
                    }
                }
            }
        }
    }

    fn on_error(&mut self, what: Pending, e: String, cx: &mut Context<Self>) {
        match what {
            Pending::Execute { tab, task } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.state = RunState::Failed;
                    t.error = Some(e.clone());
                }
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                    let d = t.duration();
                    t.finish(d, Some(e));
                }
            }
            Pending::Page { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.state = RunState::Failed;
                    t.error = Some(e);
                }
            }
            Pending::Connect { name, .. } => {
                self.status = format!("could not connect to {name}");
                self.log.push(("error".into(), format!("{name}: {e}")));
            }
            // A failed stage is the engine naming the layer that is wrong, so it
            // goes back to the window beside the button that asked.
            Pending::FormTest { stage, form } => {
                let _ = form.update(cx, |f, cx| f.test_answer(stage, Err(e), cx));
            }
            // A classification that failed is NOT permission to run: the answer
            // was "I do not know", and the whole point is to ask first.
            Pending::Classify { statement, .. } => self.log.push((
                "error".into(),
                format!("could not check whether this is destructive, so it was not run: {statement} — {e}"),
            )),
            _ => self.log.push(("error".into(), e)),
        }
    }

    // ── actions ──────────────────────────────────────────────────────────────

    fn connect(&mut self, name: &str) {
        let Some(c) = self.saved.iter().find(|c| c.name == name).cloned() else { return };
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

    fn disconnect(&mut self, name: &str) {
        let Some(live) = self.live(name) else { return };
        let (conn_id, name) = (live.conn_id, live.name.clone());
        self.ask(
            Pending::Disconnect { name },
            "conn.disconnect",
            json!({ "conn_id": conn_id }),
        );
    }

    /// Open a tab on an object and RUN it. This is the whole point of the
    /// client: the rows appear on a click, with no statement typed and none
    /// invented here — `browse_for` is the driver's own template.
    fn open_object(
        &mut self,
        conn_name: &str,
        name: &str,
        schema: Option<&str>,
        kind: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(live) = self.live(conn_name) else { return };
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let template = meta
            .as_ref()
            .map(|m| m.browse_for(kind).to_string())
            .unwrap_or_default();
        if template.is_empty() {
            self.log.push((
                "warn".into(),
                format!("the {driver} driver declares no browse statement (engine older than protocol 3)"),
            ));
            return;
        }
        let title = match schema.filter(|s| !s.is_empty()) {
            Some(s) => format!("{s}.{name}"),
            None => name.to_string(),
        };
        let id = self.next_tab;
        self.next_tab += 1;
        let mut tab = Tab::new(id, conn_name.to_string(), driver, title, meta.as_ref());
        tab.object = Some((name.to_string(), schema.map(str::to_string)));
        tab.kind = kind.to_string();
        // The browse template wins over the structured form for the first run:
        // a click asks to SEE the object, and the form is for narrowing it.
        tab.sql = lode_proto::render_template(&template, name, schema, PAGE, &[]);
        let statement = tab.sql.clone();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.make_inputs(id, meta.as_ref(), &statement, cx);
        self.run_tab(id);
        self.remember();
    }

    /// Build the query bar for a tab: the driver's declared fields where it has
    /// them, a statement box where it does not. Created once per tab, because an
    /// entity made during a render would be made again every frame.
    fn make_inputs(
        &mut self,
        tab_id: u64,
        meta: Option<&DriverMeta>,
        statement: &str,
        cx: &mut Context<Self>,
    ) {
        let colors = input_colors(&self.theme);
        let structured = meta.map(|m| m.is_structured()).unwrap_or(false);
        if structured {
            for f in &meta.unwrap().query_form {
                let (ph, default) = (f.placeholder.clone(), f.default.clone());
                let input = cx.new(|cx| {
                    let mut i = TextInput::new(cx, ph, colors);
                    i.set_text(default, cx);
                    i
                });
                self.subscribe_run(&input, tab_id, cx);
                self.inputs.insert((tab_id, f.key.clone()), input);
            }
        } else {
            let hint = meta.map(|m| m.hint.clone()).unwrap_or_default();
            let text = statement.to_string();
            let input = cx.new(|cx| {
                let mut i = TextInput::new(cx, hint, colors);
                i.set_text(text, cx);
                i
            });
            self.subscribe_run(&input, tab_id, cx);
            self.inputs.insert((tab_id, String::new()), input);
        }
    }

    /// Enter in any field of a tab's bar runs that tab.
    fn subscribe_run(&self, input: &Entity<TextInput>, tab_id: u64, cx: &mut Context<Self>) {
        cx.subscribe(input, move |this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Submit) {
                this.sync_tab_from_inputs(tab_id, cx);
                this.run_tab(tab_id);
            }
            cx.notify();
        })
        .detach();
    }

    /// Copy what was typed into the tab, then rebuild its statement. For a
    /// structured driver the statement is assembled from the engine's own
    /// template; for a typed one it IS what was typed.
    fn sync_tab_from_inputs(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let driver = self.tabs[idx].driver.clone();
        let meta = self.meta(&driver).cloned();
        let structured = meta.as_ref().map(|m| m.is_structured()).unwrap_or(false);
        if structured {
            let keys: Vec<String> = meta
                .as_ref()
                .unwrap()
                .query_form
                .iter()
                .map(|f| f.key.clone())
                .collect();
            for k in keys {
                if let Some(i) = self.inputs.get(&(tab_id, k.clone())) {
                    let v = i.read(cx).text().to_string();
                    self.tabs[idx].fields.insert(k, v);
                }
            }
            self.tabs[idx].sql = self.tabs[idx].statement(meta.as_ref(), PAGE);
        } else if let Some(i) = self.inputs.get(&(tab_id, String::new())) {
            self.tabs[idx].sql = i.read(cx).text().to_string();
        }
    }

    fn run_tab(&mut self, tab_id: u64) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let (conn_name, title) = (self.tabs[idx].conn_name.clone(), self.tabs[idx].title.clone());
        let Some(live) = self.live(&conn_name) else {
            self.log.push(("warn".into(), format!("{conn_name} is not connected")));
            return;
        };
        let conn_id = live.conn_id;
        let statement = self.tabs[idx].sql.clone();
        if statement.trim().is_empty() {
            return;
        }
        let old = self.tabs[idx].call_id.take();
        self.tabs[idx].reset_result();

        if let Some(old) = old {
            self.calls.remove(&old);
            self.ask(Pending::Ignore("release"), "query.release", json!({ "call_id": old }));
        }
        let task_id = self.next_task;
        self.next_task += 1;
        self.tasks.push(Task::new(task_id, format!("{conn_name} · {title}")));
        if self.tasks.len() > 100 {
            self.tasks.remove(0);
        }
        self.ask(
            Pending::Execute { tab: tab_id, task: task_id },
            "query.execute",
            json!({ "conn_id": conn_id, "statement": statement }),
        );
    }

    /// Reopen the remembered tabs that belong to a connection that has just
    /// come back. Taken from `restore` as they are opened, so a reconnect later
    /// in the session does not open them a second time.
    fn reopen_tabs_for(&mut self, conn: &str, cx: &mut Context<Self>) {
        let mine: Vec<SessionTab> = self
            .restore
            .tabs
            .iter()
            .filter(|t| t.conn == conn)
            .cloned()
            .collect();
        self.restore.tabs.retain(|t| t.conn != conn);
        for t in mine {
            self.open_object(&t.conn, &t.name, t.schema.as_deref(), &t.kind, cx);
            if let (Some(tab), false) = (self.tabs.last_mut(), t.view.is_empty()) {
                tab.view = match t.view.as_str() {
                    "tree" => ResultView::Tree,
                    "json" => ResultView::Json,
                    _ => ResultView::Table,
                };
            }
        }
        if self.restore.active < self.tabs.len() {
            self.active = self.restore.active;
        }
    }

    /// Remember what is open. Called after anything that changes it; writing a
    /// small file on each change is cheaper than deciding when to.
    fn remember(&self) {
        session::save(&Session {
            connections: self.conns.iter().map(|c| c.name.clone()).collect(),
            tabs: self
                .tabs
                .iter()
                .filter_map(|t| {
                    let (name, schema) = t.object.clone()?;
                    Some(SessionTab {
                        conn: t.conn_name.clone(),
                        name,
                        schema,
                        kind: t.kind.clone(),
                        view: t.view.label().to_lowercase(),
                    })
                })
                .collect(),
            active: self.active,
            theme: Some(self.theme_name.clone()),
            rail: match self.rail {
                Rail::Connections => "connections",
                Rail::History => "history",
                Rail::Tasks => "tasks",
                Rail::Settings => "settings",
            }
            .into(),
        });
    }

    fn close_tab(&mut self, id: u64) {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
            if let Some(call) = self.tabs[pos].call_id {
                self.calls.remove(&call);
                self.ask(Pending::Ignore("release"), "query.release", json!({ "call_id": call }));
            }
            self.tabs.remove(pos);
            self.inputs.retain(|(t, _), _| *t != id);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
            self.remember();
        }
    }

    /// Move the active tab's window by `pages`. The engine buffers the result,
    /// so paging backwards costs nothing and re-runs nothing.
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
            Pending::Page { tab: tab_id },
            "query.page",
            json!({ "call_id": call_id, "from": from, "n": PAGE }),
        );
    }

    /// A console line. It is NOT run yet: first the engine is asked whether it
    /// destroys anything, because only the engine knows what the statement means
    /// in that driver's language.
    fn console_submit(&mut self, line: String, _cx: &mut Context<Self>) {
        let Some(conn) = self.conns.first().map(|c| c.name.clone()) else {
            self.log.push(("warn".into(), "nothing is connected".into()));
            return;
        };
        let Some(live) = self.live(&conn) else { return };
        let driver = live.driver.clone();
        self.ask(
            Pending::Classify { conn, statement: line.clone() },
            "query.classify",
            json!({ "driver": driver, "statement": line }),
        );
    }

    fn run_statement(&mut self, conn: &str, statement: &str, cx: &mut Context<Self>) {
        let title: String = statement.chars().take(40).collect();
        self.run_statement_titled(conn, &title, statement, cx);
    }

    /// Run a statement at a connection, as its own tab — so it gets the same
    /// grid, views, paging and task entry everything else gets.
    fn run_statement_titled(&mut self, conn: &str, title: &str, statement: &str, cx: &mut Context<Self>) {
        let Some(live) = self.live(conn) else { return };
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let id = self.next_tab;
        self.next_tab += 1;
        let mut tab = Tab::new(id, conn.to_string(), driver, title.to_string(), meta.as_ref());
        tab.sql = statement.to_string();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        // A console statement is free text whatever the driver is: it was typed,
        // not assembled, so the tab gets a statement box rather than a form.
        let colors = input_colors(&self.theme);
        let text = statement.to_string();
        let input = cx.new(|cx| {
            let mut i = TextInput::new(cx, "", colors);
            i.set_text(text, cx);
            i
        });
        self.subscribe_run(&input, id, cx);
        self.inputs.insert((id, String::new()), input);
        self.run_tab(id);
    }

    /// Open the connection window. Editing passes what is stored; adding passes
    /// nothing and the driver's own defaults fill the fields.
    fn open_form(&mut self, existing: Option<(String, String, Value)>, cx: &mut Context<Self>) {
        // Already open: nothing is gained by a second one, and two would fight
        // over the same saved connection.
        if self.form_window.is_some() {
            return;
        }
        let theme = self.theme.clone();
        let drivers = self.hello.as_ref().map(|h| h.drivers.clone()).unwrap_or_default();
        let me = cx.entity().downgrade();
        let title = match &existing {
            Some((_, d, _)) => format!("lode — Edit {d} connection"),
            None => "lode — New connection".to_string(),
        };
        let bounds = Bounds::centered(None, size(px(720.0), px(560.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|cx| ConnectionForm::new(me, theme, drivers, existing, cx)),
        );
        match opened {
            Ok(h) => self.form_window = Some(h),
            Err(e) => self.log.push(("error".into(), format!("could not open the window: {e}"))),
        }
    }

    pub fn close_form(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.form_window.take() {
            let _ = h.update(cx, |_, window, _| window.remove_window());
        }
        cx.notify();
    }

    /// Run one `conn.test` stage for the connection window and send the answer
    /// back to it. The engine's message goes across untouched.
    pub fn form_test(
        &mut self,
        stage: &'static str,
        spec: Value,
        form: WeakEntity<ConnectionForm>,
        _cx: &mut Context<Self>,
    ) {
        self.ask(
            Pending::FormTest { stage, form },
            "conn.test",
            json!({ "stage": stage, "spec": spec }),
        );
    }

    pub fn form_save(
        &mut self,
        name: String,
        driver: String,
        spec: Value,
        renamed_from: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.ask(
            Pending::FormSave,
            "store.connections.save",
            json!({ "name": name, "driver": driver, "spec": spec }),
        );
        // A rename is a save under the new name and a removal of the old: the
        // store is keyed by name and has no rename of its own.
        if let Some(old) = renamed_from.filter(|o| *o != name) {
            self.ask(
                Pending::Ignore("rename"),
                "store.connections.remove",
                json!({ "name": old }),
            );
        }
        self.close_form(cx);
    }

    /// Save the active tab's statement under its title, for this connection.
    /// The engine owns saved queries, so one saved here appears in the editor.
    fn save_query(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let (conn, name) = (tab.conn_name.clone(), tab.title.clone());
        let id = tab.id;
        self.sync_tab_from_inputs(id, cx);
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else { return };
        let sql = tab.sql.clone();
        if sql.trim().is_empty() {
            return;
        }
        self.ask(
            Pending::SavedQueries { conn: conn.clone() },
            "store.queries.save",
            json!({ "conn": conn, "name": name, "sql": sql }),
        );
        // The save answers `{saved:true}`, not a list, so ask for the list.
        self.ask(
            Pending::SavedQueries { conn: conn.clone() },
            "store.queries.list",
            json!({ "conn": conn }),
        );
        self.status = format!("saved query: {name}");
    }

    /// Open a statement in its own tab on a connection — used by the history
    /// list and by a saved query.
    fn open_sql(&mut self, conn: &str, title: &str, sql: &str, cx: &mut Context<Self>) {
        if self.live(conn).is_none() {
            self.log
                .push(("warn".into(), format!("{conn} is not connected")));
            return;
        }
        self.run_statement_titled(conn, title, sql, cx);
    }

    /// Write what the grid holds. What is written is what is LOADED, and the row
    /// says so — an export that quietly paged the whole table in the background
    /// is a different feature with a different cost.
    fn write_export(&mut self, cx: &mut Context<Self>) {
        let path = self.export_path.read(cx).text().trim().to_string();
        if path.is_empty() {
            return;
        }
        let fmt = self.export_format;
        let msg = match self.tabs.get(self.active) {
            Some(t) if !t.rows.is_empty() => {
                match export::write(std::path::Path::new(&path), &t.columns, &t.rows, fmt) {
                    Ok(n) => format!("wrote {n} row(s) to {path}"),
                    Err(e) => format!("could not write {path}: {e}"),
                }
            }
            _ => "nothing loaded to export".to_string(),
        };
        self.status = msg;
    }

    fn cancel_active(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(call_id) = tab.call_id else { return };
        self.ask(Pending::Ignore("cancel"), "query.cancel", json!({ "call_id": call_id }));
    }

    fn clear_finished(&mut self) {
        self.tasks.retain(|t| t.running());
    }

    // ── painting ─────────────────────────────────────────────────────────────

    fn t(&self) -> &Theme {
        &self.theme
    }

    /// A label in the muted ink, which is most of the chrome.
    fn small(&self, text: impl Into<SharedString>, colour: Rgba) -> impl IntoElement {
        div().text_size(px(11.0)).text_color(colour).child(text.into())
    }

    fn section(&self, text: &str) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .text_size(px(10.0))
            .text_color(self.t().title())
            .child(SharedString::from(text.to_uppercase()))
    }

    fn rail_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let mut col = div()
            .flex()
            .flex_col()
            .w(px(44.0))
            .h_full()
            .bg(t.status())
            .border_r_1()
            .border_color(t.border())
            .py_2()
            .gap_1();
        for mode in [Rail::Connections, Rail::History, Rail::Tasks, Rail::Settings] {
            let on = self.rail == mode;
            col = col.child(
                div()
                    .id(SharedString::from(mode.label()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .h(px(36.0))
                    .text_size(px(15.0))
                    .text_color(if on { t.accent() } else { t.muted() })
                    .when(on, |d| d.bg(t.selected()))
                    .cursor_pointer()
                    .child(SharedString::from(mode.glyph()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.rail = mode;
                        if mode == Rail::History && this.history.is_empty() {
                            this.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                        }
                        this.remember();
                        cx.notify();
                    })),
            );
        }
        col
    }

    fn panel_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(280.0))
            .h_full()
            .bg(t.panel())
            .border_r_1()
            .border_color(t.border());

        match self.rail {
            Rail::Settings => {
                panel = panel.child(self.section("Theme")).child(
                    div()
                        .px_2()
                        .text_size(px(12.0))
                        .text_color(t.fg())
                        .child(SharedString::from(self.theme_name.clone())),
                );
                return panel;
            }
            Rail::History => {
                panel = panel.child(
                    div()
                        .id("history-reload")
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover()))
                        .text_size(px(11.0))
                        .text_color(t.accent())
                        .child("\u{f021}  Reload history")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                            cx.notify();
                        })),
                );
                panel = panel.child(self.section(&format!("History ({})", self.history.len())));
                if self.history.is_empty() {
                    panel = panel.child(
                        div().px_2().child(self.small("nothing yet — press reload", t.muted())),
                    );
                }
                for h in self.history.iter().take(100) {
                    let failed = h.state.as_deref() == Some("failed");
                    let conn = h.conn.clone().unwrap_or_default();
                    let stmt = h.statement.clone();
                    let one: String = stmt.replace('\n', " ").chars().take(70).collect();
                    panel = panel.child(
                        div()
                            .id(SharedString::from(format!("hist-{}", h.id)))
                            .flex()
                            .flex_col()
                            .px_2()
                            .py(px(2.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover()))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(if failed { t.error() } else { t.fg() })
                                    .child(SharedString::from(one)),
                            )
                            .child(self.small(
                                format!(
                                    "{} · {} ms{}",
                                    if conn.is_empty() { "—".into() } else { conn.clone() },
                                    h.ms.unwrap_or(0),
                                    h.rows.map(|r| format!(" · {r} row(s)")).unwrap_or_default()
                                ),
                                t.muted(),
                            ))
                            .on_click(cx.listener({
                                let (c, s) = (conn.clone(), stmt.clone());
                                move |this, _, _, cx| {
                                    let title: String = s.chars().take(40).collect();
                                    this.open_sql(&c, &title, &s, cx);
                                    cx.notify();
                                }
                            })),
                    );
                }
                return panel;
            }
            Rail::Tasks => {
                panel = panel.child(self.section(&format!("Tasks ({})", self.tasks.len())));
                for task in self.tasks.iter().rev().take(40) {
                    panel = panel.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_size(px(11.0))
                            .text_color(if task.failed() { t.error() } else { t.muted() })
                            .child(SharedString::from(format!(
                                "{} · {}",
                                task.label,
                                fmt_duration(task.duration())
                            ))),
                    );
                }
                return panel;
            }
            Rail::Connections => {}
        }

        panel = panel.child(div().px_2().py_1().child(self.filter.clone()));
        panel = panel.child(
            div()
                .id("new-conn")
                .px_2()
                .py_1()
                .cursor_pointer()
                .hover(|d| d.bg(t.hover()))
                .text_size(px(11.0))
                .text_color(t.accent())
                .child("\u{f067}  New connection")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_form(None, cx);
                    cx.notify();
                })),
        );
        let filter = self.filter.read(cx).text().to_string();
        let connected = self.conns.len();
        panel = panel.child(self.section(&format!(
            "Connections ({connected} connected · {} idle)",
            self.saved.len().saturating_sub(connected)
        )));

        for c in self
            .saved
            .iter()
            .filter(|c| crate::workspace::matches_filter(&c.name, &filter))
        {
            let is_live = self.conns.iter().any(|l| l.name == c.name);
            let name = c.name.clone();
            let name2 = c.name.clone();
            panel = panel.child(
                div()
                    .id(SharedString::from(format!("conn-{}", c.name)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover()))
                    .when(is_live, |d| d.bg(t.selected()))
                    .child(
                        div()
                            .text_size(px(9.0))
                            .text_color(if is_live { t.success() } else { t.muted() })
                            .child(SharedString::from(if is_live { "\u{f111}" } else { "\u{f10c}" })),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(if is_live { t.fg() } else { t.muted() })
                            .child(SharedString::from(c.name.clone())),
                    )
                    .child(self.small(c.driver.clone(), t.muted()))
                    .child(
                        div()
                            .id(SharedString::from(format!("edit-{}", c.name)))
                            .text_size(px(10.0))
                            .text_color(t.muted())
                            .child("\u{f044}")
                            .on_click(cx.listener({
                                let (n, d, sp) = (c.name.clone(), c.driver.clone(), c.spec.clone());
                                move |this, _, _, cx| {
                                    this.open_form(Some((n.clone(), d.clone(), sp.clone())), cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.conns.iter().any(|l| l.name == name) {
                            this.disconnect(&name);
                        } else {
                            this.connect(&name);
                        }
                        cx.notify();
                    })),
            );
            let _ = name2;
        }

        // The schema of every open connection, each under its own heading, so
        // two connections can be browsed at once — which is why a tab carries
        // its connection.
        for live in &self.conns {
            panel = panel.child(self.section(&live.name));
            if live.nodes.is_empty() {
                panel = panel.child(div().px_2().child(self.small("loading…", t.muted())));
            }
            if !live.queries.is_empty() {
                panel = panel.child(self.section("Saved queries"));
                for q in &live.queries {
                    panel = panel.child(
                        div()
                            .id(SharedString::from(format!("sq-{}-{}", live.name, q.name)))
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl(px(10.0))
                            .pr_2()
                            .py(px(2.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover()))
                            .child(div().text_size(px(10.0)).text_color(t.accent()).child("\u{f15c}"))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(t.fg())
                                    .child(SharedString::from(q.name.clone())),
                            )
                            .on_click(cx.listener({
                                let (c, n, s) = (live.name.clone(), q.name.clone(), q.sql.clone());
                                move |this, _, _, cx| {
                                    this.open_sql(&c, &n, &s, cx);
                                    cx.notify();
                                }
                            })),
                    );
                }
                panel = panel.child(self.section("Schema"));
            }
            for node in live
                .nodes
                .iter()
                .filter(|n| crate::workspace::node_matches(n, &filter))
            {
                panel = panel.child(self.node_el(&live.name, node, 0, cx));
            }
        }
        panel
    }

    /// One schema node. A branch toggles; a leaf OPENS and runs.
    fn node_el(&self, conn: &str, node: &Node, depth: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let key = format!("{conn}/{}/{}", node.kind, node.name);
        let is_branch = !node.children.is_empty();
        let open = self.open_nodes.contains(&key);
        let glyph = match node.kind.as_str() {
            "database" => "\u{f1c0}",
            "schema" => "\u{f07b}",
            "table" => "\u{f0ce}",
            "view" => "\u{f06e}",
            "collection" => "\u{f1c0}",
            _ => "\u{f0ce}",
        };

        let (conn_s, name_s, schema_s, kind_s, key_s) = (
            conn.to_string(),
            node.name.clone(),
            node.schema.clone(),
            node.kind.clone(),
            key.clone(),
        );

        let row = div()
            .id(SharedString::from(key.clone()))
            .flex()
            .items_center()
            .gap_1()
            .pl(px(8.0 + depth as f32 * 12.0))
            .pr_2()
            .py(px(2.0))
            .cursor_pointer()
            .hover(|d| d.bg(t.hover()))
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(t.muted())
                    .child(SharedString::from(if is_branch {
                        if open {
                            "\u{f0d7}"
                        } else {
                            "\u{f0da}"
                        }
                    } else {
                        " "
                    })),
            )
            .child(div().text_size(px(11.0)).text_color(t.accent()).child(SharedString::from(glyph)))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(t.fg())
                    .child(SharedString::from(node.name.clone())),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if is_branch {
                        if this.open_nodes.contains(&key_s) {
                            this.open_nodes.remove(&key_s);
                        } else {
                            this.open_nodes.insert(key_s.clone());
                        }
                    } else {
                        this.open_object(&conn_s, &name_s, schema_s.as_deref(), &kind_s, cx);
                    }
                    cx.notify();
                }),
            );

        let mut out = div().flex().flex_col().child(row);
        if is_branch && open {
            for child in &node.children {
                out = out.child(self.node_el(conn, child, depth + 1, cx));
            }
        }
        out.into_any_element()
    }

    fn tabs_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let mut strip = div()
            .flex()
            .items_center()
            .w_full()
            .h(px(30.0))
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border());

        for (i, tab) in self.tabs.iter().enumerate() {
            let on = i == self.active;
            let id = tab.id;
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("tab-{}", tab.id)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h_full()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(t.border())
                    .when(on, |d| d.bg(t.base()))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(t.accent())
                            // The tab carries its connection's driver, because
                            // tabs from different connections sit side by side.
                            .child(SharedString::from(driver_glyph(&tab.driver))),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(if on { t.fg() } else { t.muted() })
                            .child(SharedString::from(tab.title.clone())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("tabx-{}", tab.id)))
                            .text_size(px(10.0))
                            .text_color(t.muted())
                            .child("\u{f00d}")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_tab(id);
                                cx.notify();
                            })),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(p) = this.tabs.iter().position(|t| t.id == id) {
                                this.active = p;
                            }
                            cx.notify();
                        }),
                    ),
            );
        }
        strip
    }

    fn result_el(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let Some(tab) = self.tabs.get(self.active) else {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(div().text_color(t.muted()).child("Click a table or a collection to open it."))
                .child(self.small(
                    "Its rows appear here — the statement comes from the driver, not from you.",
                    t.muted(),
                ))
                .into_any_element();
        };

        let mut body = div().flex().flex_col().size_full();

        // ── breadcrumbs and numbers ─────────────────────────────────────────
        let count = match (tab.total, tab.rows.len()) {
            (Some(total), _) => format!("{total} row(s)"),
            (None, n) => format!("{n} row(s) loaded"),
        };
        let mut sub_row = div().flex().items_center().gap_3();
        for s in SubTab::all() {
            let on = tab.sub == s;
            let usable = s.implemented();
            sub_row = sub_row.child(
                div()
                    .text_size(px(11.0))
                    .text_color(if on {
                        t.fg()
                    } else if usable {
                        t.muted()
                    } else {
                        // Laid out but not built; shown so the row does not
                        // rearrange when they arrive.
                        t.border()
                    })
                    .child(SharedString::from(s.label())),
            );
        }

        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .px_3()
                .py_1()
                .bg(t.panel())
                .border_b_1()
                .border_color(t.border())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(self.small(tab.conn_name.clone(), t.muted()))
                        .child(self.small("›", t.border()))
                        .child(self.small(tab.title.clone(), t.fg()))
                        .child(self.small(count, t.muted()))
                        .child(self.small(format!("{} ms", tab.ms), t.muted())),
                )
                .child(sub_row),
        );

        // ── view switch ─────────────────────────────────────────────────────
        let mut views = div().flex().items_center().gap_2().px_3().py_1();
        for v in [ResultView::Tree, ResultView::Table, ResultView::Json] {
            let on = tab.view == v;
            let tab_id = tab.id;
            views = views.child(
                div()
                    .id(SharedString::from(format!("view-{}-{}", tab_id, v.label())))
                    .px_2()
                    .cursor_pointer()
                    .text_size(px(11.0))
                    .text_color(if on { t.inverted() } else { t.muted() })
                    .when(on, |d| d.bg(t.accent()))
                    .child(SharedString::from(v.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                            tb.view = v;
                        }
                        cx.notify();
                    })),
            );
        }
        let running = tab.state == RunState::Running;
        views = views.child(
            div()
                .id("cancel")
                .px_2()
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(if running { t.error() } else { t.border() })
                .child("Cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_active();
                    cx.notify();
                })),
        );
        views = views.child(
            div()
                .id("save-query")
                .px_2()
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(t.muted())
                .child("Save query")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.save_query(cx);
                    cx.notify();
                })),
        );
        let has_rows = !tab.rows.is_empty();
        views = views.child(
            div()
                .id("export-toggle")
                .px_2()
                .cursor_pointer()
                .text_size(px(11.0))
                .text_color(if has_rows { t.muted() } else { t.border() })
                .child("Export")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.export_open = !this.export_open;
                    cx.notify();
                })),
        );
        body = body.child(views);
        body = body.child(self.query_bar(tab, cx));
        if self.export_open {
            body = body.child(self.export_row(tab, cx));
        }

        if let Some(err) = &tab.error {
            body = body.child(
                div()
                    .m_3()
                    .p_2()
                    .border_1()
                    .border_color(t.error())
                    .text_size(px(12.0))
                    .text_color(t.error())
                    .child(SharedString::from(err.clone())),
            );
            return body.into_any_element();
        }
        if running {
            // No percentage: no driver reports one, and a bar that moved would
            // be inventing progress.
            body = body.child(div().p_3().child(self.small("running…", t.muted())));
            return body.into_any_element();
        }

        body = body.child(match tab.view {
            ResultView::Table => self.table_el(tab, cx),
            ResultView::Tree => self.tree_el(tab, cx),
            ResultView::Json => self.json_el(tab),
        });
        body.into_any_element()
    }

    /// The query bar: the driver's declared fields, or one statement box.
    ///
    /// Which one is NOT decided here — `is_structured` is true exactly when the
    /// engine sent a `query_form` for that driver. MongoDB sends four fields and
    /// the JSON each contributes; every SQL driver sends none and gets a box.
    fn query_bar(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let meta = self.meta(&tab.driver);
        let structured = meta.map(|m| m.is_structured()).unwrap_or(false);
        let tab_id = tab.id;

        let mut bar = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border());

        if structured {
            for f in &meta.unwrap().query_form {
                let Some(input) = self.inputs.get(&(tab_id, f.key.clone())) else { continue };
                bar = bar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(self.small(f.label.clone(), t.muted()))
                        // A limit box does not need the width a filter does.
                        .child(div().w(px(if f.key == "limit" { 60.0 } else { 220.0 })).child(input.clone())),
                );
            }
        } else if let Some(input) = self.inputs.get(&(tab_id, String::new())) {
            bar = bar.child(div().flex_1().child(input.clone()));
        }

        // The run button says what it does in this engine's words.
        let run_label = if structured { "Find" } else { "Run" };
        bar = bar.child(
            div()
                .id(SharedString::from(format!("run-{tab_id}")))
                .px_2()
                .cursor_pointer()
                .text_size(px(11.0))
                .bg(t.accent())
                .text_color(t.inverted())
                .child(SharedString::from(format!("{run_label} (↵)")))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sync_tab_from_inputs(tab_id, cx);
                    this.run_tab(tab_id);
                    cx.notify();
                })),
        );

        // Paging, beside what it pages.
        let can_prev = tab.state == RunState::Done && tab.from > 0;
        let can_next = tab.state == RunState::Done && tab.has_more;
        bar = bar.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .id("prev")
                        .px_2()
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(if can_prev { t.fg() } else { t.border() })
                        .child("◀")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page_by(-1);
                            cx.notify();
                        })),
                )
                .child(self.small(
                    format!("{}–{}", tab.from + 1, tab.from + tab.rows.len()),
                    t.muted(),
                ))
                .child(
                    div()
                        .id("next")
                        .px_2()
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(if can_next { t.fg() } else { t.border() })
                        .child("▶")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page_by(1);
                            cx.notify();
                        })),
                ),
        );
        bar.into_any_element()
    }

    /// The export row. It says what it writes — the LOADED rows — because an
    /// export that silently paged the whole table would be a different feature.
    fn export_row(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border())
            .child(self.small(
                format!("Export {} loaded row(s) as", tab.rows.len()),
                t.muted(),
            ));

        for f in [Format::Csv, Format::Tsv, Format::Json] {
            let on = self.export_format == f;
            row = row.child(
                div()
                    .id(SharedString::from(format!("fmt-{}", f.label())))
                    .px_1()
                    .cursor_pointer()
                    .text_size(px(10.0))
                    .text_color(if on { t.inverted() } else { t.muted() })
                    .when(on, |d| d.bg(t.accent()))
                    .child(SharedString::from(f.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Keep the extension honest when the format changes.
                        let old = this.export_format.extension();
                        this.export_format = f;
                        let path = this.export_path.read(cx).text().to_string();
                        if path.ends_with(old) {
                            let stem = path.trim_end_matches(old).trim_end_matches('.').to_string();
                            let next = format!("{stem}.{}", f.extension());
                            this.export_path.update(cx, |i, cx| i.set_text(next, cx));
                        }
                        cx.notify();
                    })),
            );
        }

        row.child(div().w(px(260.0)).child(self.export_path.clone())).child(
            div()
                .id("export-write")
                .px_2()
                .cursor_pointer()
                .text_size(px(11.0))
                .bg(t.accent())
                .text_color(t.inverted())
                .child("Write")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.write_export(cx);
                    cx.notify();
                })),
        )
        .into_any_element()
    }

    fn table_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let cols: Vec<(usize, String)> = tab
            .grid_columns()
            .into_iter()
            .map(|(i, c)| (i, c.name.clone()))
            .collect();
        if cols.is_empty() {
            return div().p_3().child(self.small("no columns", t.muted())).into_any_element();
        }

        let mut header = div()
            .flex()
            .w_full()
            .bg(t.header_bg())
            .border_b_1()
            .border_color(t.border())
            .child(div().w(px(50.0)).px_2().py_1().child(self.small("#", t.header_fg())));
        for (_, name) in &cols {
            header = header.child(
                div()
                    .w(px(170.0))
                    .px_2()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(t.header_fg())
                    .child(SharedString::from(name.clone())),
            );
        }

        let rows: Vec<Vec<Value>> = tab.rows.clone();
        let first = tab.from;
        let (muted, null_fg, row_alt, fg) = (t.muted(), t.null_fg(), t.row_alt(), t.fg());
        let cols2 = cols.clone();

        let list = uniform_list(
            "rows",
            rows.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, _cx| {
                let mut out = Vec::new();
                for i in range {
                    let mut line = div()
                        .flex()
                        .w_full()
                        // Alternating rows: the stripe is a surface from the
                        // theme, not a computed shade.
                        .when(i % 2 == 1, |d| d.bg(row_alt))
                        .child(
                            div()
                                .w(px(50.0))
                                .px_2()
                                .text_size(px(11.0))
                                .text_color(muted)
                                .child(SharedString::from((first + i + 1).to_string())),
                        );
                    for (ci, _) in &cols2 {
                        let cell = rows.get(i).and_then(|r| r.get(*ci));
                        let (text, colour) = match cell.and_then(cell_text) {
                            None => ("NULL".to_string(), null_fg),
                            Some(s) => (s.replace('\n', "⏎"), fg),
                        };
                        line = line.child(
                            div()
                                .w(px(170.0))
                                .px_2()
                                .text_size(px(12.0))
                                .text_color(colour)
                                .child(SharedString::from(text)),
                        );
                    }
                    out.push(line);
                }
                out
            }),
        )
        .h_full();

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(div().flex_1().child(list))
            .into_any_element()
    }

    /// The document view: one expandable record per row, with a type badge on
    /// every field. This is what a document store should open in, and what the
    /// previous client could not show at all.
    fn tree_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let mut out = div().flex().flex_col().size_full().overflow_hidden();
        for i in 0..tab.rows.len().min(200) {
            let doc = tab.document(i);
            let open = tab.expanded.contains(&i);
            let tab_id = tab.id;
            out = out.child(
                div()
                    .id(SharedString::from(format!("doc-{tab_id}-{i}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(2.0))
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover()))
                    .child(self.small(format!("{}", i + 1), t.line_nr()))
                    .child(self.small(if open { "\u{f0d7}" } else { "\u{f0da}" }, t.muted()))
                    .child(self.small(preview(&doc), t.fg()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                            if !tb.expanded.remove(&i) {
                                tb.expanded.insert(i);
                            }
                        }
                        cx.notify();
                    })),
            );
            if open {
                out = out.child(self.value_el(&doc, tab, 1));
            }
        }
        out.into_any_element()
    }

    /// One value inside a document, with its type beside it.
    fn value_el(&self, v: &Value, tab: &Tab, depth: usize) -> gpui::AnyElement {
        let t = self.t();
        let mut out = div().flex().flex_col();
        match v {
            Value::Object(m) => {
                for (k, val) in m {
                    let declared = tab
                        .columns
                        .iter()
                        .find(|c| &c.name == k)
                        .map(|c| c.type_name.as_str());
                    let badge = type_badge(val, declared);
                    out = out.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl(px(16.0 + depth as f32 * 14.0))
                            .py(px(1.0))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(t.muted())
                                    .child(SharedString::from(k.clone())),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(t.fg())
                                    .child(SharedString::from(preview(val))),
                            )
                            .child(
                                // The badge, in the colour its type maps to.
                                div()
                                    .text_size(px(9.0))
                                    .text_color(t.badge(badge))
                                    .child(SharedString::from(badge)),
                            ),
                    );
                    if matches!(val, Value::Object(_)) && depth < 4 {
                        out = out.child(self.value_el(val, tab, depth + 1));
                    }
                }
            }
            other => {
                out = out.child(
                    div()
                        .pl(px(16.0 + depth as f32 * 14.0))
                        .text_size(px(11.0))
                        .text_color(t.fg())
                        .child(SharedString::from(preview(other))),
                );
            }
        }
        out.into_any_element()
    }

    fn json_el(&self, tab: &Tab) -> gpui::AnyElement {
        let t = self.t();
        let docs: Vec<Value> = (0..tab.rows.len().min(200)).map(|i| tab.document(i)).collect();
        let text = serde_json::to_string_pretty(&docs).unwrap_or_else(|_| "[]".into());
        div()
            .size_full()
            .p_2()
            .text_size(px(11.0))
            .text_color(t.fg())
            .child(SharedString::from(text))
            .into_any_element()
    }

    fn tasks_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let failed = self.tasks.iter().filter(|x| x.failed()).count();
        let mut panel = div()
            .flex()
            .flex_col()
            .w_full()
            .bg(t.panel())
            .border_t_1()
            .border_color(t.border());

        panel = panel.child(
            div()
                .id("tasks-header")
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_1()
                .cursor_pointer()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(self.small("Background tasks", t.title()))
                        .child(self.small(
                            format!("{failed} failed"),
                            if failed > 0 { t.error() } else { t.muted() },
                        )),
                )
                .child(
                    div()
                        .id("clear-finished")
                        .text_size(px(11.0))
                        .text_color(t.muted())
                        .child("Clear finished")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_finished();
                            cx.notify();
                        })),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.tasks_open = !this.tasks_open;
                    cx.notify();
                })),
        );

        if self.tasks_open {
            for task in self.tasks.iter().rev().take(6) {
                panel = panel.child(
                    div()
                        .flex()
                        .flex_col()
                        .px_3()
                        .py(px(1.0))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(self.small(
                                    task.label.clone(),
                                    if task.failed() { t.error() } else { t.fg() },
                                ))
                                .child(self.small(fmt_duration(task.duration()), t.muted())),
                        )
                        // A failure keeps its WHOLE message, expanded: the point
                        // of keeping a failed task is that it is read.
                        .when_some(task.error.clone(), |d, e| {
                            d.child(
                                div()
                                    .pl_3()
                                    .text_size(px(10.0))
                                    .text_color(t.error())
                                    .child(SharedString::from(e)),
                            )
                        }),
                );
            }
        }
        panel
    }

    /// The console: a prompt named after what it runs against, one line, and —
    /// when the engine says the line destroys something — its reason and two
    /// buttons. Nothing runs from here without that answer.
    fn console_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let target = self
            .conns
            .first()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "nothing connected".into());

        let mut panel = div()
            .flex()
            .flex_col()
            .w_full()
            .bg(t.panel())
            .border_t_1()
            .border_color(t.border());

        panel = panel.child(
            div()
                .id("console-header")
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .cursor_pointer()
                .child(self.small("Command", t.title()))
                .child(self.small(
                    format!("runs against {target}, dangerous commands ask first"),
                    t.muted(),
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.console_open = !this.console_open;
                    cx.notify();
                })),
        );

        if self.console_open {
            panel = panel.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .pb_1()
                    .child(self.small(format!("{target}>"), t.accent()))
                    .child(div().flex_1().child(self.console.clone())),
            );

            // The confirmation. The REASON is the sentence, not a title bar.
            if let Some((_, statement, reason)) = &self.confirm {
                panel = panel.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .mx_3()
                        .mb_1()
                        .p_2()
                        .border_1()
                        .border_color(t.error())
                        .child(self.small(reason.clone(), t.error()))
                        .child(self.small(statement.clone(), t.fg()))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    div()
                                        .id("confirm-run")
                                        .px_2()
                                        .cursor_pointer()
                                        .text_size(px(11.0))
                                        .bg(t.error())
                                        .text_color(t.inverted())
                                        .child("Run it")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some((conn, stmt, _)) = this.confirm.take() {
                                                this.run_statement(&conn, &stmt, cx);
                                                this.console.update(cx, |i, cx| i.set_text("", cx));
                                            }
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("confirm-cancel")
                                        .px_2()
                                        .cursor_pointer()
                                        .text_size(px(11.0))
                                        .text_color(t.muted())
                                        .child("Cancel")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm = None;
                                            cx.notify();
                                        })),
                                ),
                        ),
                );
            }
        }
        panel
    }

    fn status_el(&self) -> impl IntoElement {
        let t = self.t();
        let failed = self.tasks.iter().filter(|x| x.failed()).count();
        let last = self
            .tasks
            .iter()
            .rev()
            .find(|x| !x.running())
            .map(|x| format!("{} · {}", x.label, fmt_duration(x.duration())))
            .unwrap_or_default();
        div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(22.0))
            .px_3()
            .bg(t.status())
            .border_t_1()
            .border_color(t.border())
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(self.small(self.status.clone(), t.muted()))
                    .child(self.small(last, t.muted())),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(self.small(
                        if self.store_ok { "store ok" } else { "no store" },
                        if self.store_ok { t.muted() } else { t.warning() },
                    ))
                    .child(self.small(
                        format!("{failed} failed"),
                        if failed > 0 { t.error() } else { t.muted() },
                    )),
            )
    }
}

/// A field's colours, taken from the theme like everything else. The selection
/// is `Visual` — selected TEXT — which is the same role the grid uses for a
/// selected row and a different one from a selected list entry.
fn input_colors(t: &Theme) -> InputColors {
    InputColors {
        text: t.fg(),
        placeholder: t.muted(),
        cursor: t.accent(),
        selection: t.selection_bg(),
        background: t.base(),
        border: t.border(),
    }
}

/// The glyph for a driver, by the shape of what it is rather than by a list of
/// names: the engine says whether it is relational, and that is the only
/// distinction a one-character icon can honestly carry.
fn driver_glyph(driver: &str) -> &'static str {
    match driver {
        "redis" => "\u{f1c0}",
        "mongodb" => "\u{f1c0}",
        _ => "\u{f1c0}",
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let root = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
            .font_family("CodeNerdFont")
            .text_size(px(13.0));

        if let Some(fatal) = self.fatal.clone() {
            return root.child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(div().text_color(t.error()).child("No engine"))
                    .child(div().text_color(t.fg()).child(SharedString::from(fatal)))
                    .child(self.small(
                        "Build it with scripts/build and put loded on $PATH, or set $LODE_DAEMON.",
                        t.muted(),
                    )),
            );
        }

        root.child(
            div()
                .flex()
                .flex_1()
                .w_full()
                .overflow_hidden()
                .child(self.rail_el(cx))
                .child(self.panel_el(cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .h_full()
                        .child(self.tabs_el(cx))
                        .child(div().flex_1().overflow_hidden().child(self.result_el(cx))),
                ),
        )
        .child(self.console_el(cx))
        .child(self.tasks_el(cx))
        .child(self.status_el())
    }
}
