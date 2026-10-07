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

use std::time::Instant;

use gpui::{
    div, prelude::*, px, size, uniform_list, Bounds, ClipboardItem, Context, Entity, FocusHandle,
    MouseButton, MouseMoveEvent, MouseUpEvent, Rgba, SharedString, TitlebarOptions, WeakEntity,
    Window, WindowBounds, WindowHandle, WindowOptions,
};
use lode_proto::{
    cell_text, AuditRow, DriverMeta, Engine, Hello, HistoryRow, Index, Message, Node, QueryState,
    SavedChart, SavedConnection, SavedQuery, TableColumn,
};
use serde_json::{json, Value};

use crate::export::{self, Format};
use crate::form::ConnectionForm;
use crate::input::{InputColors, InputEvent, TextInput};
use crate::metrics::Metrics;
use crate::session::{self, Session, SessionTab};
use crate::theme::{self, Theme};
use crate::workspace::{
    bar_label, column_editable, copy_as_insert, copy_as_select, fuzzy_score, row_key,
    value_range, ChartShape, Cond,
    PendingWrite, Toast,
    fmt_duration, preview, type_badge, Live, ResultView, RunState, SubTab, Tab, Task,
};

gpui::actions!(
    lode,
    [
        /// Down a row, up a row — j and k, and the arrows, because a grid that
        /// only answers to vim keys is a grid half the people cannot drive.
        NextRow,
        PrevRow,
        /// Left and right move between TABS: h and l are the only pair spare,
        /// and moving between results is what this window is for.
        PrevTab,
        NextTab,
        /// Page down and up, ctrl-f and ctrl-b.
        PageDown,
        PageUp,
        /// First and last loaded row.
        FirstRow,
        LastRow,
        /// Open the row under the cursor in the detail panel, or shut it.
        ToggleDetail,
        /// The palette, and the way out of anything.
        TogglePalette,
        Escape,
        /// Run the active tab.
        RunActive,
        /// Move the palette's highlight without leaving the text field.
        PaletteDown,
        PaletteUp,
        PaletteAccept,
    ]
);

/// Bind the window's keys.
///
/// Every one of these is barred inside a text field (`!TextInput`), because a
/// `j` typed into a filter box has to be a `j`. The field's own bindings — the
/// arrows, home, end — stay as they are; these are what is left over when
/// nothing is being typed into.
pub fn bind_workspace_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    const OUTSIDE: Option<&str> = Some("Workspace && !TextInput");
    cx.bind_keys([
        KeyBinding::new("j", NextRow, OUTSIDE),
        KeyBinding::new("down", NextRow, OUTSIDE),
        KeyBinding::new("k", PrevRow, OUTSIDE),
        KeyBinding::new("up", PrevRow, OUTSIDE),
        KeyBinding::new("h", PrevTab, OUTSIDE),
        KeyBinding::new("l", NextTab, OUTSIDE),
        KeyBinding::new("ctrl-f", PageDown, OUTSIDE),
        KeyBinding::new("ctrl-b", PageUp, OUTSIDE),
        KeyBinding::new("g", FirstRow, OUTSIDE),
        KeyBinding::new("shift-g", LastRow, OUTSIDE),
        KeyBinding::new("enter", ToggleDetail, OUTSIDE),
        KeyBinding::new("r", RunActive, OUTSIDE),
        // The palette and escape work WHEREVER you are, including mid-word in a
        // filter box — being unable to leave is the one thing a modal must not do.
        KeyBinding::new("ctrl-p", TogglePalette, None),
        KeyBinding::new("escape", Escape, None),
        KeyBinding::new("ctrl-n", PaletteDown, Some("Palette")),
        KeyBinding::new("ctrl-j", PaletteDown, Some("Palette")),
        KeyBinding::new("ctrl-k", PaletteUp, Some("Palette")),
        KeyBinding::new("ctrl-y", PaletteAccept, Some("Palette")),
    ]);
}

/// One line in the command palette.
pub struct PaletteItem {
    pub label: String,
    pub hint: String,
    pub act: PaletteAct,
}

#[derive(Clone)]
pub enum PaletteAct {
    Rail(Rail),
    View(ResultView),
    GoTab(u64),
    Connect(String),
    Open { conn: String, object: String, schema: Option<String>, kind: String },
    SavedQuery { conn: String, sql: String, name: String },
    HistoryItem { conn: String, sql: String },
    RunActive,
    ToggleExport,
    SaveQuery,
    ToggleConsole,
    NewConnection,
}

/// Read a typed cell back into a JSON value.
///
/// `null` is the empty word for "no value", and it has to be typed exactly —
/// otherwise a column holding the TEXT "null" could never be written. The rest
/// follows what the cell held before: a number stays a number, and everything
/// else is text, because guessing that "007" is seven is how a product code
/// becomes an integer.
fn parse_cell(typed: &str, was: Option<&Value>) -> Value {
    if typed == "null" {
        return Value::Null;
    }
    match was {
        Some(Value::Number(_)) => match typed.parse::<i64>() {
            Ok(i) => json!(i),
            Err(_) => match typed.parse::<f64>() {
                Ok(f) => json!(f),
                // It was a number and this is not one. Send the text and let
                // the database refuse it — which it will, clearly, in its own
                // words, rather than this silently storing a zero.
                Err(_) => json!(typed),
            },
        },
        Some(Value::Bool(_)) => match typed {
            "true" => json!(true),
            "false" => json!(false),
            other => json!(other),
        },
        _ => json!(typed),
    }
}

/// Rows fetched per page.
const PAGE: usize = 200;

/// Grid metrics. A data grid wants DENSITY — the complaint about the first
/// version was that there was more air than data in it. 20px a row with 6px of
/// side padding fits a third again as many rows on a screen and still leaves a
/// line of space between them; 150px is a column that shows a short value whole
/// without wasting half a screen on an id.
// Was `const ROW_H: f32 = 20.0; const COL_W: f32 = 150.0;`. Both now come from
// the type scale: a row that does not grow with the text in it pinches, and the
// pinch is invisible in a screenshot taken at the old size. See `metrics.rs`.
const NUM_W: f32 = 44.0;
const BLOCKING: Duration = Duration::from_secs(15);

/// What the icon rail selects. It changes the PANEL, never the work area.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rail {
    Connections,
    /// What the server on the other end is doing. Reading this COSTS the server
    /// a round trip, which is why it is its own panel: it happens when it is
    /// being looked at and not otherwise.
    Instance,
    /// Every statement this engine has run, from either client — the history is
    /// the ENGINE's, so the editor's runs are in here too.
    History,
    /// What CHANGED, and what was stopped from changing. Different panel from
    /// History on purpose: history is every statement, most of them reads, and
    /// a refusal buried in a thousand SELECTs is a refusal nobody finds.
    Audit,
    Tasks,
    Settings,
}

impl Rail {
    fn glyph(self) -> &'static str {
        match self {
            // Font Awesome codepoints, carried by every Nerd Font patch.
            Rail::Connections => "\u{f1c0}",
            Rail::Instance => "\u{f080}",
            Rail::History => "\u{f1da}",
            Rail::Audit => "\u{f132}",
            Rail::Tasks => "\u{f0ae}",
            Rail::Settings => "\u{f013}",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Rail::Connections => "Connections",
            Rail::Instance => "Instance",
            Rail::History => "History",
            Rail::Audit => "Audit",
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
    Audit,
    Metrics { conn: String },
    SavedQueries { conn: String },
    SavedCharts { conn: String },
    /// The columns of a tab's object — where the identifying key comes from.
    Columns { tab: u64 },
    Indexes { tab: u64 },
    Ddl { tab: u64 },
    /// A write the engine has been asked to DESCRIBE, not to do.
    RowPlan { tab: u64, row: usize, key: Vec<(String, Value)>, set: Vec<(String, Value)>, delete: bool },
    /// The same write, confirmed.
    RowApply { tab: u64 },
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
    /// The history panel's own search box.
    history_filter: Entity<TextInput>,
    /// The engine's audit log, and how much of a statement it is keeping.
    ///
    /// `audit_mode` is shown to the reader: a blank statement column with no
    /// explanation reads as a broken client, and it is a setting on the engine.
    audit: Vec<AuditRow>,
    audit_mode: String,
    audit_filter: Entity<TextInput>,
    /// The type scale. One number; every size in the window comes off it.
    scale: Metrics,
    /// Which row's full reason is open. One at a time: a refusal's reason is a
    /// sentence, and a list where every row is three lines tall is a list
    /// nobody scans.
    audit_open: Option<i64>,

    /// What the server last said about itself, and which connection said it.
    metrics: Vec<lode_proto::Metric>,
    metrics_conn: Option<String>,
    metrics_error: Option<String>,
    /// How often to ask again, or None for "only when asked".
    ///
    /// None by DEFAULT. Repeating a statistics query against somebody's
    /// production database is a decision for the person looking at it, and a
    /// window that starts a timer on its own has made that decision for them.
    metrics_every: Option<Duration>,
    metrics_at: Option<Instant>,
    metrics_inflight: bool,

    /// The command palette, while it is open.
    palette_open: bool,
    palette_input: Entity<TextInput>,
    palette_sel: usize,

    /// Short-lived corner messages. Never the only place an error appears.
    toasts: Vec<Toast>,
    next_toast: u64,

    /// A column being dragged wider: (tab, column, where the drag began, the
    /// width it had then).
    resizing: Option<(u64, usize, f32, f32)>,

    /// The window's own focus, so the vim keys know whether a field has it.
    focus: FocusHandle,
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
        // Read ONCE: the window's fields and the type scale both come out of it.
        let restore = session::load();
        let export_path = cx.new(|cx| TextInput::new(cx, "result.csv", colors));
        let history_filter = cx.new(|cx| TextInput::new(cx, "Search history…", colors));
        let audit_filter = cx.new(|cx| TextInput::new(cx, "Search the audit log…", colors));
        let palette_input = cx.new(|cx| TextInput::new(cx, "Type to find a command…", colors));
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
            restore: restore.clone(),
            filter,
            inputs: HashMap::new(),
            console,
            console_open: true,
            history: Vec::new(),
            history_filter,
            audit: Vec::new(),
            audit_mode: "full".into(),
            audit_filter,
            audit_open: None,
            scale: Metrics::new(restore.font_size.unwrap_or(crate::metrics::DEFAULT_BASE)),
            metrics: Vec::new(),
            metrics_conn: None,
            metrics_error: None,
            metrics_every: None,
            metrics_at: None,
            metrics_inflight: false,
            palette_open: false,
            palette_input,
            palette_sel: 0,
            toasts: Vec::new(),
            next_toast: 1,
            resizing: None,
            focus: cx.focus_handle(),
            export_open: false,
            export_format: Format::Csv,
            export_path,
            form_window: None,
            confirm: None,
        };

