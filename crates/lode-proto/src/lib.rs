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
    /// What became of each registered driver PROGRAM (`proto >= 13`): its name,
    /// the kind it claimed, whether it loaded, and the reason if it did not.
    ///
    /// Carried as JSON rather than typed, because this is a REPORT and not a
    /// contract: the engine may add a field to it, and a client that has to be
    /// rebuilt to keep reading a diagnostic is a client that stops showing the
    /// diagnostic. An older engine sends none, and an empty list reads as
    /// "none registered" — which is what it means.
    #[serde(default)]
    pub external: Vec<serde_json::Value>,
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
    /// The comparisons this dialect offers. Empty on an older engine and on an
    /// engine with no builder, which the client reads the same way: no builder.
    #[serde(default)]
    pub builder_ops: Vec<BuilderOp>,
    /// How this engine spells a time window, over `{col}`, `{from}`, `{to}`.
    ///
    /// Empty where the engine has no such thing (Redis), and empty on an
    /// engine from before protocol 8 — a client reads both the same way, which
    /// is the right answer for both: no time filter here.
    #[serde(default)]
    pub time_filter: String,
    /// The schema changes this dialect can be asked to WRITE OUT
    /// (`proto >= 23`), as templates — `ALTER TABLE … RENAME COLUMN` is
    /// standard, MySQL before 8 wanted `CHANGE`, and SQL Server wants
    /// `sp_rename`, so the words come from the driver rather than from here.
    ///
    /// Empty from an older engine and empty where the idea does not apply, and
    /// a client that has none offers nothing rather than a statement it made
    /// up.
    #[serde(default)]
    pub ddl_ops: Vec<DdlOp>,
    #[serde(default)]
    pub types: Vec<TypeSpec>,
    /// The live views this engine can be asked about (`proto >= 20`): who is
    /// connected, what is locked, what is running.
    ///
    /// Empty from an older engine, and empty for an engine that has nothing to
    /// show — which is the honest answer for SQLite. Nothing may be guessed
    /// from the driver's name: the catalogues have nothing in common, and a
    /// client that guessed would ask for a view that is not there.
    #[serde(default)]
    pub inspectors: Vec<Inspector>,
}

/// One schema change a driver can write out.
#[derive(Debug, Clone, Deserialize)]
pub struct DdlOp {
    pub op: String,
    pub label: String,
    pub template: String,
    /// What the template needs filled in. Absent from an older engine, which
    /// is why it defaults: a client that gets nothing offers nothing to fill
    /// in and the statement arrives with its holes still in it, exactly as
    /// it used to.
    #[serde(default)]
    pub asks: Vec<DdlAsk>,
}

/// One hole in a `DdlOp`'s template — see the engine's `spec::DdlAsk`.
#[derive(Debug, Clone, Deserialize)]
pub struct DdlAsk {
    pub key: String,
    pub label: String,
    pub kind: AskKind,
    #[serde(default)]
    pub suggest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    Text,
    Type,
    Column,
    Columns,
    Index,
    Flag,
}

/// One type this dialect's columns can be declared with.
#[derive(Debug, Clone, Deserialize)]
pub struct TypeSpec {
    pub name: String,
    #[serde(default)]
    pub args: u8,
    #[serde(default)]
    pub note: String,
}

/// One live view a driver offers.
#[derive(Debug, Clone, Deserialize)]
pub struct Inspector {
    pub name: String,
    pub label: String,
    /// What acting on a row does, as a word a person reads — `terminate`, or
    /// empty where nothing can be done.
    #[serde(default)]
    pub act: String,
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
    render_full(template, name, schema, limit, fragments, clauses, "")
}