        w.rail = match w.restore.rail.as_str() {
            "history" => Rail::History,
            "audit" => Rail::Audit,
            "instance" => Rail::Instance,
            "tasks" => Rail::Tasks,
            "settings" => Rail::Settings,
            _ => Rail::Connections,
        };
        // The history panel is empty until it is asked for, and if that is the
        // panel being restored it should not open blank.
        let want_history = w.rail == Rail::History;
        let want_audit = w.rail == Rail::Audit;

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
                    if want_audit {
                        w.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
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
        self.poll_metrics();
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
                    charts: Vec::new(),
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
            Pending::Metrics { conn } => {
                self.metrics =
                    serde_json::from_value(v.get("metrics").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                self.metrics_conn = Some(conn);
                self.metrics_error = None;
                self.metrics_at = Some(Instant::now());
                self.metrics_inflight = false;
            }
            Pending::History => {
                self.history =
                    serde_json::from_value(v.get("history").cloned().unwrap_or(json!([]))).unwrap_or_default();
            }
            Pending::Audit => {
                self.audit =
                    serde_json::from_value(v.get("audit").cloned().unwrap_or(json!([]))).unwrap_or_default();
                // An engine from before the audit log answers without a mode,
                // and the honest reading of that is that nothing is withheld.
                self.audit_mode = v
                    .get("mode")
                    .and_then(|m| m.as_str())
                    .unwrap_or("full")
                    .to_string();
            }
            Pending::SavedCharts { conn } => {
                let cs: Vec<SavedChart> =
                    serde_json::from_value(v.get("charts").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == conn) {
                    c.charts = cs;
                }
            }
            Pending::SavedQueries { conn } => {
                let qs: Vec<SavedQuery> =
                    serde_json::from_value(v.get("queries").cloned().unwrap_or(json!([]))).unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == conn) {
                    c.queries = qs;
                }
            }
            Pending::Columns { tab } => {
                let cols: Vec<TableColumn> =
                    serde_json::from_value(v.get("columns").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.meta_columns = cols;
                }
            }
            Pending::Indexes { tab } => {
                let idx: Vec<Index> =
                    serde_json::from_value(v.get("indexes").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.indexes = idx;
                }
            }
            Pending::Ddl { tab } => {
                let ddl = v.get("ddl").and_then(|d| d.as_str()).map(str::to_string);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.ddl = ddl;
                }
            }
            Pending::RowPlan { tab, row, key, set, delete } => {
                // The statement held here is the ENGINE's. A client that showed
                // its own guess and then sent something else would be asking
                // about one thing and doing another.
                let statement = v
                    .get("statement")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let reason = v.get("reason").and_then(|r| r.as_str()).map(str::to_string);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.pending_write = Some(PendingWrite { statement, reason, key, set, delete, row });
                    t.editing = None;
                }
            }
            Pending::RowApply { tab } => {
                let n = v.get("affected").and_then(|a| a.as_u64()).unwrap_or(0);
                self.toast(format!("{n} row(s) written"), false);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.pending_write = None;
                }
                // Read it back rather than patching the grid from what we sent:
                // a column with a default, a trigger or a type coercion means
                // the stored row is not always the row that was asked for.
                self.run_tab(tab);
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
                    t.finish(d, Some(e.clone()));
                }
                // The grid shows the error in full; the toast is so it is seen
                // when the eye is somewhere else.
                self.toast(e.lines().next().unwrap_or("failed").to_string(), true);
            }
            Pending::Page { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.state = RunState::Failed;
                    t.error = Some(e);
                }
            }
            Pending::Connect { name, .. } => {
                self.status = format!("could not connect to {name}");
                self.toast(format!("{name}: {e}"), true);
            }
            Pending::Metrics { conn } => {
                // The panel shows the refusal where the numbers would be. A
                // managed service withholding its statistics views is a fact
                // about that server, not a malfunction here.
                self.metrics.clear();
                self.metrics_conn = Some(conn);
                self.metrics_error = Some(e);
                self.metrics_at = Some(Instant::now());
                self.metrics_inflight = false;
            }
            Pending::RowPlan { tab, .. } | Pending::RowApply { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.pending_write = None;
                    t.editing = None;
                }
                // A refused write is the most important thing on the screen: it
                // is both the reason nothing happened and, usually, the reason
                // it should not have.
                self.toast(e, true);
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
        // The SAVED name goes with the spec so the engine's audit log can say
        // which connection a statement ran on. The engine does not use it to
        // connect; without it the log names the driver and leaves the name
        // empty, which reads as a gap rather than as a wrong answer.
        spec.insert("name".into(), json!(c.name.clone()));
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
        // What the object IS, asked once per tab. The identifying key comes
        // from here and nowhere else — without it a row cannot be addressed,
        // and a client that guessed a key from the columns it happened to get
        // back is a client that writes the wrong row.
        let objref = json!({ "name": name, "schema": schema });
        if let Some(conn_id) = self.live(conn_name).map(|l| l.conn_id) {
            self.ask(
                Pending::Columns { tab: id },
                "schema.columns",
                json!({ "conn_id": conn_id, "object": objref }),
            );
            if meta.as_ref().map_or(false, |m| m.caps.indexes) {
                self.ask(
                    Pending::Indexes { tab: id },
                    "schema.indexes",
                    json!({ "conn_id": conn_id, "object": objref }),
                );
            }
            if meta.as_ref().map_or(false, |m| m.caps.ddl) {
                self.ask(
                    Pending::Ddl { tab: id },
                    "schema.ddl",
                    json!({ "conn_id": conn_id, "object": objref }),
                );
            }
        }
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
        for f in meta.map(|m| m.browse_form.clone()).unwrap_or_default() {
            let (ph, def) = (f.placeholder.clone(), f.default.clone());
            let input = cx.new(|cx| {
                let mut i = TextInput::new(cx, &ph, colors);
                if !def.is_empty() {
                    i.set_text(def.clone(), cx);
                }
                i
            });
            self.inputs.insert((tab_id, format!("browse:{}", f.key)), input);
        }
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
            let typed = i.read(cx).text().to_string();
            // A browse with narrowing fields: read them, and rewrite the box
            // ONLY if nobody has typed in it since we last filled it.
            let browse_keys: Vec<String> = meta
                .as_ref()
                .map(|m| m.browse_form.iter().map(|f| f.key.clone()).collect())
                .unwrap_or_default();
            let object = self.tabs[idx].object.clone();
            if let (Some(m), Some((name, schema)), false) =
                (meta.as_ref(), object, browse_keys.is_empty())
            {
                for k in &browse_keys {
                    if let Some(f) = self.inputs.get(&(tab_id, format!("browse:{k}"))) {
                        let v = f.read(cx).text().to_string();
                        self.tabs[idx].fields.insert(k.clone(), v);
                    }
                }
                let kind = self.tabs[idx].kind.clone();
                let fresh = lode_proto::render_browse(
                    m,
                    &kind,
                    &self.tabs[idx].fields,
                    &name,
                    schema.as_deref(),
                    PAGE,
                );
                if typed == self.tabs[idx].rendered || typed.trim().is_empty() {
                    self.tabs[idx].sql = fresh.clone();
                    self.tabs[idx].rendered = fresh.clone();
                    i.update(cx, |inp, cx| inp.set_text(fresh, cx));
                    return;
                }
            }
            self.tabs[idx].sql = typed;
        }
    }

    fn run_tab(&mut self, tab_id: u64) {
        self.run_tab_confirmed(tab_id, false)
    }

    /// Run a tab, saying whether the person has already been asked.
    ///
    /// The `confirm` goes to the ENGINE, which is where the policy lives. This
    /// client asks first anyway; the flag is how it reports that it did, and the
    /// engine refuses when it did not.
    fn run_tab_confirmed(&mut self, tab_id: u64, confirm: bool) {
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
            json!({ "conn_id": conn_id, "statement": statement, "confirm": confirm }),
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
            if t.name.is_empty() {
                // A typed tab. It comes back with its words in the box and
                // NOTHING run: a statement someone was halfway through writing
                // is not a statement they asked for, and re-running it on a
                // restart is how a client deletes a table nobody asked it to.
                let title = if t.title.is_empty() { "Draft".into() } else { t.title.clone() };
                self.open_draft(&t.conn, &title, &t.draft, cx);
                continue;
            }
            self.open_object(&t.conn, &t.name, t.schema.as_deref(), &t.kind, cx);
            if let Some(tab) = self.tabs.last_mut() {
                if !t.view.is_empty() {
                    tab.view = match t.view.as_str() {
                        "tree" => ResultView::Tree,
                        "json" => ResultView::Json,
                        "chart" => ResultView::Chart,
                        _ => ResultView::Table,
                    };
                }
                tab.sub = match t.sub.as_str() {
                    "schema" => SubTab::Schema,
                    "aggregate" => SubTab::Aggregate,
                    "builder" => SubTab::Builder,
                    _ => SubTab::Documents,
                };
                tab.detail = t.detail;
            }
        }
        if self.restore.active < self.tabs.len() {
            self.active = self.restore.active;
        }
    }

    /// Open a tab holding a statement, without running it.
    fn open_draft(&mut self, conn: &str, title: &str, sql: &str, cx: &mut Context<Self>) {
        let Some(live) = self.live(conn) else { return };
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let id = self.next_tab;
        self.next_tab += 1;
        let mut tab = Tab::new(id, conn.to_string(), driver, title.to_string(), meta.as_ref());
        tab.sql = sql.to_string();
        self.make_inputs(id, meta.as_ref(), sql, cx);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    /// Remember what is open. Called after anything that changes it; writing a
    /// small file on each change is cheaper than deciding when to.
    fn remember(&self) {
        session::save(&Session {
            connections: self.conns.iter().map(|c| c.name.clone()).collect(),
            tabs: self
                .tabs
                .iter()
                .map(|t| {
                    let (name, schema) = t.object.clone().unzip();
                    SessionTab {
                        conn: t.conn_name.clone(),
                        name: name.unwrap_or_default(),
                        schema: schema.flatten(),
                        kind: t.kind.clone(),
                        view: t.view.label().to_lowercase(),
                        title: t.title.clone(),
                        draft: t.sql.clone(),
                        sub: t.sub.label().to_lowercase(),
                        detail: t.detail,
                    }
                })
                .collect(),
            active: self.active,
            theme: Some(self.theme_name.clone()),
            font_size: Some(self.scale.base()),
            rail: match self.rail {
                Rail::Connections => "connections",
                Rail::Instance => "instance",
                Rail::History => "history",
                Rail::Audit => "audit",
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
        self.run_statement_titled(conn, &title, statement, false, cx);
    }

    /// The same, for a statement the person has just been asked about and
    /// approved. The flag travels to the engine, which is what actually decides.
    fn run_statement_approved(&mut self, conn: &str, statement: &str, cx: &mut Context<Self>) {
        let title: String = statement.chars().take(40).collect();
        self.run_statement_titled(conn, &title, statement, true, cx);
    }

    /// Run a statement at a connection, as its own tab — so it gets the same
    /// grid, views, paging and task entry everything else gets.
    fn run_statement_titled(
        &mut self,
        conn: &str,
        title: &str,
        statement: &str,
        confirm: bool,
        cx: &mut Context<Self>,
    ) {
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
        // The approval travels with the run. Without it the engine refuses a
        // destructive statement that the person has just been asked about and
        // said yes to — the console would look broken, correctly but uselessly.
        self.run_tab_confirmed(id, confirm);
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
            |_window, cx| cx.new(|cx| ConnectionForm::new(me, theme, self.scale, drivers, existing, cx)),
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
        self.toast(format!("saved query: {name}"), false);
    }

    /// Open a statement in its own tab on a connection — used by the history
    /// list and by a saved query.
    fn open_sql(&mut self, conn: &str, title: &str, sql: &str, cx: &mut Context<Self>) {
        if self.live(conn).is_none() {
            self.log
                .push(("warn".into(), format!("{conn} is not connected")));
            return;
        }
        self.run_statement_titled(conn, title, sql, false, cx);
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

    /// Order the loaded page by a column; clicking the same one again reverses
    /// it. The page, not the result — see `sort_rows`.
    fn sort_by(&mut self, tab_id: u64, col: usize) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
        let ascending = match tab.sort {
            Some((c, asc)) if c == col => !asc,
            _ => true,
        };
        crate::workspace::sort_rows(&mut tab.rows, col, ascending);
        tab.sort = Some((col, ascending));
        tab.selected = None;
    }

    // ── keyboard ────────────────────────────────────────────────────────────

    /// Move the highlighted row, and keep it inside the rows that are loaded.
    /// `by` of 0 means "go to the end".
    fn move_row(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if tab.rows.is_empty() {
            return;
        }
        let last = tab.rows.len() - 1;
        let now = tab.selected.unwrap_or(0) as isize;
        tab.selected = Some(match by {
            isize::MAX => last,
            isize::MIN => 0,
            d => (now + d).clamp(0, last as isize) as usize,
        });
        // A detail panel that is open follows the cursor — it is showing "the
        // row", not "that row".
        if tab.detail.is_some() {
            tab.detail = tab.selected;
        }
        cx.notify();
    }

    fn step_tab(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        self.active = (((self.active as isize + by) % n + n) % n) as usize;
        self.remember();
        cx.notify();
    }

    // ── toasts ──────────────────────────────────────────────────────────────

    /// Say something briefly in the corner. It goes to the console log too —
    /// a toast expires, and an error that only ever appeared in one is an error
    /// nobody can go back and read.
    fn toast(&mut self, text: impl Into<String>, bad: bool) {
        let text = text.into();
        self.log
            .push((if bad { "error".into() } else { "info".into() }, text.clone()));
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, text, bad, born: Instant::now() });
    }

    // ── the palette ─────────────────────────────────────────────────────────

    /// Everything the palette can reach, before filtering.
    ///
    /// It is built fresh each time it is drawn, from the state as it is: there
    /// is no registry to forget to update, so a connection opened a second ago
    /// is in here, and one that was deleted is not.
    fn palette_items(&self) -> Vec<PaletteItem> {
        let mut out = Vec::new();
        for s in &self.saved {
            let live = self.live(&s.name).is_some();
            out.push(PaletteItem {
                label: format!("{} {}", if live { "Go to" } else { "Connect" }, s.name),
                hint: s.driver.clone(),
                act: PaletteAct::Connect(s.name.clone()),
            });
        }
        for c in &self.conns {
            for q in &c.queries {
                out.push(PaletteItem {
                    label: format!("Query: {}", q.name),
                    hint: c.name.clone(),
                    act: PaletteAct::SavedQuery {
                        conn: c.name.clone(),
                        sql: q.sql.clone(),
                        name: q.name.clone(),
                    },
                });
            }
            for n in &c.nodes {
                Self::palette_nodes(&c.name, n, None, &mut out);
            }
        }
        for h in self.history.iter().take(40) {
            let one: String = h.statement.replace('\n', " ").chars().take(60).collect();
            out.push(PaletteItem {
                label: format!("History: {one}"),
                hint: h.conn.clone().unwrap_or_default(),
                act: PaletteAct::HistoryItem {
                    conn: h.conn.clone().unwrap_or_default(),
                    sql: h.statement.clone(),
                },
            });
        }
        for (i, tb) in self.tabs.iter().enumerate() {
            out.push(PaletteItem {
                label: format!("Tab: {}", tb.title),
                hint: format!("{} · {}", tb.conn_name, i + 1),
                act: PaletteAct::GoTab(tb.id),
            });
        }
        for v in [ResultView::Tree, ResultView::Table, ResultView::Json, ResultView::Chart] {
            out.push(PaletteItem {
                label: format!("View as {}", v.label()),
                hint: "result".into(),
                act: PaletteAct::View(v),
            });
        }
        for r in [Rail::Connections, Rail::Instance, Rail::History, Rail::Audit, Rail::Tasks, Rail::Settings] {
            out.push(PaletteItem {
                label: format!("Show {}", r.label()),
                hint: "panel".into(),
                act: PaletteAct::Rail(r),
            });
        }
        out.push(PaletteItem { label: "Run this tab".into(), hint: "r".into(), act: PaletteAct::RunActive });
        out.push(PaletteItem { label: "Export loaded rows".into(), hint: "result".into(), act: PaletteAct::ToggleExport });
        out.push(PaletteItem { label: "Save this query".into(), hint: "result".into(), act: PaletteAct::SaveQuery });
        out.push(PaletteItem { label: "Toggle console".into(), hint: "window".into(), act: PaletteAct::ToggleConsole });
        out.push(PaletteItem { label: "New connection".into(), hint: "window".into(), act: PaletteAct::NewConnection });
        out
    }

    fn palette_nodes(conn: &str, node: &Node, schema: Option<&str>, out: &mut Vec<PaletteItem>) {
        let here = if node.kind == "schema" || node.kind == "database" {
            Some(node.name.as_str())
        } else {
            schema
        };
        if !matches!(node.kind.as_str(), "schema" | "database" | "folder" | "group") {
            out.push(PaletteItem {
                label: format!("Open {}", node.name),
                hint: format!("{conn} · {}", node.kind),
                act: PaletteAct::Open {
                    conn: conn.to_string(),
                    object: node.name.clone(),
                    schema: here.map(|s| s.to_string()),
                    kind: node.kind.clone(),
                },
            });
        }
        for c in &node.children {
            Self::palette_nodes(conn, c, here, out);
        }
    }

    /// The items that match what has been typed, best first.
    fn palette_matches(&self, cx: &gpui::App) -> Vec<(usize, PaletteItem)> {
        let needle = self.palette_input.read(cx).text().to_string();
        let mut scored: Vec<(i32, usize, PaletteItem)> = self
            .palette_items()
            .into_iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let s = fuzzy_score(&it.label, &needle)
                    .or_else(|| fuzzy_score(&it.hint, &needle).map(|s| s + 50))?;
                Some((s, i, it))
            })
            .collect();
        scored.sort_by_key(|(s, i, _)| (*s, *i));
        scored.into_iter().take(60).map(|(_, i, it)| (i, it)).collect()
    }

    fn palette_run(&mut self, act: PaletteAct, cx: &mut Context<Self>) {
        self.palette_open = false;
        match act {
            PaletteAct::Rail(r) => {
                self.rail = r;
                if r == Rail::History && self.history.is_empty() {
                    self.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                }
                if r == Rail::Audit && self.audit.is_empty() {
                    self.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
                }
                self.remember();
            }
            PaletteAct::View(v) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.view = v;
                }
            }
            PaletteAct::GoTab(id) => {
                if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
                    self.active = i;
                    self.remember();
                }
            }
            PaletteAct::Connect(name) => {
                if let Some(i) = self.tabs.iter().position(|t| t.conn_name == name) {
                    self.active = i;
                } else {
                    self.connect(&name);
                }
            }
            PaletteAct::Open { conn, object, schema, kind } => {
                self.open_object(&conn, &object, schema.as_deref(), &kind, cx)
            }
            PaletteAct::SavedQuery { conn, sql, name } => self.open_sql(&conn, &name, &sql, cx),
            PaletteAct::HistoryItem { conn, sql } => {
                let title: String = sql.chars().take(40).collect();
                self.open_sql(&conn, &title, &sql, cx);
            }
            PaletteAct::RunActive => {
                if let Some(id) = self.tabs.get(self.active).map(|t| t.id) {
                    self.run_tab(id);
                }
            }
            PaletteAct::ToggleExport => self.export_open = !self.export_open,
            PaletteAct::SaveQuery => self.save_query(cx),
            PaletteAct::ToggleConsole => self.console_open = !self.console_open,
            PaletteAct::NewConnection => self.open_form(None, cx),
        }
    }

    // ── the clipboard ───────────────────────────────────────────────────────

    /// Put the selected row on the clipboard as a statement. See
    /// `copy_as_select` for why generating SQL is allowed HERE and nowhere else.
    fn copy_row(&mut self, as_insert: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(i) = tab.selected.or(tab.detail) else {
            self.toast("pick a row first", true);
            return;
        };
        let Some(row) = tab.rows.get(i) else { return };
        let table = tab
            .object
            .as_ref()
            .map(|(name, schema)| match schema {
                Some(s) => format!("{s}.{name}"),
                None => name.clone(),
            })
            .unwrap_or_else(|| tab.title.clone());
        let sql = if as_insert {
            copy_as_insert(&table, &tab.columns, row)
        } else {
            copy_as_select(&table, &tab.columns, row)
        };
        cx.write_to_clipboard(ClipboardItem::new_string(sql));
        self.toast(
            if as_insert { "row copied as INSERT" } else { "row copied as SELECT" },
            false,
        );
    }

    // ── saved charts ────────────────────────────────────────────────────────

    /// Open a saved chart: its statement in a tab, shown as a chart, measuring
    /// the column it was saved measuring.
    ///
    /// The measure is matched BY NAME once the result arrives, in `on_result` —
    /// if the statement has changed and that column is gone, the chart falls
    /// back to the first measurement rather than drawing a different column
    /// under the old name.
    fn open_chart(&mut self, conn: &str, name: &str, sql: &str, measure: &str, cx: &mut Context<Self>) {
        if self.live(conn).is_none() {
            self.toast(format!("{conn} is not connected"), true);
            return;
        }
        self.run_statement_titled(conn, name, sql, false, cx);
        if let Some(tab) = self.tabs.last_mut() {
            tab.view = ResultView::Chart;
            tab.chart_measure = Some(measure.to_string());
        }
    }

    /// Save what the chart is showing. The statement and the measured column,
    /// under a name — kept by the ENGINE, so it is there in the editor too.
    fn save_chart(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let (conn, name, id) = (tab.conn_name.clone(), tab.title.clone(), tab.id);
        self.sync_tab_from_inputs(id, cx);
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else { return };
        if tab.sql.trim().is_empty() {
            return;
        }
        let measure = tab
            .chart_col
            .and_then(|i| tab.columns.get(i))
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let (sql, conn2) = (tab.sql.clone(), conn.clone());
        self.ask(
            Pending::SavedCharts { conn: conn.clone() },
            "store.charts.save",
            json!({ "conn": conn, "name": name, "sql": sql, "measure": measure }),
        );
        self.ask(
            Pending::SavedCharts { conn: conn2.clone() },
            "store.charts.list",
            json!({ "conn": conn2 }),
        );
        self.toast(format!("saved chart: {name}"), false);
    }

    // ── instance metrics ────────────────────────────────────────────────────

    /// Ask the server what it is doing. One round trip, when asked.
    fn ask_metrics(&mut self) {
        let Some(live) = self.conns.first() else { return };
        let (name, conn_id, driver) = (live.name.clone(), live.conn_id, live.driver.clone());
        if !self.meta(&driver).map_or(false, |m| m.caps.metrics) {
            self.metrics.clear();
            self.metrics_conn = Some(name);
            self.metrics_error =
                Some(format!("the {driver} driver has nothing to report about its server"));
            return;
        }
        self.metrics_inflight = true;
        self.ask(
            Pending::Metrics { conn: name },
            "instance.metrics",
            json!({ "conn_id": conn_id }),
        );
    }

    /// Repeat it, but ONLY while the panel is open and only at a rate somebody
    /// chose. Leaving the panel stops it, because this is the only place it is
    /// called from and it checks.
    fn poll_metrics(&mut self) {
        if self.rail != Rail::Instance || self.metrics_inflight || self.conns.is_empty() {
            return;
        }
        // The panel being open is the request. A session restored WITH it open
        // is somebody who left it open, so it reads once — and then only again
        // at a rate they chose.
        if self.metrics_at.is_none() && self.metrics_error.is_none() {
            self.ask_metrics();
            return;
        }
        let Some(every) = self.metrics_every else { return };
        let due = self.metrics_at.map_or(true, |t| t.elapsed() >= every);
        if due {
            self.ask_metrics();
        }
    }

    // ── the builder ─────────────────────────────────────────────────────────

    /// Put the builder's choices into the statement box, through the DRIVER's
    /// template. The builder never writes SQL itself; it supplies a projection
    /// and a list of clauses, and the template decides where they go.
    fn sync_builder(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let driver = self.tabs[idx].driver.clone();
        let Some(meta) = self.meta(&driver).cloned() else { return };
        let Some((name, schema)) = self.tabs[idx].object.clone() else { return };

        // Read the typed values back before rendering, or the statement lags a
        // keystroke behind what is on the screen.
        for i in 0..self.tabs[idx].builder.conds.len() {
            if let Some(input) = self.inputs.get(&(tab_id, format!("b:{i}"))) {
                let v = input.read(cx).text().to_string();
                if let Some(c) = self.tabs[idx].builder.conds.get_mut(i) {
                    c.value = v;
                }
            }
        }

        let kind = self.tabs[idx].kind.clone();
        let (columns, clauses) = self.tabs[idx].builder.render(&meta.builder_ops);
        let sql = lode_proto::render_full(
            meta.browse_for(&kind),
            &name,
            schema.as_deref(),
            PAGE,
            &[],
            &clauses,
            &columns,
        );
        self.tabs[idx].sql = sql.clone();
        self.tabs[idx].rendered = sql.clone();
        if let Some(input) = self.inputs.get(&(tab_id, String::new())) {
            input.update(cx, |i, cx| i.set_text(sql, cx));
        }
    }

    fn add_condition(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let first = self
            .tabs[idx]
            .meta_columns
            .first()
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let i = self.tabs[idx].builder.conds.len();
        self.tabs[idx].builder.conds.push(Cond { col: first, op: 0, value: String::new() });
        let colors = input_colors(&self.theme);
        let input = cx.new(|cx| TextInput::new(cx, "value", colors));
        self.inputs.insert((tab_id, format!("b:{i}")), input);
        self.sync_builder(tab_id, cx);
    }

    /// Step a condition to the next column. A list of the object's columns is
    /// all there is to choose from — a free-text column name would be a place to
    /// mistype one and find out at run time.
    fn cycle_cond_column(&mut self, tab_id: u64, i: usize) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let names: Vec<String> = self.tabs[idx].meta_columns.iter().map(|c| c.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        if let Some(c) = self.tabs[idx].builder.conds.get_mut(i) {
            let at = names.iter().position(|n| *n == c.col).map_or(0, |p| (p + 1) % names.len());
            c.col = names[at].clone();
        }
    }

    /// none \u{2192} first column ascending \u{2192} descending \u{2192} next column \u{2026}
    fn cycle_order(&mut self, tab_id: u64) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let names: Vec<String> = self.tabs[idx].meta_columns.iter().map(|c| c.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        let b = &mut self.tabs[idx].builder;
        b.order = match b.order.take() {
            None => Some((names[0].clone(), true)),
            Some((c, true)) => Some((c, false)),
            Some((c, false)) => {
                let at = names.iter().position(|n| *n == c).map_or(0, |p| p + 1);
                if at >= names.len() {
                    None
                } else {
                    Some((names[at].clone(), true))
                }
            }
        };
    }

    // ── writing a row ───────────────────────────────────────────────────────

    /// Ask the ENGINE what a change would do. Nothing is written by this.
    fn propose_write(
        &mut self,
        tab_id: u64,
        row: usize,
        set: Vec<(String, Value)>,
        delete: bool,
        _cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let tab = &self.tabs[idx];
        if !self.meta(&tab.driver).map_or(false, |m| m.caps.edit) {
            self.toast(
                format!("the {} driver does not edit rows", tab.driver),
                true,
            );
            return;
        }
        let key = match row_key(tab, row) {
            Ok(k) => k,
            Err(why) => {
                // The refusal IS the message. Not a disabled button with no
                // explanation, which teaches nothing.
                self.toast(why, true);
                return;
            }
        };
        let Some(conn_id) = self.live(&tab.conn_name).map(|l| l.conn_id) else {
            self.toast(format!("{} is not connected", tab.conn_name), true);
            return;
        };
        let Some((name, schema)) = tab.object.clone() else { return };
        let params = json!({
            "conn_id": conn_id,
            "obj": { "name": name, "schema": schema },
            "key": key,
            "set": set,
            "delete": delete,
        });
        self.ask(
            Pending::RowPlan { tab: tab_id, row, key, set, delete },
            "row.write",
            params,
        );
    }

    /// Repeat the write the engine described, with `confirm`.
    fn apply_write(&mut self, tab_id: u64, _cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let Some(p) = self.tabs[idx].pending_write.clone() else { return };
        let tab = &self.tabs[idx];
        let Some(conn_id) = self.live(&tab.conn_name).map(|l| l.conn_id) else { return };
        let Some((name, schema)) = tab.object.clone() else { return };
        self.ask(
            Pending::RowApply { tab: tab_id },
            "row.write",
            json!({
                "conn_id": conn_id,
                "obj": { "name": name, "schema": schema },
                "key": p.key,
                "set": p.set,
                "delete": p.delete,
                "confirm": true,
            }),
        );
    }

    /// Start editing one cell, loading the input with what is there now.
    fn begin_edit(&mut self, tab_id: u64, row: usize, col: usize, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        if !column_editable(&self.tabs[idx], col) {
            let why = match self.tabs[idx].columns.get(col) {
                Some(c) if self.tabs[idx].meta_columns.iter().any(|m| m.name == c.name) => {
                    format!("{} is part of the key — changing it would move the row", c.name)
                }
                Some(c) => format!("{} is not a column of the table, so there is nothing to write it to", c.name),
                None => "no such column".into(),
            };
            self.toast(why, true);
            return;
        }
        if let Err(why) = row_key(&self.tabs[idx], row) {
            self.toast(why, true);
            return;
        }
        let now = self.tabs[idx]
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(|v| cell_text(v).unwrap_or_default())
            .unwrap_or_default();
        let colors = input_colors(&self.theme);
        let input = cx.new(|cx| {
            let mut i = TextInput::new(cx, "", colors);
            i.set_text(now, cx);
            i
        });
        self.inputs.insert((tab_id, "edit".into()), input);
        self.tabs[idx].editing = Some((row, col));
        // Focus is taken on the next frame, once the field exists in the tree.
        cx.defer(move |cx| {
            cx.refresh_windows();
        });
    }

    /// Take what was typed and ask about it. A value is sent as TEXT unless it
    /// parses as a number or is literally `null` — the engine binds whatever it
    /// gets, so what matters here is not inventing a type the person did not.
    fn commit_edit(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let Some((row, col)) = self.tabs[idx].editing else { return };
        let Some(input) = self.inputs.get(&(tab_id, "edit".into())) else { return };
        let typed = input.read(cx).text().to_string();
        let Some(name) = self.tabs[idx].columns.get(col).map(|c| c.name.clone()) else { return };
        let was = self.tabs[idx].rows.get(row).and_then(|r| r.get(col)).cloned();
        let value = parse_cell(&typed, was.as_ref());
        if Some(&value) == was.as_ref() {
            self.tabs[idx].editing = None;
            return;
        }
        self.propose_write(tab_id, row, vec![(name, value)], false, cx);
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
        div().text_size(self.scale.small()).text_color(colour).child(text.into())
    }

    /// The scrolling part of a panel: everything above it (a heading, a search
    /// box, a reload link) stays put, and the rows move under it.
    ///
    /// `min_h_0` is the half that is easy to leave out: without it a flex child
    /// refuses to shrink below its content, so a long list pushes the heading
    /// and the search box off the top instead of scrolling beneath them.
    fn panel_list(&self, id: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            // Vertical scrolling only. `overflow_y_scroll` alone leaves the X
            // axis visible, and a child is then free to size itself to its
            // content — which is exactly how a long statement ends up wider
            // than the panel and gets clipped with no ellipsis instead of
            // being laid out to fit and truncated.
            .overflow_x_hidden()
            .overflow_y_scroll()
    }

    /// One line of free text that must not escape its panel: it ends in an
    /// ellipsis at the panel's edge rather than running over the result area.
    ///
    /// NO `min_w_0` here, and that is the whole lesson of this helper. `min_w_0`
    /// says "you may shrink to nothing", which is right for a flex ITEM sitting
    /// beside a fixed-width sibling and wrong for the text itself: the first
    /// version put it here, every metadata line computed to zero width, and the
    /// panel rendered a column of bare ellipses. The permission to shrink goes
    /// on the wrapper; the text takes the width it is given and cuts itself to
    /// fit.
    /// `w` is a real width in pixels, not `w_full`, and the comment on
    /// `Metrics::line_w` says why: GPUI draws the ellipsis only when the text
    /// was measured against a definite width, and a percentage inside a flex
    /// chain is not one. With `w_full` the text was clipped at the panel's edge
    /// mid-glyph and the `…` never appeared.
    /// A [`line`] for a slot in a flex ROW.
    ///
    /// The extra wrapper is not decoration. A flex item is measured by the flex
    /// algorithm before its own width is settled, and the text inside it is
    /// sized against that measurement — so a `line` placed straight into a row
    /// gets clipped at the slot's edge with no ellipsis, while the same `line`
    /// as a block child of a block truncates correctly. The wrapper is the
    /// block.
    fn line_in(&self, text: impl Into<SharedString>, size: gpui::Pixels, colour: Rgba, w: f32) -> impl IntoElement {
        div()
            .flex_none()
            .w(px(w))
            .overflow_hidden()
            .child(self.line(text, size, colour, w))
    }

    fn line(&self, text: impl Into<SharedString>, size: gpui::Pixels, colour: Rgba, w: f32) -> impl IntoElement {
        div()
            .w(px(w))
            .flex_none()
            .truncate()
            .text_size(size)
            .text_color(colour)
            .child(text.into())
    }

    fn section(&self, text: &str) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .text_size(self.scale.tiny())
            .text_color(self.t().title())
            .child(SharedString::from(text.to_uppercase()))
    }

    fn rail_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let ms = self.scale;
        let mut col = div()
            .flex()
            .flex_col()
            .w(px(ms.base() * 3.1))
            .h_full()
            .bg(t.status())
            .border_r_1()
            .border_color(t.border())
            .py_2()
            .gap_1();
        for mode in [Rail::Connections, Rail::Instance, Rail::History, Rail::Audit, Rail::Tasks, Rail::Settings] {
            let on = self.rail == mode;
            col = col.child(
                div()
                    .id(SharedString::from(mode.label()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .h(px(ms.rail_h()))
                    // Bigger, and the chosen one says so twice: a filled
                    // background AND a bar down its leading edge. One of the two
                    // alone was invisible on a palette whose surfaces sit 1.06
                    // apart — which is most of them.
                    .text_size(ms.icon())
                    .text_color(if on { t.accent() } else { t.muted() })
                    .when(on, |d| d.bg(t.selected()).border_l_2().border_color(t.accent()))
                    .when(!on, |d| d.border_l_2().border_color(t.status()))
                    .hover(|d| d.bg(t.hover()))
                    .cursor_pointer()
                    .child(SharedString::from(mode.glyph()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.rail = mode;
                        if mode == Rail::History && this.history.is_empty() {
                            this.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                        }
                        if mode == Rail::Audit && this.audit.is_empty() {
                            this.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
                        }
                        // Opening the panel is the request. Closing it is why
                        // nothing asks again.
                        if mode == Rail::Instance {
                            this.ask_metrics();
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
        let ms = self.scale;
        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(ms.panel_w()))
            .flex_none()
            .h_full()
            // Nothing in a panel may paint outside its column. Set HERE rather
            // than on each row, so it holds for the panels not written yet: a
            // long path or a long statement leaking over the result area is a
            // bug per panel if the rule lives in the rows, and impossible if it
            // lives in the container.
            .overflow_hidden()
            .bg(t.panel())
            .border_r_1()
            .border_color(t.border());

        match self.rail {
            Rail::Settings => {
                panel = panel.child(self.section("Theme")).child(
                    div()
                        .px_2()
                        .text_size(ms.body())
                        .text_color(t.fg())
                        .child(SharedString::from(self.theme_name.clone())),
                );
                panel = panel.child(self.section("Text size"));
                // One number, and it moves everything: the steps, the grid row
                // height, the rail. Here rather than only in an environment
                // variable because "a bit bigger" is a thing somebody adjusts
                // while looking at the result, not between restarts.
                let step = |label: &'static str, delta: f32| {
                    div()
                        .id(label)
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(ms.base() * 1.8))
                        .h(px(ms.base() * 1.8))
                        .cursor_pointer()
                        .bg(t.raised())
                        .hover(|d| d.bg(t.hover()))
                        .text_size(ms.body())
                        .text_color(t.accent())
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.scale = this.scale.with_base(this.scale.base() + delta);
                            this.remember();
                            cx.notify();
                        }))
                };
                panel = panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .child(step("\u{2212}", -1.0))
                        .child(
                            div()
                                .min_w(px(ms.base() * 2.5))
                                .text_size(ms.body())
                                .text_color(t.fg())
                                .child(SharedString::from(format!("{} px", ms.base()))),
                        )
                        .child(step("+", 1.0))
                        .child(
                            div()
                                .id("size-reset")
                                .px_2()
                                .cursor_pointer()
                                .hover(|d| d.bg(t.hover()))
                                .text_size(ms.small())
                                .text_color(t.muted())
                                .child("reset")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.scale = Metrics::new(crate::metrics::DEFAULT_BASE);
                                    this.remember();
                                    cx.notify();
                                })),
                        ),
                );
                panel = panel.child(div().px_2().child(self.small(
                    format!(
                        "every size comes off this one — headings {}, body {}, small {}, a grid row {}",
                        f32::from(ms.heading()),
                        f32::from(ms.body()),
                        f32::from(ms.small()),
                        ms.row_h()
                    ),
                    t.muted(),
                )));
                return panel;
            }
            Rail::Instance => {
                let who = self.metrics_conn.clone().unwrap_or_else(|| "nothing connected".into());
                panel = panel.child(self.section(&format!("Instance \u{2014} {who}")));

                // How often, said plainly and chosen explicitly. "off" is the
                // default because repeating a statistics query against
                // somebody else\u{2019}s production database is their decision.
                let mut rate = div().flex().items_center().gap_1().px_2().py_1();
                rate = rate.child(self.small("read every", t.muted()));
                for (label, every) in [
                    ("off", None),
                    ("2s", Some(Duration::from_secs(2))),
                    ("10s", Some(Duration::from_secs(10))),
                    ("60s", Some(Duration::from_secs(60))),
                ] {
                    let on = self.metrics_every == every;
                    rate = rate.child(
                        div()
                            .id(SharedString::from(format!("rate-{label}")))
                            .px_1()
                            .cursor_pointer()
                            .text_size(ms.tiny())
                            .text_color(if on { t.inverted() } else { t.muted() })
                            .when(on, |d| d.bg(t.accent()))
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.metrics_every = every;
                                cx.notify();
                            })),
                    );
                }
                rate = rate.child(
                    div()
                        .id("metrics-now")
                        .px_1()
                        .cursor_pointer()
                        .text_size(ms.tiny())
                        .text_color(t.accent())
                        .child("\u{f021} now")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask_metrics();
                            cx.notify();
                        })),
                );
                panel = panel.child(rate);

                if let Some(at) = self.metrics_at {
                    panel = panel.child(div().px_2().child(self.small(
                        format!("read {} ago", fmt_duration(at.elapsed())),
                        t.muted(),
                    )));
                }
                if let Some(e) = &self.metrics_error {
                    panel = panel.child(div().px_2().child(self.small(e.clone(), t.error())));
                }

                let mut group = String::new();
                let mut list = self.panel_list("instance-list");
                for row in &self.metrics {
                    if row.group != group {
                        group = row.group.clone();
                        list = list.child(self.section(&group));
                    }
                    list = list.child(
                        div()
                            .flex()
                            .items_center()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .justify_between()
                            .px_2()
                            .py(px(1.0))
                            // The NAME gives way first: a server that answers
                            // with a long path or a long version string is
                            // answering with the part worth reading.
                            .child(self.line_in(row.name.clone(), ms.small(), t.muted(), ms.line_w() - ms.base() * 9.0 - 8.0))
                            .child(self.line_in(row.value.clone(), ms.small(), t.fg(), ms.base() * 9.0)),
                    );
                }
                return panel.child(list);
            }
            Rail::History => {
                panel = panel.child(
                    div()
                        .id("history-reload")
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover()))
                        .text_size(ms.small())
                        .text_color(t.accent())
                        .child("\u{f021}  Reload history")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                            cx.notify();
                        })),
                );
                panel = panel.child(div().px_2().py_1().child(self.history_filter.clone()));
                let needle = self.history_filter.read(cx).text().to_string();
                let shown: Vec<&HistoryRow> = self
                    .history
                    .iter()
                    .filter(|h| {
                        needle.is_empty()
                            || fuzzy_score(&h.statement, &needle).is_some()
                            || h.conn
                                .as_deref()
                                .map_or(false, |c| fuzzy_score(c, &needle).is_some())
                    })
                    .collect();
                panel = panel.child(self.section(&if needle.is_empty() {
                    format!("History ({})", self.history.len())
                } else {
                    format!("History ({} of {})", shown.len(), self.history.len())
                }));
                if shown.is_empty() {
                    panel = panel.child(div().px_2().child(self.small(
                        if self.history.is_empty() {
                            "nothing yet — press reload"
                        } else {
                            "nothing matches"
                        },
                        t.muted(),
                    )));
                }
                let mut list = self.panel_list("history-list");
                for h in shown.into_iter().take(300) {
                    let failed = h.state.as_deref() == Some("failed");
                    let conn = h.conn.clone().unwrap_or_default();
                    let stmt = h.statement.clone();
                    // One line, with the newlines flattened. The ellipsis comes
                    // from the layout, not from a character count: 70 characters
                    // of a wide font is wider than the panel.
                    let one: String = stmt.replace('\n', " ");
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("hist-{}", h.id)))
                            .flex()
                            .flex_col()
                            .w_full()
                            .overflow_hidden()
                            .px_2()
                            .py(px(2.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover()))
                            .child(self.line(one, ms.small(), if failed { t.error() } else { t.fg() }, ms.line_w()))
                            .child(self.line(
                                format!(
                                    "{} · {} ms{}",
                                    if conn.is_empty() { "—".into() } else { conn.clone() },
                                    h.ms.unwrap_or(0),
                                    h.rows.map(|r| format!(" · {r} row(s)")).unwrap_or_default()
                                ),
                                ms.small(),
                                t.muted(),
                                ms.line_w(),
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
                return panel.child(list);
            }
            Rail::Audit => {
                panel = panel.child(
                    div()
                        .id("audit-reload")
                        .px_2()
                        .py_1()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover()))
                        .text_size(ms.small())
                        .text_color(t.accent())
                        .child("\u{f021}  Reload audit")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
                            cx.notify();
                        })),
                );
                panel = panel.child(div().px_2().py_1().child(self.audit_filter.clone()));
                let needle = self.audit_filter.read(cx).text().to_string();
                let shown: Vec<&AuditRow> = self
                    .audit
                    .iter()
                    .filter(|a| {
                        needle.is_empty()
                            || fuzzy_score(&a.statement, &needle).is_some()
                            || fuzzy_score(&a.outcome, &needle).is_some()
                            || fuzzy_score(&a.path, &needle).is_some()
                            || a.conn.as_deref().map_or(false, |c| fuzzy_score(c, &needle).is_some())
                            || a.reason.as_deref().map_or(false, |r| fuzzy_score(r, &needle).is_some())
                    })
                    .collect();
                let stopped = shown.iter().filter(|a| a.stopped()).count();
                panel = panel.child(self.section(&if needle.is_empty() {
                    format!("Audit ({}, {} stopped)", self.audit.len(), stopped)
                } else {
                    format!("Audit ({} of {})", shown.len(), self.audit.len())
                }));
                // Why a statement might be blank. The engine decides this, and a
                // reader who is not told reads the blank as a broken client.
                if self.audit_mode != "full" {
                    panel = panel.child(div().px_2().child(self.small(
                        if self.audit_mode == "shape" {
                            "this engine keeps only the SHAPE of a statement (LODE_AUDIT_STATEMENTS=shape)"
                        } else {
                            "this engine keeps no statement text (LODE_AUDIT_STATEMENTS=off)"
                        },
                        t.warning(),
                    )));
                }
                if shown.is_empty() {
                    panel = panel.child(div().px_2().child(self.small(
                        if self.audit.is_empty() {
                            "nothing has changed anything yet — reads are not in here"
                        } else {
                            "nothing matches"
                        },
                        t.muted(),
                    )));
                }
                let mut list = self.panel_list("audit-list");
                for a in shown.into_iter().take(400) {
                    // Three outcomes, three colours, and `applied` is the quiet
                    // one: the rows worth finding are the ones that were stopped.
                    let colour = match a.outcome.as_str() {
                        "refused" => t.warning(),
                        "failed" => t.error(),
                        "cancelled" => t.muted(),
                        "connected" | "disconnected" => t.muted(),
                        _ => t.fg(),
                    };
                    // No character count: the ellipsis is the layout's job, and
                    // 68 characters of a proportional font is a different width
                    // in every theme and at every text size.
                    let one: String = if a.statement.is_empty() {
                        format!("({} — not kept)", a.path)
                    } else {
                        a.statement.replace('\n', " ")
                    };
                    let open = self.audit_open == Some(a.id);
                    let id = a.id;
                    let mut row = div()
                        .id(SharedString::from(format!("aud-{}", a.id)))
                        .flex()
                        .flex_col()
                        .w_full()
                        .overflow_hidden()
                        .px_2()
                        .py(px(2.0))
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover()))
                        .child(
                            div()
                                .flex()
                                // A real width, for the same reason `line` takes one: a
                                // child of a flex row is measured against the row, and a row
                                // that is 100%-of-something is not a width at measure time.
                                .w(px(ms.line_w()))
                                .gap_2()
                                .child(
                                    // A fixed width, so every statement starts
                                    // at the same column: a list scanned for
                                    // the rows that were STOPPED is scanned
                                    // down one edge.
                                    // "disconnected" is the longest word that
                                    // goes here, so the column is sized from the
                                    // scale rather than from a pixel count that
                                    // was right at one font size and wrapped at
                                    // the next.
                                    div()
                                        .w(px(ms.outcome_w()))
                                        .flex_none()
                                        .whitespace_nowrap()
                                        .text_size(ms.tiny())
                                        .text_color(colour)
                                        .child(SharedString::from(a.outcome.clone())),
                                )
                                // flex_1 + min_w_0: without the second one a
                                // flex child will not shrink below its text, so
                                // a long statement widens the row instead of
                                // being cut at the panel's edge.
                                .child(self.line_in(
                                    one,
                                    ms.small(),
                                    t.fg(),
                                    ms.line_w() - ms.outcome_w() - 8.0,
                                )),
                        )
                        .child(self.line(
                            format!(
                                "{} · {}{}{}{}",
                                a.conn.clone().unwrap_or_else(|| "—".into()),
                                a.path,
                                a.policy.as_deref().map(|p| format!(" · {p}")).unwrap_or_default(),
                                a.affected.map(|n| format!(" · {n} row(s)")).unwrap_or_default(),
                                if a.confirmed { " · confirmed" } else { "" },
                            ),
                            ms.small(),
                            t.muted(),
                            ms.line_w(),
                        ));
                    // The reason, in full, for the row being looked at. Never
                    // truncated: a refusal shortened to fit is a refusal that
                    // stops explaining itself, which is the one thing it is for.
                    if open {
                        // The opened row is the one place text WRAPS instead of
                        // being cut: a refusal shortened to fit is a refusal
                        // that stops explaining itself, which is the one thing
                        // it is for. It still cannot leave the panel, because
                        // wrapping is not overflowing.
                        if let Some(reason) = a.reason.clone() {
                            row = row.child(
                                div()
                                    .w_full()
                                    .px_1()
                                    .py_1()
                                    .text_size(ms.small())
                                    .text_color(colour)
                                    .child(SharedString::from(reason)),
                            );
                        }
                        if !a.statement.is_empty() && a.statement.chars().count() > 68 {
                            row = row.child(
                                div()
                                    .w_full()
                                    .px_1()
                                    .text_size(ms.small())
                                    .text_color(t.fg())
                                    .child(SharedString::from(a.statement.clone())),
                            );
                        }
                        if let Some(fp) = a.fingerprint.clone() {
                            row = row.child(self.line(format!("shape {fp}"), ms.small(), t.muted(), ms.line_w()));
                        }
                    }
                    list = list.child(row.on_click(cx.listener(move |this, _, _, cx| {
                        this.audit_open = if this.audit_open == Some(id) { None } else { Some(id) };
                        cx.notify();
                    })));
                }
                return panel.child(list);
            }
            Rail::Tasks => {
                panel = panel.child(self.section(&format!("Tasks ({})", self.tasks.len())));
                let mut list = self.panel_list("tasks-list");
                for task in self.tasks.iter().rev().take(200) {
                    list = list.child(
                        div()
                            .w_full()
                            .overflow_hidden()
                            .px_2()
                            .py_1()
                            .child(self.line(
                                format!("{} · {}", task.label, fmt_duration(task.duration())),
                                ms.small(),
                                if task.failed() { t.error() } else { t.muted() },
                                ms.line_w(),
                            )),
                    );
                }
                return panel.child(list);
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
                .text_size(ms.small())
                .text_color(t.accent())
                .child("\u{f067}  New connection")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_form(None, cx);
                    cx.notify();
                })),
        );
        let filter = self.filter.read(cx).text().to_string();
        let connected = self.conns.len();
        // The saved connections AND every open connection's schema below them:
        // one list, because a deep tree is exactly what pushes a heading off the
        // top of a panel that is not scrolling.
        let mut list = self.panel_list("connections-list");
        list = list.child(self.section(&format!(
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
            list = list.child(
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
                            .text_size(ms.tiny())
                            .text_color(if is_live { t.success() } else { t.muted() })
                            .child(SharedString::from(if is_live { "\u{f111}" } else { "\u{f10c}" })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.line_in(
                            c.name.clone(),
                            ms.body(),
                            if is_live { t.fg() } else { t.muted() },
                            ms.line_w() - ms.base() * 7.5,
                        )),
                    )
                    .child(div().flex_none().child(self.small(c.driver.clone(), t.muted())))
                    .child(
                        div()
                            .id(SharedString::from(format!("edit-{}", c.name)))
                            .text_size(ms.tiny())
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
            list = list.child(self.section(&live.name));
            if live.nodes.is_empty() {
                list = list.child(div().px_2().child(self.small("loading…", t.muted())));
            }
            if !live.charts.is_empty() {
                list = list.child(self.section("Saved charts"));
                for c in &live.charts {
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("sc-{}-{}", live.name, c.name)))
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl(px(10.0))
                            .pr_2()
                            .py(px(2.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover()))
                            .child(div().flex_none().text_size(ms.tiny()).text_color(t.accent()).child("\u{f080}"))
                            .child(self.line_in(c.name.clone(), ms.small(), t.fg(), ms.line_w() - ms.base() - 14.0))
                            .on_click(cx.listener({
                                let (cn, n, sql, m) =
                                    (live.name.clone(), c.name.clone(), c.sql.clone(), c.measure.clone());
                                move |this, _, _, cx| {
                                    this.open_chart(&cn, &n, &sql, &m, cx);
                                    cx.notify();
                                }
                            })),
                    );
                }
            }
            if !live.queries.is_empty() {
                list = list.child(self.section("Saved queries"));
                for q in &live.queries {
                    list = list.child(
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
                            .child(div().flex_none().text_size(ms.tiny()).text_color(t.accent()).child("\u{f15c}"))
                            .child(self.line_in(q.name.clone(), ms.small(), t.fg(), ms.line_w() - ms.base() - 14.0))
                            .on_click(cx.listener({
                                let (c, n, s) = (live.name.clone(), q.name.clone(), q.sql.clone());
                                move |this, _, _, cx| {
                                    this.open_sql(&c, &n, &s, cx);
                                    cx.notify();
                                }
                            })),
                    );
                }
                list = list.child(self.section("Schema"));
            }
            for node in live
                .nodes
                .iter()
                .filter(|n| crate::workspace::node_matches(n, &filter))
            {
                list = list.child(self.node_el(&live.name, node, 0, cx));
            }
        }
        panel.child(list)
    }

    /// One schema node. A branch toggles; a leaf OPENS and runs.
    fn node_el(&self, conn: &str, node: &Node, depth: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
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
                    .text_size(ms.tiny())
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
            .child(
                div()
                    .flex_none()
                    .text_size(ms.small())
                    .text_color(t.accent())
                    .child(SharedString::from(glyph)),
            )
            // A node is indented by its depth, so the room it has shrinks as
            // the tree goes deeper — the width has to say so, or a deep leaf
            // would be cut without an ellipsis again.
            .child(self.line_in(
                node.name.clone(),
                ms.body(),
                t.fg(),
                (ms.line_w() - 8.0 - depth as f32 * 12.0 - ms.base() * 2.2).max(ms.base() * 3.0),
            ))
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
        let ms = self.scale;
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
                            .text_size(ms.tiny())
                            .text_color(t.accent())
                            // The tab carries its connection's driver, because
                            // tabs from different connections sit side by side.
                            .child(SharedString::from(driver_glyph(&tab.driver))),
                    )
                    .child(
                        div()
                            .text_size(ms.body())
                            .text_color(if on { t.fg() } else { t.muted() })
                            .child(SharedString::from(tab.title.clone())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("tabx-{}", tab.id)))
                            .text_size(ms.tiny())
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
        let ms = self.scale;
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
                    .text_size(ms.small())
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
                ),
        );

        // ── view switch ─────────────────────────────────────────────────────
        // The sub-tabs belong HERE, next to the views, not pinned to the far
        // corner of the breadcrumbs: they are about the result below them.
        let mut views = div().flex().items_center().gap_2().px_3().py_1();
        views = views.child(sub_row).child(self.small("│", t.border()));
        for v in [ResultView::Tree, ResultView::Table, ResultView::Json, ResultView::Chart] {
            let on = tab.view == v;
            let tab_id = tab.id;
            views = views.child(
                div()
                    .id(SharedString::from(format!("view-{}-{}", tab_id, v.label())))
                    .px_2()
                    .cursor_pointer()
                    .text_size(ms.small())
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
                .text_size(ms.small())
                .text_color(if running { t.error() } else { t.border() })
                .child("Cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.cancel_active();
                    cx.notify();
                })),
        );
        if tab.view == ResultView::Chart {
            views = views.child(
                div()
                    .id("save-chart")
                    .px_2()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.accent())
                    .child("Save chart")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_chart(cx);
                        cx.notify();
                    })),
            );
        }
        views = views.child(
            div()
                .id("save-query")
                .px_2()
                .cursor_pointer()
                .text_size(ms.small())
                .text_color(t.accent())
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
                .text_size(ms.small())
                // A control nobody can use still has to be READABLE — greyed
                // out to the border colour, it simply vanished, and a button
                // that is invisible is not disabled, it is missing.
                .text_color(if has_rows { t.accent() } else { t.muted() })
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
                    .text_size(ms.body())
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

        // The Schema sub-tab describes the OBJECT, so the view switch and the
        // paging do not apply to it: it is not a different rendering of the
        // rows, it is a different subject.
        if tab.sub == SubTab::Schema {
            return body.child(self.schema_el(tab)).into_any_element();
        }
        if tab.sub == SubTab::Builder {
            return body.child(self.builder_el(tab, cx)).into_any_element();
        }
        let view = match tab.view {
            ResultView::Table => self.table_el(tab, cx),
            ResultView::Tree => self.tree_el(tab, cx),
            ResultView::Json => self.json_el(tab),
            ResultView::Chart => self.chart_el(tab, cx),
        };
        // The detail panel sits BESIDE the grid, not over it: the row it is
        // showing stays visible in its place among the others, which is half of
        // what you are looking at when you open one.
        body = body.child(match self.detail_el(tab, cx) {
            Some(detail) => div()
                .flex()
                .flex_1()
                .overflow_hidden()
                .child(div().flex_1().overflow_hidden().child(view))
                .child(detail)
                .into_any_element(),
            None => view,
        });
        body.into_any_element()
    }

    /// The result as bars.
    ///
    /// Drawn from the ROWS, not from the declared column types: a driver that
    /// declares nothing — sqlite usually does — would otherwise never be
    /// chartable, and a column declared INTEGER can still arrive holding text.
    fn chart_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let shape = match ChartShape::of(&tab.columns, &tab.rows) {
            Ok(s) => s,
            // Say WHY there is no chart. "No chart" on its own reads as a bug.
            Err(why) => {
                return div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(div().text_color(t.muted()).child(SharedString::from(why)))
                    .child(self.small(
                        "a chart needs one column of numbers and one to name the bars",
                        t.muted(),
                    ))
                    .into_any_element();
            }
        };

        let tab_id = tab.id;
        // A saved chart names its measurement. Match it by NAME here, so a
        // statement that gained or reordered a column still measures what the
        // chart was saved measuring — and falls back visibly rather than
        // drawing a different column under the old name.
        let by_name = tab.chart_measure.as_ref().and_then(|m| {
            shape
                .values
                .iter()
                .copied()
                .find(|&i| tab.columns.get(i).map(|c| &c.name) == Some(m))
        });
        let chosen = tab
            .chart_col
            .filter(|c| shape.values.contains(c))
            .or(by_name)
            .unwrap_or(shape.values[0]);

        // Which measurement is being drawn, when there is more than one.
        let mut picker = div().flex().items_center().gap_2().px_3().py_1();
        if shape.values.len() > 1 {
            picker = picker.child(self.small("measure", t.muted()));
            for &i in &shape.values {
                let on = i == chosen;
                let name = tab.columns.get(i).map(|c| c.name.clone()).unwrap_or_default();
                picker = picker.child(
                    div()
                        .id(SharedString::from(format!("ch-{tab_id}-{i}")))
                        .px_1()
                        .cursor_pointer()
                        .text_size(ms.small())
                        .text_color(if on { t.inverted() } else { t.fg() })
                        .when(on, |d| d.bg(t.accent()))
                        .hover(|d| d.bg(t.hover()))
                        .child(SharedString::from(name))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                                tb.chart_col = Some(i);
                            }
                            cx.notify();
                        })),
                );
            }
        }

        let (lo, hi) = value_range(&tab.rows, chosen);
        let span = hi - lo;
        let mut bars = div().flex().flex_col().flex_1().overflow_hidden().px_3().py_1().gap(px(2.0));
        // Only what fits is drawn, and the count below says how many there are:
        // a chart of two hundred bars three pixels wide is not a chart.
        let shown = tab.rows.len().min(40);
        for r in 0..shown {
            let v = tab.rows.get(r).and_then(|row| row.get(chosen)).and_then(|x| x.as_f64());
            let frac = v.map(|n| ((n - lo) / span).clamp(0.0, 1.0)).unwrap_or(0.0);
            let label = bar_label(tab, &shape, r);
            bars = bars.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(16.0))
                    .child(
                        div()
                            .w(px(160.0))
                            .text_size(ms.small())
                            .text_color(t.muted())
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div().flex_1().h_full().child(
                            div()
                                .h_full()
                                .w(gpui::relative(frac as f32))
                                // A missing measurement is drawn as NOTHING, not
                                // as a bar of zero: "no value" and "zero" are
                                // different answers.
                                .bg(if v.is_some() { t.accent() } else { t.panel() }),
                        ),
                    )
                    .child(
                        div()
                            .w(px(110.0))
                            .text_size(ms.small())
                            .text_color(if v.is_some() { t.fg() } else { t.muted() })
                            .child(SharedString::from(match v {
                                Some(n) => format!("{n}"),
                                None => "NULL".into(),
                            })),
                    ),
            );
        }

        let footer = self.small(
            format!(
                "{}\u{2013}{} of {} loaded row(s) \u{2014} axis from {lo} to {hi}",
                if shown == 0 { 0 } else { 1 },
                shown,
                tab.rows.len()
            ),
            t.muted(),
        );

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(picker)
            .child(bars)
            .child(div().px_3().py_1().child(footer))
            .into_any_element()
    }

    /// The visual builder: choices in, a statement out.
    ///
    /// What comes out goes into the statement box above, where it can be read
    /// and edited before it runs — and when it runs it goes through everything
    /// else does, including the connection's policy. The builder is a way of
    /// WRITING a statement, not a way around the rules about running one.
    fn builder_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let tab_id = tab.id;
        let ops = self.meta(&tab.driver).map(|m| m.builder_ops.clone()).unwrap_or_default();
        let mut body = div().flex().flex_col().flex_1().overflow_hidden().p_3().gap_1();

        if ops.is_empty() {
            return body
                .child(self.small(
                    format!("the {} driver has no builder — its statements are not SQL", tab.driver),
                    t.muted(),
                ))
                .into_any_element();
        }
        if tab.object.is_none() {
            return body
                .child(self.small(
                    "the builder works on an object — open a table from the tree",
                    t.muted(),
                ))
                .into_any_element();
        }

        // ── columns ─────────────────────────────────────────────────────────
        body = body.child(self.section("Show"));
        let all = tab.builder.columns.is_empty();
        let mut cols_row = div().flex().flex_wrap().gap_2().child(
            div()
                .id("b-all")
                .px_1()
                .cursor_pointer()
                .text_size(ms.small())
                .text_color(if all { t.inverted() } else { t.muted() })
                .when(all, |d| d.bg(t.accent()))
                .child("every column")
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                        tb.builder.columns.clear();
                    }
                    this.sync_builder(tab_id, cx);
                    cx.notify();
                })),
        );
        for c in &tab.meta_columns {
            let on = tab.builder.columns.contains(&c.name);
            let name = c.name.clone();
            cols_row = cols_row.child(
                div()
                    .id(SharedString::from(format!("b-col-{}", c.name)))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(if on { t.inverted() } else { t.fg() })
                    .when(on, |d| d.bg(t.accent()))
                    .hover(|d| d.bg(t.hover()))
                    .child(SharedString::from(c.name.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                            match tb.builder.columns.iter().position(|x| *x == name) {
                                Some(i) => {
                                    tb.builder.columns.remove(i);
                                }
                                None => tb.builder.columns.push(name.clone()),
                            }
                        }
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
        }
        body = body.child(cols_row);

        // ── conditions ──────────────────────────────────────────────────────
        body = body.child(self.section("Where"));
        for (i, cond) in tab.builder.conds.iter().enumerate() {
            let mut row = div().flex().items_center().gap_2();
            // column
            let cur = cond.col.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("b-cc-{i}")))
                    .w(px(160.0))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.fg())
                    .border_1()
                    .border_color(t.border())
                    .child(SharedString::from(if cur.is_empty() {
                        "pick a column".to_string()
                    } else {
                        cur
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cycle_cond_column(tab_id, i);
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
            // operator
            let label = ops.get(cond.op).map(|o| o.label.clone()).unwrap_or_default();
            row = row.child(
                div()
                    .id(SharedString::from(format!("b-op-{i}")))
                    .w(px(110.0))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.accent())
                    .border_1()
                    .border_color(t.border())
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let n = this
                            .meta(&this.tabs.iter().find(|x| x.id == tab_id).map(|x| x.driver.clone()).unwrap_or_default())
                            .map(|m| m.builder_ops.len())
                            .unwrap_or(1)
                            .max(1);
                        if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                            if let Some(c) = tb.builder.conds.get_mut(i) {
                                c.op = (c.op + 1) % n;
                            }
                        }
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
            // value, only where the operator takes one
            if ops.get(cond.op).map_or(true, |o| o.takes_value()) {
                if let Some(input) = self.inputs.get(&(tab_id, format!("b:{i}"))) {
                    row = row.child(div().w(px(240.0)).child(input.clone()));
                }
            } else {
                row = row.child(div().w(px(240.0)).child(self.small("takes no value", t.muted())));
            }
            row = row.child(
                div()
                    .id(SharedString::from(format!("b-rm-{i}")))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.muted())
                    .child("\u{f00d}")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                            if i < tb.builder.conds.len() {
                                tb.builder.conds.remove(i);
                            }
                        }
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
            body = body.child(row);
        }
        body = body.child(
            div()
                .id("b-add")
                .px_1()
                .cursor_pointer()
                .text_size(ms.small())
                .text_color(t.accent())
                .child("+ condition")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.add_condition(tab_id, cx);
                    cx.notify();
                })),
        );

        // ── order ───────────────────────────────────────────────────────────
        body = body.child(self.section("Order"));
        let order_label = match &tab.builder.order {
            Some((c, asc)) => format!("{c} {}", if *asc { "ascending" } else { "descending" }),
            None => "none".to_string(),
        };
        body = body.child(
            div()
                .id("b-order")
                .w(px(300.0))
                .px_1()
                .cursor_pointer()
                .text_size(ms.small())
                .text_color(t.fg())
                .border_1()
                .border_color(t.border())
                .child(SharedString::from(order_label))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.cycle_order(tab_id);
                    this.sync_builder(tab_id, cx);
                    cx.notify();
                })),
        );

        body.child(self.small(
            "the statement above is what runs \u{2014} it goes through the connection\u{2019}s policy \
             like anything else",
            t.muted(),
        ))
        .into_any_element()
    }

    /// What the object IS: its columns, which of them identify a row, its
    /// indexes, and the engine's own CREATE statement where it has one.
    ///
    /// Every part of this comes from the engine; none of it is reconstructed
    /// here. A CREATE assembled from column metadata reads as authoritative and
    /// quietly drops constraints, defaults and storage clauses — so a driver
    /// that cannot ask its server for one says so instead.
    fn schema_el(&self, tab: &Tab) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let mut body = div().flex().flex_col().flex_1().overflow_hidden().p_3().gap_2();

        if tab.object.is_none() {
            return body
                .child(self.small(
                    "this tab is a statement, not an object — there is nothing to describe",
                    t.muted(),
                ))
                .into_any_element();
        }

        body = body.child(self.section(&format!("Columns ({})", tab.meta_columns.len())));
        if tab.meta_columns.is_empty() {
            body = body.child(self.small("the engine has not answered yet", t.muted()));
        }
        for c in &tab.meta_columns {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(220.0)).text_size(ms.body()).text_color(t.fg()).child(
                        SharedString::from(c.name.clone()),
                    ))
                    .child(div().w(px(160.0)).child(self.small(c.type_name.clone(), t.muted())))
                    .when(c.primary, |d| d.child(self.small("key", t.accent()))),
            );
        }

        let keyed = tab.meta_columns.iter().any(|c| c.primary);
        body = body.child(self.small(
            if keyed {
                "a row here can be addressed exactly, so it can be edited".to_string()
            } else {
                "no key: a row here cannot be addressed exactly, so it is not editable".to_string()
            },
            if keyed { t.muted() } else { t.error() },
        ));

        if !tab.indexes.is_empty() {
            body = body.child(self.section(&format!("Indexes ({})", tab.indexes.len())));
            for i in &tab.indexes {
                let mut tags = Vec::new();
                if i.primary {
                    tags.push("primary");
                }
                if i.unique {
                    tags.push("unique");
                }
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(220.0)).text_size(ms.body()).text_color(t.fg()).child(
                            SharedString::from(i.name.clone()),
                        ))
                        .child(div().w(px(260.0)).child(self.small(i.columns.join(", "), t.muted())))
                        .child(self.small(tags.join(" · "), t.accent())),
                );
            }
        }

        if let Some(ddl) = &tab.ddl {
            body = body.child(self.section("CREATE"));
            body = body.child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(ms.small())
                    .text_color(t.fg())
                    .child(SharedString::from(ddl.clone())),
            );
        }
        body.into_any_element()
    }

    /// One row, read downward — the only way to read a row with forty columns
    /// in it, and the place the clipboard buttons belong.
    fn detail_el(&self, tab: &Tab, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let i = tab.detail?;
        let row = tab.rows.get(i)?;
        let t = self.t();
        let ms = self.scale;

        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(380.0))
            .h_full()
            .bg(t.panel())
            .border_l_1()
            .border_color(t.border())
            .overflow_hidden();

        panel = panel.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_1()
                .bg(t.header_bg())
                .child(
                    div()
                        .text_size(ms.small())
                        .text_color(t.header_fg())
                        .child(SharedString::from(format!("Row {}", tab.from + i + 1))),
                )
                .child(
                    div()
                        .id("detail-close")
                        .px_1()
                        .cursor_pointer()
                        .text_size(ms.small())
                        .text_color(t.muted())
                        .child("\u{f00d}")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(tb) = this.tabs.get_mut(this.active) {
                                tb.detail = None;
                            }
                            cx.notify();
                        })),
                ),
        );

        let tab_id = tab.id;
        let editable = self.meta(&tab.driver).map_or(false, |m| m.caps.edit);
        let mut fields = div().flex().flex_col().flex_1().overflow_hidden().py_1();
        for (ci, c) in tab.columns.iter().enumerate() {
            let v = row.get(ci).unwrap_or(&serde_json::Value::Null);
            let null = v.is_null();
            let key_col = tab.meta_columns.iter().any(|m| m.name == c.name && m.primary);
            let editing_this = tab.editing == Some((i, ci));
            let can = editable && column_editable(tab, ci) && tab.pending_write.is_none();

            let label = div()
                .flex()
                .items_center()
                .gap_2()
                .child(self.small(c.name.clone(), t.header_fg()))
                .child(self.small(
                    crate::workspace::type_badge(v, Some(&c.type_name)),
                    t.muted(),
                ))
                // A key column says so. It is why it cannot be edited, and
                // saying it here is cheaper than a refusal later.
                .when(key_col, |d| d.child(self.small("key", t.accent())));

            let value: gpui::AnyElement = if editing_this {
                match self.inputs.get(&(tab_id, "edit".to_string())) {
                    Some(input) => div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().child(input.clone()))
                        .child(
                            div()
                                .id("edit-ok")
                                .px_1()
                                .cursor_pointer()
                                .text_size(ms.small())
                                .text_color(t.accent())
                                .child("\u{f00c}")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.commit_edit(tab_id, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("edit-no")
                                .px_1()
                                .cursor_pointer()
                                .text_size(ms.small())
                                .text_color(t.muted())
                                .child("\u{f00d}")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(tb) =
                                        this.tabs.iter_mut().find(|x| x.id == tab_id)
                                    {
                                        tb.editing = None;
                                    }
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                    None => div().into_any_element(),
                }
            } else {
                div()
                    .id(SharedString::from(format!("val-{tab_id}-{ci}")))
                    .text_size(ms.body())
                    .text_color(if null { t.muted() } else { t.fg() })
                    .when(can, |d| d.cursor_pointer().hover(|h| h.bg(t.hover())))
                    .child(SharedString::from(
                        cell_text(v).unwrap_or_else(|| "NULL".to_string()),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_edit(tab_id, i, ci, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            };

            fields = fields.child(
                div().flex().flex_col().px_3().py(px(2.0)).child(label).child(value),
            );
        }
        panel = panel.child(fields);

        // A write the engine has described but not done.
        if let Some(p) = &tab.pending_write {
            let mut ask = div()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_1()
                .bg(t.panel())
                .border_t_1()
                .border_color(t.error())
                .child(self.small(
                    p.reason.clone().unwrap_or_else(|| {
                        "this changes stored data".to_string()
                    }),
                    t.error(),
                ))
                // The statement is the ENGINE's, shown verbatim. A client that
                // displayed its own version and sent another would be asking
                // about one thing and doing a different one.
                .child(
                    div()
                        .text_size(ms.small())
                        .text_color(t.fg())
                        .child(SharedString::from(p.statement.clone())),
                );
            ask = ask.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("write-yes")
                            .px_2()
                            .cursor_pointer()
                            .text_size(ms.small())
                            .bg(t.error())
                            .text_color(t.inverted())
                            .child(if p.delete { "Delete it" } else { "Write it" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_write(tab_id, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("write-no")
                            .px_2()
                            .cursor_pointer()
                            .text_size(ms.small())
                            .text_color(t.muted())
                            .child("Leave it")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                                    tb.pending_write = None;
                                }
                                cx.notify();
                            })),
                    ),
            );
            panel = panel.child(ask);
        }

        // Copy as Query. See `copy_as_select` for why the client is allowed to
        // write SQL here: this goes to a person, not to a database.
        let sql = self.meta(&tab.driver).map_or(false, |m| m.caps.sql);
        let mut foot = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(t.border());
        if sql {
            foot = foot
                .child(
                    div()
                        .id("copy-select")
                        .px_2()
                        .cursor_pointer()
                        .text_size(ms.small())
                        .text_color(t.accent())
                        .child("Copy as SELECT")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_row(false, cx);
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("copy-insert")
                        .px_2()
                        .cursor_pointer()
                        .text_size(ms.small())
                        .text_color(t.accent())
                        .child("Copy as INSERT")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_row(true, cx);
                            cx.notify();
                        })),
                );
        } else {
            foot = foot.child(self.small("this driver does not speak SQL", t.muted()));
        }
        if editable && tab.pending_write.is_none() {
            foot = foot.child(div().flex_1()).child(
                div()
                    .id("row-delete")
                    .px_2()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.error())
                    .child("Delete row")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.propose_write(tab_id, i, Vec::new(), true, cx);
                        cx.notify();
                    })),
            );
        }
        Some(panel.child(foot).into_any_element())
    }

    /// The query bar: the driver's declared fields, or one statement box.
    ///
    /// Which one is NOT decided here — `is_structured` is true exactly when the
    /// engine sent a `query_form` for that driver. MongoDB sends four fields and
    /// the JSON each contributes; every SQL driver sends none and gets a box.
    fn query_bar(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
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

        // A browse the tree produced can be narrowed. The fields come from the
        // DRIVER — it is the one that knows where a WHERE goes in its own
        // dialect, and which dialects have one at all.
        let browse = meta
            .filter(|_| tab.object.is_some())
            .map(|m| m.browse_form.clone())
            .unwrap_or_default();
        let mut narrow = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border());
        let has_narrow = !browse.is_empty();
        for f in &browse {
            let Some(input) = self.inputs.get(&(tab_id, format!("browse:{}", f.key))) else {
                continue;
            };
            narrow = narrow.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.small(f.label.clone(), t.muted()))
                    .child(div().w(px(240.0)).child(input.clone())),
            );
        }
        if has_narrow {
            narrow = narrow.child(div().flex_1()).child(self.small(
                "the statement above is what runs — edit it and these stop rewriting it",
                t.muted(),
            ));
        }

        // The run button says what it does in this engine's words.
        let run_label = if structured { "Find" } else { "Run" };
        bar = bar.child(
            div()
                .id(SharedString::from(format!("run-{tab_id}")))
                .px_2()
                .cursor_pointer()
                .text_size(ms.small())
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
                        .text_size(ms.small())
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
                        .text_size(ms.small())
                        .text_color(if can_next { t.fg() } else { t.border() })
                        .child("▶")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page_by(1);
                            cx.notify();
                        })),
                ),
        );
        if has_narrow {
            return div()
                .flex()
                .flex_col()
                .w_full()
                .child(bar)
                .child(narrow)
                .into_any_element();
        }
        bar.into_any_element()
    }

    /// The export row. It says what it writes — the LOADED rows — because an
    /// export that silently paged the whole table would be a different feature.
    fn export_row(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
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
                    .text_size(ms.tiny())
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
                .text_size(ms.small())
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
        let ms = self.scale;
        let cols: Vec<(usize, String)> = tab
            .grid_columns()
            .into_iter()
            .map(|(i, c)| (i, c.name.clone()))
            .collect();
        if cols.is_empty() {
            return div().p_3().child(self.small("no columns", t.muted())).into_any_element();
        }

        let sort = tab.sort;
        let tab_id = tab.id;
        let mut header = div()
            .flex()
            .w_full()
            .bg(t.header_bg())
            .border_b_1()
            .border_color(t.border())
            .child(div().w(px(NUM_W)).px(px(6.0)).child(self.small("#", t.header_fg())));
        for (ci, name) in &cols {
            let (ci, name) = (*ci, name.clone());
            let arrow = match sort {
                Some((c, asc)) if c == ci => {
                    if asc {
                        " \u{f0de}"
                    } else {
                        " \u{f0dd}"
                    }
                }
                _ => "",
            };
            header = header.child(
                div()
                    .id(SharedString::from(format!("hdr-{tab_id}-{ci}")))
                    .w(px(tab.widths.get(&ci).copied().unwrap_or(ms.col_w())))
                    .px(px(6.0))
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.header_fg())
                    .hover(|d| d.bg(t.hover()))
                    .child(SharedString::from(format!("{name}{arrow}")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sort_by(tab_id, ci);
                        cx.notify();
                    })),
            );
            // The handle between this column and the next. It is its own strip
            // so that dragging never sorts: a click that moved is not a click.
            let start_w = tab.widths.get(&ci).copied().unwrap_or(ms.col_w());
            header = header.child(
                div()
                    .id(SharedString::from(format!("grip-{tab_id}-{ci}")))
                    .w(px(3.0))
                    .h_full()
                    .bg(t.border())
                    .cursor_col_resize()
                    .hover(|d| d.bg(t.accent()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                            this.resizing = Some((tab_id, ci, f32::from(ev.position.x), start_w));
                            cx.notify();
                        }),
                    ),
            );
        }

        let rows: Vec<Vec<Value>> = tab.rows.clone();
        let first = tab.from;
        let (muted, null_fg, row_alt, fg) = (t.muted(), t.null_fg(), t.row_alt(), t.fg());
        let cols2 = cols.clone();
        let widths: HashMap<usize, f32> = tab.widths.clone();

        let list = uniform_list(
            "rows",
            rows.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _window, _cx| {
                let mut out = Vec::new();
                for i in range {
                    let mut line = div()
                        .flex()
                        .w_full()
                        .h(px(ms.row_h()))
                        // Alternating rows: the stripe is a surface from the
                        // theme, not a computed shade.
                        .when(i % 2 == 1, |d| d.bg(row_alt))
                        .child(
                            div()
                                .w(px(NUM_W))
                                .px(px(6.0))
                                .text_size(ms.tiny())
                                .text_color(muted)
                                .child(SharedString::from((first + i + 1).to_string())),
                        );
                    for (ci, _) in &cols2 {
                        let cell = rows.get(i).and_then(|r| r.get(*ci));
                        let (text, colour) = match cell.and_then(cell_text) {
                            None => ("NULL".to_string(), null_fg),
                            Some(s) => (s.replace('\n', "⏎"), fg),
                        };
                        line = line
                            .child(
                                div()
                                    .w(px(widths.get(ci).copied().unwrap_or(ms.col_w())))
                                    .px(px(6.0))
                                    .text_size(ms.body())
                                    .text_color(colour)
                                    .child(SharedString::from(text)),
                            )
                            .child(div().w(px(3.0)));
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
        let _ = self.scale;
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
        let ms = self.scale;
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
                                    .text_size(ms.small())
                                    .text_color(t.muted())
                                    .child(SharedString::from(k.clone())),
                            )
                            .child(
                                div()
                                    .text_size(ms.small())
                                    .text_color(t.fg())
                                    .child(SharedString::from(preview(val))),
                            )
                            .child(
                                // The badge, in the colour its type maps to.
                                div()
                                    .text_size(ms.tiny())
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
                        .text_size(ms.small())
                        .text_color(t.fg())
                        .child(SharedString::from(preview(other))),
                );
            }
        }
        out.into_any_element()
    }

    fn json_el(&self, tab: &Tab) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let docs: Vec<Value> = (0..tab.rows.len().min(200)).map(|i| tab.document(i)).collect();
        let text = serde_json::to_string_pretty(&docs).unwrap_or_else(|_| "[]".into());
        div()
            .size_full()
            .p_2()
            .text_size(ms.small())
            .text_color(t.fg())
            .child(SharedString::from(text))
            .into_any_element()
    }

    fn tasks_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let ms = self.scale;
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
                        .text_size(ms.small())
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
                                    .text_size(ms.tiny())
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
        let ms = self.scale;
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
                                        .text_size(ms.small())
                                        .bg(t.error())
                                        .text_color(t.inverted())
                                        .child("Run it")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some((conn, stmt, _)) = this.confirm.take() {
                                                this.run_statement_approved(&conn, &stmt, cx);
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
                                        .text_size(ms.small())
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
        let _ = self.scale;
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

impl Workspace {
    /// The corner messages. They stack upward from the bottom right, newest
    /// nearest the corner, and they do not take clicks: nothing is hidden
    /// behind one for longer than it lasts.
    fn toasts_el(&self) -> Option<gpui::AnyElement> {
        if self.toasts.is_empty() {
            return None;
        }
        let t = self.t();
        let ms = self.scale;
        let mut stack = div().absolute().right(px(16.0)).bottom(px(16.0)).flex().flex_col().gap_1();
        for x in self.toasts.iter().rev().take(5) {
            stack = stack.child(
                div()
                    .px_3()
                    .py_1()
                    .bg(t.panel())
                    .border_l_2()
                    .border_color(if x.bad { t.error() } else { t.accent() })
                    .text_size(ms.body())
                    .text_color(if x.bad { t.error() } else { t.fg() })
                    .child(SharedString::from(x.text.clone())),
            );
        }
        Some(stack.into_any_element())
    }

    /// The command palette: one box, and everything this window can do ranked
    /// against what has been typed. ctrl-p opens it, escape leaves it,
    /// ctrl-n/ctrl-k move, enter or ctrl-y takes the one highlighted.
    fn palette_el(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.palette_open {
            return None;
        }
        let t = self.t();
        let ms = self.scale;
        let matches = self.palette_matches(cx);
        let sel = self.palette_sel.min(matches.len().saturating_sub(1));

        let mut list = div().flex().flex_col().max_h(px(420.0)).overflow_hidden();
        if matches.is_empty() {
            list = list.child(div().px_3().py_1().child(self.small("nothing matches", t.muted())));
        }
        for (n, (idx, it)) in matches.iter().enumerate().take(14) {
            let on = n == sel;
            let act = it.act.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("pal-{idx}")))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py(px(3.0))
                    .cursor_pointer()
                    .when(on, |d| d.bg(t.selected()).border_l_2().border_color(t.accent()))
                    .when(!on, |d| d.border_l_2().border_color(t.panel()))
                    .hover(|d| d.bg(t.hover()))
                    .child(
                        div()
                            .text_size(ms.body())
                            .text_color(t.fg())
                            .child(SharedString::from(it.label.clone())),
                    )
                    .child(self.small(it.hint.clone(), t.muted()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.palette_run(act.clone(), cx);
                        cx.notify();
                    })),
            );
        }

        Some(
            div()
                .absolute()
                .top(px(60.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .key_context("Palette")
                        .w(px(680.0))
                        .bg(t.panel())
                        .border_1()
                        .border_color(t.accent())
                        .flex()
                        .flex_col()
                        .child(div().px_2().py_1().child(self.palette_input.clone()))
                        .child(div().h(px(1.0)).w_full().bg(t.border()))
                        .child(list)
                        .child(
                            div().px_3().py(px(2.0)).child(self.small(
                                format!(
                                    "{} of {} \u{2014} ctrl-n / ctrl-k move, ctrl-y takes, esc leaves",
                                    matches.len().min(14),
                                    matches.len()
                                ),
                                t.muted(),
                            )),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Toasts expire on the next frame they are late for; the pump redraws
        // often enough that nothing sits there after its time.
        let now = Instant::now();
        self.toasts.retain(|x| !x.expired(now));

        let t = self.t();

        let ms = self.scale;
        let root = div()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NextRow, _, cx| this.move_row(1, cx)))
            .on_action(cx.listener(|this, _: &PrevRow, _, cx| this.move_row(-1, cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.move_row(20, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.move_row(-20, cx)))
            .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.move_row(isize::MIN, cx)))
            .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_row(isize::MAX, cx)))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.step_tab(1, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.step_tab(-1, cx)))
            .on_action(cx.listener(|this, _: &RunActive, _, cx| {
                if let Some(id) = this.tabs.get(this.active).map(|x| x.id) {
                    this.sync_tab_from_inputs(id, cx);
                    this.run_tab(id);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleDetail, _, cx| {
                if let Some(tab) = this.tabs.get_mut(this.active) {
                    tab.detail = match tab.detail {
                        Some(_) => None,
                        None => tab.selected.or(Some(0)),
                    };
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                this.palette_open = !this.palette_open;
                this.palette_sel = 0;
                if this.palette_open {
                    this.palette_input.update(cx, |i, cx| i.set_text(String::new(), cx));
                    let h = this.palette_input.read(cx).focus_handle.clone();
                    window.focus(&h);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                // Escape leaves whatever is in front, innermost first.
                if this.palette_open {
                    this.palette_open = false;
                } else if this.export_open {
                    this.export_open = false;
                } else if this.tabs.get(this.active).map_or(false, |x| x.detail.is_some()) {
                    if let Some(tab) = this.tabs.get_mut(this.active) {
                        tab.detail = None;
                    }
                } else {
                    // Nothing in front: take focus back from the text field, so
                    // the keys work again.
                    window.focus(&this.focus);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _, cx| {
                this.palette_sel = this.palette_sel.saturating_add(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PaletteUp, _, cx| {
                this.palette_sel = this.palette_sel.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PaletteAccept, _, cx| {
                let pick = this.palette_matches(cx).into_iter().nth(this.palette_sel);
                if let Some((_, it)) = pick {
                    this.palette_run(it.act, cx);
                }
                cx.notify();
            }))
            // A column being dragged is tracked at the root, because the mouse
            // leaves the narrow handle long before the drag is over.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some((tab_id, col, from, was)) = this.resizing {
                    let w = (was + (f32::from(ev.position.x) - from)).clamp(40.0, 900.0);
                    if let Some(tab) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                        tab.widths.insert(col, w);
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.resizing.take().is_some() {
                        this.remember();
                        cx.notify();
                    }
                }),
            )
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
            .font_family("CodeNerdFont")
            .text_size(ms.heading());

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
        .children(self.toasts_el())
        .children(self.palette_el(cx))
    }
}