/// As `render_with`, plus the projection the builder chose.
pub fn render_full(
    template: &str,
    name: &str,
    schema: Option<&str>,
    limit: usize,
    fragments: &[String],
    clauses: &[String],
    columns: &str,
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
        // No projection chosen means every column, which is what a browse has
        // always meant — the placeholder exists so the BUILDER can narrow it,
        // not so a plain browse changes.
        .replace("{columns}", if columns.is_empty() { "*" } else { columns })
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

/// Write out a schema change, in the words the DRIVER declared.
///
/// Generated, never run: the statement goes into the editor where it can be
/// read and changed first. A schema change that happens because somebody
/// picked a menu item is a schema change nobody reviewed.
///
/// `None` when this driver has no such operation — which is how a key/value
/// store answers "add a column" and how SQL Server would answer an operation
/// it has no spelling for. A client draws nothing rather than a statement it
/// made up.
pub fn render_ddl(
    meta: &DriverMeta,
    op: &str,
    name: &str,
    schema: Option<&str>,
    values: &[(String, String)],
) -> Option<String> {
    let t = meta.ddl_ops.iter().find(|d| d.op == op)?;
    let schema = schema.filter(|s| !s.is_empty());
    let qualified = match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    };
    // `{qualified}` BEFORE `{table}`, or the substitution of the shorter name
    // would eat the longer one's braces.
    let mut out = t
        .template
        .replace("{qualified}", &qualified)
        .replace("{table}", name)
        .replace("{schema}", schema.unwrap_or(""));
    // Then whatever the op asked for, by the key it asked under. The client
    // does not know what the keys mean and does not have to: it collected
    // them because the driver said to.
    for (key, value) in values {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    // Any hole nobody filled keeps its braces rather than becoming an empty
    // space: a statement with `{type}` still in it cannot be run by mistake,
    // which is the same reason the old one wrote `<type>`.
    //
    // A flag that is off was substituted as nothing and left a gap where its
    // word would have been, so the same squeeze the browse templates get:
    // between tokens, never inside a quoted string, which is the whole of a
    // JSON command template.
    Some(collapse_spaces(&out))
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

/// Does this text LOOK like a filesystem path?
///
/// One definition, shared by the engine and by every client, because two
/// notions of "is this a path" is how the two ends end up disagreeing about the
/// same string. It is deliberately a question about SHAPE and not about the
/// filesystem: whether a file is there is a different question, asked later and
/// answered differently.
///
/// Two places need exactly this:
///
///   * The connection window, before it refuses a save — its message says a key
///     file "starts with / or ~", so `~` has to be accepted here.
///   * The engine's redaction. A value out of a path field is normally a path
///     and is the only useful thing in `no such file`; a value that is NOT
///     shaped like one is somebody's passphrase typed into the wrong box, and
///     that must never be printed. Shape is what tells the two apart.
pub fn looks_like_path(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // A newline is not in any path worth printing, and is how a value could
    // smuggle a second line into a log.
    if s.contains(['\n', '\r']) {
        return false;
    }
    // Whitespace is legal in a path and common in a passphrase. A path that
    // needs a space is rare enough to be worth losing here; a passphrase shown
    // in a log is not.
    if s.chars().any(char::is_whitespace) {
        return false;
    }
    s.starts_with('/') || s.starts_with("~/") || s == "~" || s.starts_with("./") || s.starts_with("../")
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
    /// The engine can change ONE addressed row. False on an older engine, which
    /// is the right answer there: it has no `row.write` to call.
    #[serde(default)]
    pub edit: bool,
    /// The engine can report what its server is doing.
    #[serde(default)]
    pub metrics: bool,
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

/// Where a foreign key points.
#[derive(Debug, Clone, Deserialize)]
pub struct Ref {
    pub table: String,
    #[serde(default)]
    pub schema: Option<String>,
    pub column: String,
}

/// One column of a result.
#[derive(Debug, Clone, Deserialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type", default)]
    pub type_name: String,
    /// What the values ARE, as opposed to what the driver calls them:
    /// `timestamp`, `integer`, `float`, `text`, `bool`, `json`, `binary` or
    /// `unknown` (`proto >= 18`).
    ///
    /// Empty from an older engine, which is why nothing here may require it:
    /// everything that reads it falls back to deciding from the values, which
    /// is what this client did before the field existed.
    #[serde(default)]
    pub kind: String,
}

/// One column of an object, as `schema.columns` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct TableColumn {
    pub name: String,
    #[serde(rename = "type", default)]
    pub type_name: String,
    /// Part of the identifying key. A row can be addressed exactly only by
    /// these, and only when there is at least one.
    #[serde(default)]
    pub primary: bool,
    /// What this column points at, when it is a foreign key (`proto >= 22`).
    ///
    /// Absent where there is no reference AND where the driver cannot be
    /// asked, which are different things the engine deliberately does not
    /// distinguish: both mean "nothing is known", and a client that drew "no
    /// references" over a driver that was never asked would be stating a fact
    /// nobody established.
    #[serde(default)]
    pub references: Option<Ref>,
}

/// A chart somebody saved: a statement and how to read it.
#[derive(Debug, Clone, Deserialize)]
pub struct SavedChart {
    pub name: String,
    pub sql: String,
    /// The measured column BY NAME — an index would move the moment the
    /// statement changed a column, and silently measure something else.
    #[serde(default)]
    pub measure: String,
    /// How it is drawn: `bars`, `line`, `area`, `scatter`, `pie`
    /// (`proto >= 19`). Empty from an older engine, or from a chart saved
    /// before the field existed, and that means the client's default.
    #[serde(default)]
    pub kind: String,
}

/// One thing an engine says about itself.
#[derive(Debug, Clone, Deserialize)]
pub struct Metric {
    pub group: String,
    pub name: String,
    pub value: String,
}

/// One comparison the builder offers, as the driver declared it.
#[derive(Debug, Clone, Deserialize)]
pub struct BuilderOp {
    pub label: String,
    pub fragment: String,
}

impl BuilderOp {
    /// Whether this comparison takes a value at all. `IS NULL` does not, and
    /// showing a box that is ignored is how an interface teaches the wrong
    /// thing.
    pub fn takes_value(&self) -> bool {
        self.fragment.contains("{value}")
    }

    pub fn render(&self, col: &str, value: &str) -> String {
        self.fragment.replace("{col}", col).replace("{value}", value)
    }
}

/// One index on an object, as `schema.indexes` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct Index {
    pub name: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub primary: bool,
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
    /// The folder a client files it in (`proto >= 16`), or `None` for none.
    ///
    /// In the SHARED store rather than a client's own file, because two clients
    /// filing one connection in two places is two answers to a question that
    /// has one.
    #[serde(default)]
    pub group: Option<String>,
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

// ── dashboards ─────────────────────────────────────────────────────────────────

/// The marker a chart statement puts where the board's time range should go.
///
/// A statement WITHOUT it is not time-filtered. That is deliberate: no client
/// can know where in somebody's statement a condition belongs — before a GROUP
/// BY, inside a sub-select, nowhere at all — and guessing would put a filter in
/// a place that changes what the chart means. The author says where; a board
/// says which of its charts did not.
pub const TIME_MARKER: &str = "{{time}}";

/// A dashboard, as the engine stores it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dashboard {
    #[serde(default)]
    pub tiles: Vec<Tile>,
    #[serde(default)]
    pub time: Option<TimeRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tile {
    pub kind: TileKind,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// For a chart tile: the saved chart's NAME. Never an index — an index
    /// would move the moment a chart was renamed or removed, and the board
    /// would go on drawing the wrong one under the old heading.
    #[serde(default)]
    pub chart: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileKind {
    Chart,
    Divider,
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    /// The column carrying the time, by name.
    pub column: String,
    /// Both already canonicalised by the engine, which refuses anything that is
    /// not exactly a timestamp before it is stored.
    pub from: String,
    pub to: String,
}

/// One board as it comes back, with the complaint the engine has about it.
#[derive(Debug, Clone, Default)]
pub struct LoadedDashboard {
    pub dashboard: Dashboard,
    /// Why this board is no longer valid — most often a saved chart it names
    /// having been deleted from the other client. `None` when it is fine.
    pub stale: Option<String>,
    pub columns: u32,
}

/// What became of a chart's time filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeFit {
    /// The range went in. The statement is time-filtered.
    Applied,
    /// The statement does not ask for one. It runs unfiltered, and the tile
    /// says so — a range that silently applies to four charts out of six is
    /// worse than one that says which two it missed.
    ///
    /// The default, because a chart nobody has run yet has had nothing done to
    /// it, and "not filtered" is the true thing to say about it.
    #[default]
    NotMarked,
    /// The statement is WRITTEN AROUND a range and there is none set.
    ///
    /// This does not run. The first version took the marker out and left
    /// `… WHERE  ORDER BY …` behind, which is not a statement; the engine
    /// answered with a syntax error, and a syntax error is a terrible way to
    /// say "you have not picked a date". A chart written as
    /// `WHERE {{time}}` is a chart ABOUT a window, and the honest answer when
    /// there is no window is to ask for one.
    NeedsRange,
    /// The statement asks for a range and this engine has no way to spell one —
    /// Redis, or an engine from before protocol 8. Also does not run, for the
    /// same reason.
    Unsupported,
}

impl TimeFit {
    pub fn runnable(self) -> bool {
        matches!(self, TimeFit::Applied | TimeFit::NotMarked)
    }

    /// What to show on the tile. `None` where there is nothing to say.
    pub fn note(self, driver: &str) -> Option<String> {
        match self {
            TimeFit::Applied => None,
            TimeFit::NotMarked => Some("not time-filtered".into()),
            TimeFit::NeedsRange => {
                Some("this chart is written around a time range — set one above".into())
            }
            TimeFit::Unsupported => Some(format!("{driver} has no way to express a time window")),
        }
    }
}

/// Put the board's time range into one chart's statement.
///
/// Every word of the condition comes from the driver's own template, and the
/// two timestamps have already been refused by the engine if they were anything
/// but timestamps. The client contributes a column name and a position.
pub fn apply_time(statement: &str, meta: &DriverMeta, time: Option<&TimeRange>) -> (String, TimeFit) {
    if !statement.contains(TIME_MARKER) {
        return (statement.to_string(), TimeFit::NotMarked);
    }
    let Some(time) = time else {
        return (statement.to_string(), TimeFit::NeedsRange);
    };
    if meta.time_filter.is_empty() {
        return (statement.to_string(), TimeFit::Unsupported);
    }
    let fragment = meta
        .time_filter
        .replace("{col}", &time.column)
        .replace("{from}", &time.from)
        .replace("{to}", &time.to);
    (statement.replace(TIME_MARKER, &fragment), TimeFit::Applied)
}

/// A saved piece of network, by name.
#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub name: String,
    /// The spec as stored, with every credential still a TEMPLATE. Left as
    /// `Value` because the two kinds have different shapes and the client shows
    /// whichever fields are there — the engine is the one that parses it as a
    /// tunnel or a proxy, and it does so on the way in.
    #[serde(default)]
    pub spec: Value,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

/// One audit row.
///
/// The engine's record of something that CHANGED the database, or of a
/// connection opened or closed — including the attempts it refused. A client
/// cannot write to this and cannot delete from it; it can only ask.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditRow {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub ts: Option<i64>,
    /// The saved connection's name, when the client that ran it sent one.
    #[serde(default)]
    pub conn: Option<String>,
    #[serde(default)]
    pub driver: Option<String>,
    /// `execute` | `row.write` | `conn.connect` | `conn.disconnect`.
    pub path: String,
    /// A stable name for the statement's shape — same shape, same fingerprint,
    /// whatever the values were.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Already redacted by the engine, and empty when the engine is running
    /// with the statement column turned down (see `AuditLog::mode`).
    pub statement: String,
    /// The connection's policy at the time this ran.
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub confirmed: bool,
    /// `applied` | `refused` | `failed` | `cancelled` | `connected` |
    /// `disconnected`.
    pub outcome: String,
    #[serde(default)]
    pub affected: Option<i64>,
    #[serde(default)]
    pub ms: Option<i64>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl AuditRow {
    /// Whether this row is something that did NOT happen — a refusal or a
    /// failure. The client paints these differently, because they are the rows
    /// somebody opened the log to find.
    pub fn stopped(&self) -> bool {
        matches!(self.outcome.as_str(), "refused" | "failed" | "cancelled")
    }
}

/// The audit log as the engine hands it over: the rows, and how much of a
/// statement this engine is keeping.
///
/// `mode` travels with the rows so a client can SAY why a statement is blank.
/// A blank statement with no explanation reads as a bug; this one is a setting,
/// and the difference matters to whoever is reading the log.
#[derive(Debug, Clone, Default)]
pub struct AuditLog {
    pub rows: Vec<AuditRow>,
    pub mode: String,
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

    /// The audit log, newest first. `conn` narrows it to one connection by name.
    ///
    /// There is no `clear_audit` to match, and that is deliberate: a log a
    /// client can delete from is a log of what that client chose to keep.
    pub fn audit(&self, limit: i64, conn: Option<&str>, timeout: Duration) -> anyhow::Result<AuditLog> {
        let v = self.call("store.audit.list", json!({ "limit": limit, "conn": conn }), timeout)?;
        Ok(AuditLog {
            rows: serde_json::from_value(v.get("audit").cloned().unwrap_or_else(|| json!([])))?,
            // An engine from before this existed answers without a mode; the
            // honest reading of that is "nothing is being withheld", which is
            // what `full` means.
            mode: v
                .get("mode")
                .and_then(|m| m.as_str())
                .unwrap_or("full")
                .to_string(),
        })
    }

    /// The saved profiles of one kind — `"tunnel"` or `"proxy"`.
    ///
    /// A profile is a reusable piece of network, by name, and is NOT scoped to
    /// a connection: the whole point is that several share one.
    pub fn profiles(&self, kind: &str, timeout: Duration) -> anyhow::Result<Vec<Profile>> {
        let v = self.call("store.profiles.list", json!({ "kind": kind }), timeout)?;
        Ok(serde_json::from_value(
            v.get("profiles").cloned().unwrap_or_else(|| json!([])),
        )?)
    }

    pub fn save_profile(&self, kind: &str, name: &str, spec: &Value, timeout: Duration) -> anyhow::Result<()> {
        self.call(
            "store.profiles.save",
            json!({ "kind": kind, "name": name, "spec": spec }),
            timeout,
        )?;
        Ok(())
    }

    /// Remove a profile. The connections that were USING it come back, because
    /// a removal that takes several connections down silently is a surprise
    /// rather than a choice.
    pub fn remove_profile(&self, kind: &str, name: &str, timeout: Duration) -> anyhow::Result<Vec<String>> {
        let v = self.call("store.profiles.remove", json!({ "kind": kind, "name": name }), timeout)?;
        Ok(serde_json::from_value(
            v.get("was_used_by").cloned().unwrap_or_else(|| json!([])),
        )?)
    }

    pub fn dashboards(&self, conn: &str, timeout: Duration) -> anyhow::Result<Vec<Value>> {
        let v = self.call("store.dashboards.list", json!({ "conn": conn }), timeout)?;
        Ok(serde_json::from_value(
            v.get("dashboards").cloned().unwrap_or_else(|| json!([])),
        )?)
    }

    pub fn dashboard(&self, conn: &str, name: &str, timeout: Duration) -> anyhow::Result<Option<LoadedDashboard>> {
        let v = self.call("store.dashboards.get", json!({ "conn": conn, "name": name }), timeout)?;
        let raw = v.get("dashboard").cloned().unwrap_or(Value::Null);
        if raw.is_null() {
            return Ok(None);
        }
        Ok(Some(LoadedDashboard {
            dashboard: serde_json::from_value(raw)?,
            stale: v.get("stale").and_then(|s| s.as_str()).map(str::to_string),
            columns: v.get("columns").and_then(|c| c.as_u64()).unwrap_or(12) as u32,
        }))
    }

    pub fn save_dashboard(&self, conn: &str, name: &str, d: &Dashboard, timeout: Duration) -> anyhow::Result<()> {
        self.call(
            "store.dashboards.save",
            json!({ "conn": conn, "name": name, "dashboard": d }),
            timeout,
        )?;
        Ok(())
    }

    pub fn remove_dashboard(&self, conn: &str, name: &str, timeout: Duration) -> anyhow::Result<()> {
        self.call("store.dashboards.remove", json!({ "conn": conn, "name": name }), timeout)?;
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

    /// A statement is built from what the DRIVER declared, and a flag that is
    /// off leaves no hole and no double space behind it.
    ///
    /// The templates here are the real ones, copied from `spec.rs`: this is
    /// the client's half of that contract, and a test with an invented
    /// template would be a test of nothing.
    #[test]
    fn a_schema_change_is_written_in_the_dialects_own_words() {
        let pg = meta_from(json!({
            "kind": "postgres", "display": "PostgreSQL",
            "ddl_ops": [
                { "op": "alter_type", "label": "", "asks": [],
                  "template": "ALTER TABLE {qualified} ALTER COLUMN {column} TYPE {type};" },
                { "op": "create_index", "label": "", "asks": [],
                  "template": "CREATE {unique} INDEX {index} ON {qualified} ({columns});" }
            ]
        }));
        assert_eq!(
            render_ddl(&pg, "alter_type", "users", Some("public"), &[
                ("column".into(), "age".into()),
                ("type".into(), "numeric(6, 2)".into()),
            ]),
            Some("ALTER TABLE public.users ALTER COLUMN age TYPE numeric(6, 2);".into())
        );
        // UNIQUE on, and off: the word simply is not there, and `CREATE
        // INDEX` has one space where the hole was.
        assert_eq!(
            render_ddl(&pg, "create_index", "users", None, &[
                ("index".into(), "idx_users_email".into()),
                ("columns".into(), "email".into()),
                ("unique".into(), "UNIQUE".into()),
            ]),
            Some("CREATE UNIQUE INDEX idx_users_email ON users (email);".into())
        );
        assert_eq!(
            render_ddl(&pg, "create_index", "users", None, &[
                ("index".into(), "idx_users_email".into()),
                ("columns".into(), "email, name".into()),
                ("unique".into(), String::new()),
            ]),
            Some("CREATE INDEX idx_users_email ON users (email, name);".into())
        );

        // MONGO's is a command document, and the braces around the key are
        // its syntax rather than a hole — which is the one thing a renderer
        // that squeezed spaces inside quotes would get wrong.
        let mongo = meta_from(json!({
            "kind": "mongodb", "display": "MongoDB",
            "ddl_ops": [{ "op": "create_index", "label": "", "asks": [],
                "template": "{\"createIndexes\": \"{table}\", \"indexes\": \
                             [{\"key\": {{columns}}, \"name\": \"{index}\", \"unique\": {unique}}]}" }]
        }));
        let out = render_ddl(&mongo, "create_index", "people", None, &[
            ("index".into(), "idx_people_email".into()),
            ("columns".into(), "\"email\": 1".into()),
            ("unique".into(), "true".into()),
        ])
        .unwrap();
        let v: Value = serde_json::from_str(&out).expect("a command document that parses");
        assert_eq!(v["createIndexes"], "people");
        assert_eq!(v["indexes"][0]["name"], "idx_people_email");
        assert_eq!(v["indexes"][0]["unique"], true);
        assert_eq!(v["indexes"][0]["key"]["email"], 1);
    }

    /// The time range reaches a chart only where its author put the marker, and
    /// the condition is the DRIVER'S words with values in them.
    #[test]
    fn the_time_range_lands_where_the_marker_is_and_nowhere_else() {
        let pg = meta_from(json!({
            "kind": "postgres", "display": "PostgreSQL",
            "time_filter": "{col} >= '{from}' AND {col} < '{to}'"
        }));
        let range = TimeRange {
            column: "created_at".into(),
            from: "2026-01-01".into(),
            to: "2026-02-01".into(),
        };
        let (sql, fit) = apply_time(
            "SELECT date_trunc('day', created_at) d, count(*) FROM signups WHERE {{time}} GROUP BY 1",
            &pg,
            Some(&range),
        );
        assert_eq!(fit, TimeFit::Applied);
        assert_eq!(
            sql,
            "SELECT date_trunc('day', created_at) d, count(*) FROM signups \
             WHERE created_at >= '2026-01-01' AND created_at < '2026-02-01' GROUP BY 1"
        );
    }

    /// The other half, and the one that has to be SAID on the tile: a statement
    /// with no marker is not filtered, and comes back untouched.
    #[test]
    fn a_statement_with_no_marker_is_not_filtered_and_is_not_changed() {
        let pg = meta_from(json!({ "kind": "postgres", "display": "p", "time_filter": "{col} >= '{from}'" }));
        let range = TimeRange {
            column: "ts".into(),
            from: "2026-01-01".into(),
            to: "2026-02-01".into(),
        };
        let src = "SELECT country, count(*) FROM signups GROUP BY 1";
        let (sql, fit) = apply_time(src, &pg, Some(&range));
        assert_eq!(fit, TimeFit::NotMarked);
        assert!(fit.runnable());
        assert_eq!(sql, src);
    }

    /// An engine with no way to spell a time window — Redis — and an engine
    /// from before protocol 8 look the same from here, and both are honest: the
    /// marker goes, the statement still runs, and the tile is told it was not
    /// filtered.
    /// An engine with no way to spell a time window — Redis — and an engine
    /// from before protocol 8 look the same from here, and neither statement
    /// is run: a chart that asks for a window it cannot have is a chart to
    /// explain, not to execute.
    #[test]
    fn an_engine_with_no_time_filter_does_not_run_a_chart_that_wanted_one() {
        let redis = meta_from(json!({ "kind": "redis", "display": "Redis", "time_filter": "" }));
        let old = meta_from(json!({ "kind": "postgres", "display": "p" }));
        let range = TimeRange {
            column: "ts".into(),
            from: "2026-01-01".into(),
            to: "2026-02-01".into(),
        };
        for meta in [&redis, &old] {
            let (_, fit) = apply_time("KEYS {{time}} *", meta, Some(&range));
            assert_eq!(fit, TimeFit::Unsupported);
            assert!(!fit.runnable());
            assert!(fit.note(&meta.kind).unwrap().contains(&meta.kind));
        }
    }

    /// The bug this enum exists for. Taking the marker out of
    /// `… WHERE {{time}} ORDER BY …` leaves `WHERE ORDER BY`, which is not a
    /// statement — and the engine's answer, `near "ORDER": syntax error`, is a
    /// terrible way to say "you have not picked a date".
    #[test]
    fn a_chart_written_around_a_range_asks_for_one_instead_of_running_broken_sql() {
        let pg = meta_from(json!({ "kind": "postgres", "display": "p", "time_filter": "{col} >= '{from}'" }));
        let src = "SELECT who, amount FROM payments WHERE {{time}} ORDER BY amount DESC";
        let (sql, fit) = apply_time(src, &pg, None);
        assert_eq!(fit, TimeFit::NeedsRange);
        assert!(!fit.runnable(), "this must NOT be sent to a database");
        // Returned untouched, so what the tile can show is the statement as its
        // author wrote it.
        assert_eq!(sql, src);
        assert!(fit.note("postgres").unwrap().contains("time range"));
    }

    /// Tiles name charts. A board read back must still name the same ones.
    #[test]
    fn a_dashboard_round_trips_with_its_charts_named() {
        let d = Dashboard {
            tiles: vec![
                Tile { kind: TileKind::Chart, x: 0, y: 0, w: 6, h: 4, chart: "revenue".into(), text: String::new() },
                Tile { kind: TileKind::Divider, x: 0, y: 4, w: 12, h: 1, chart: String::new(), text: String::new() },
                Tile { kind: TileKind::Text, x: 0, y: 5, w: 12, h: 2, chart: String::new(), text: "Quarter to date".into() },
            ],
            time: Some(TimeRange { column: "ts".into(), from: "2026-01-01".into(), to: "2026-02-01".into() }),
        };
        let back: Dashboard = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back.tiles.len(), 3);
        assert_eq!(back.tiles[0].chart, "revenue");
        assert_eq!(back.tiles[0].kind, TileKind::Chart);
        assert_eq!(back.tiles[2].text, "Quarter to date");
        assert_eq!(back.time.unwrap().column, "ts");
    }

    /// An audit row parses field for field, and `stopped()` divides the rows
    /// the way the panel colours them: the ones that did NOT happen are the
    /// ones somebody opened the log to find.
    #[test]
    fn an_audit_row_parses_and_knows_whether_it_happened() {
        let rows: Vec<AuditRow> = serde_json::from_value(json!([
            {
                "id": 3, "ts": 1700000000, "conn": "prod", "driver": "postgres",
                "path": "execute", "fingerprint": "a1b2c3d4e5f6a7b8",
                "statement": "DELETE FROM people", "policy": "read only",
                "confirmed": true, "outcome": "refused", "affected": null,
                "ms": null, "reason": "this connection is read only"
            },
            {
                "id": 2, "path": "row.write", "statement": "UPDATE t SET a = ?1 WHERE id = ?2",
                "outcome": "applied", "affected": 1, "ms": 4
            },
            {
                "id": 1, "path": "conn.connect", "statement": "sqlite /tmp/x tls=prefer encrypted=false",
                "outcome": "connected"
            }
        ]))
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].policy.as_deref(), Some("read only"));
        assert_eq!(rows[0].reason.as_deref(), Some("this connection is read only"));
        assert_eq!(rows[0].fingerprint.as_deref(), Some("a1b2c3d4e5f6a7b8"));
        assert!(rows[0].stopped(), "a refusal is the row somebody is looking for");
        // The sparse rows: everything absent defaults rather than failing, so a
        // row an older engine wrote still reads.
        assert_eq!(rows[1].affected, Some(1));
        assert!(!rows[1].stopped());
        assert!(rows[2].conn.is_none());
        assert!(!rows[2].confirmed, "an absent confirm is not a confirm");
        assert!(!rows[2].stopped());
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
