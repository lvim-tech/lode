//! The workspace model: live connections, result tabs, background tasks.
//!
//! Split out of `app.rs` because it is the part with rules rather than widgets,
//! and the rules are the part worth testing. Nothing here draws anything.
//!
//! The shape follows one decision: **a tab carries its own connection.** Several
//! connections are open at once and a tab belongs to one of them, so clicking a
//! collection under one connection and a table under another gives two tabs that
//! both keep working. The previous model had a single live connection and tabs
//! that were editor buffers, which could not express that at all.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use lode_proto::{
    BuilderOp, Column, DriverMeta, Index, Node, SavedChart, SavedQuery, TableColumn,
};
use serde_json::Value;

/// How a result is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResultView {
    /// An expandable document, with its field types beside it.
    Tree,
    /// Rows and columns.
    Table,
    /// The raw record.
    Json,
    /// The same rows as bars. Not a different QUERY — a different reading of
    /// the one that ran, which is why it sits with the views and not with the
    /// sub-tabs.
    Chart,
}

impl ResultView {
    pub fn label(self) -> &'static str {
        match self {
            ResultView::Tree => "Tree",
            ResultView::Table => "Table",
            ResultView::Json => "JSON",
            ResultView::Chart => "Chart",
        }
    }

    /// What a driver should open with.
    ///
    /// A document store opens on Tree because a document is a tree and flattening
    /// it into columns is what every other client gets wrong; a relational engine
    /// opens on Table because rows and columns are what it has. Decided from the
    /// engine's own capability flag, not from a driver name.
    pub fn default_for(meta: Option<&DriverMeta>) -> ResultView {
        match meta {
            Some(m) if !m.caps.sql => ResultView::Tree,
            _ => ResultView::Table,
        }
    }
}

/// What a result can be drawn as, read off the rows themselves.
///
/// A chart needs one column to name the bars and at least one to size them, and
/// the second has to be a NUMBER. This decides that from the loaded rows rather
/// than from the column types the driver declared, because a driver that
/// declares nothing (sqlite often does) would otherwise never be chartable, and
/// because a column declared INTEGER can still arrive holding nulls.
#[derive(Debug)]
pub struct ChartShape {
    /// The column whose values name each bar.
    pub label: usize,
    /// The numeric columns that can be plotted, in result order.
    pub values: Vec<usize>,
}

impl ChartShape {
    /// Work out what can be drawn, or say why nothing can.
    pub fn of(columns: &[Column], rows: &[Vec<Value>]) -> Result<ChartShape, String> {
        if rows.is_empty() {
            return Err("nothing has been loaded to draw".into());
        }
        let numeric: Vec<usize> = (0..columns.len())
            .filter(|&i| {
                // A column counts as numeric when every value in it that is not
                // NULL is a number. One stray string means the column is not a
                // measurement, and drawing it as one would invent a zero.
                let seen: Vec<&Value> = rows.iter().filter_map(|r| r.get(i)).collect();
                !seen.is_empty()
                    && seen.iter().any(|v| v.is_number())
                    && seen.iter().all(|v| v.is_number() || v.is_null())
            })
            .collect();
        if numeric.is_empty() {
            return Err("no column in this result holds numbers, so there is nothing to measure".into());
        }
        // The label is the first column that is NOT one of the measurements;
        // failing that, the row number, which is what `label_of` falls back to.
        let label = (0..columns.len())
            .find(|i| !numeric.contains(i))
            .unwrap_or(usize::MAX);
        Ok(ChartShape { label, values: numeric })
    }
}

/// The text under a bar: the label column, or the row number where every column
/// turned out to be a measurement.
pub fn bar_label(tab: &Tab, shape: &ChartShape, row: usize) -> String {
    bar_label_of(&tab.rows, tab.from, shape, row)
}

/// The same, for rows that are not a tab's — a dashboard tile holds its own.
pub fn bar_label_of(rows: &[Vec<Value>], from: usize, shape: &ChartShape, row: usize) -> String {
    if shape.label == usize::MAX {
        return (from + row + 1).to_string();
    }
    rows.get(row)
        .and_then(|r| r.get(shape.label))
        .map(preview)
        .unwrap_or_default()
}

/// The number a bar is drawn from, and the range to draw it against.
///
/// The range starts at zero unless the data goes below it. A bar chart whose
/// axis starts somewhere else exaggerates every difference on it, which is the
/// oldest way to mislead with a chart and is not worth the extra pixels.
pub fn value_range(rows: &[Vec<Value>], col: usize) -> (f64, f64) {
    let mut lo = 0.0f64;
    let mut hi = 0.0f64;
    for r in rows {
        if let Some(n) = r.get(col).and_then(|v| v.as_f64()) {
            lo = lo.min(n);
            hi = hi.max(n);
        }
    }
    if (hi - lo).abs() < f64::EPSILON {
        // Every value the same: give the bars something to be drawn against so
        // they are visible rather than zero-height.
        hi = lo + 1.0;
    }
    (lo, hi)
}

/// The row of sub-tabs inside a result. Only `Documents` does anything yet; the
/// rest are laid out now so that adding them later is not a rearrangement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubTab {
    Documents,
    Schema,
    Aggregate,
    Builder,
}

impl SubTab {
    pub fn label(self) -> &'static str {
        match self {
            SubTab::Documents => "Documents",
            SubTab::Schema => "Schema",
            SubTab::Aggregate => "Aggregate",
            SubTab::Builder => "Builder",
        }
    }

    /// Everything a document store offers; a relational tab shows only the first.
    pub fn all() -> [SubTab; 4] {
        [SubTab::Documents, SubTab::Schema, SubTab::Aggregate, SubTab::Builder]
    }

    pub fn implemented(self) -> bool {
        matches!(self, SubTab::Documents | SubTab::Schema | SubTab::Builder)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug, Default)]
pub enum RunState {
    #[default]
    Idle,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One open connection.
pub struct Live {
    pub name: String,
    pub driver: String,
    pub conn_id: u64,
    pub encrypted: bool,
    pub tunneled: bool,
    pub nodes: Vec<Node>,
    pub queries: Vec<SavedQuery>,
    pub charts: Vec<SavedChart>,
    /// The dashboards saved on this connection, by name. Names only: a list is
    /// for choosing from, and the layout comes when one is opened.
    pub boards: Vec<String>,
}

/// One tab: a query, its connection, and the result it produced.
pub struct Tab {
    pub id: u64,
    /// The connection this tab belongs to, by NAME — stable across a reconnect,
    /// where the engine's numeric id is not.
    pub conn_name: String,
    pub driver: String,
    pub title: String,
    /// The object this tab was opened on, if it came from the tree.
    pub object: Option<(String, Option<String>)>,
    /// The node kind it was opened as — kept because the browse template is
    /// chosen by kind, so reopening the tab has to ask the same question again.
    pub kind: String,
    /// Free-text statement, for engines that are typed.
    pub sql: String,
    /// Structured values, for engines that declare a query form.
    pub fields: HashMap<String, String>,

    pub call_id: Option<u64>,
    pub state: RunState,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub from: usize,
    pub has_more: bool,
    pub total: Option<usize>,
    pub affected: Option<u64>,
    pub ms: u64,
    pub error: Option<String>,
    pub selected: Option<usize>,
    pub view: ResultView,
    pub sub: SubTab,
    /// Which rows are expanded in the Tree view.
    pub expanded: std::collections::HashSet<usize>,
    /// Column widths the user has dragged, by column index.
    pub widths: HashMap<usize, f32>,
    /// Which column the loaded page is sorted by, and which way.
    pub sort: Option<(usize, bool)>,
    /// A row opened in the detail panel beside the grid.
    pub detail: Option<usize>,
    /// What the object's columns ARE, as the engine described them — the only
    /// place the identifying key is known. Empty until `schema.columns` answers,
    /// and empty FOREVER for a tab that is a typed statement rather than an
    /// object, which is also why such a tab is not editable: there is no one
    /// table behind a join to write back to.
    pub meta_columns: Vec<TableColumn>,
    pub indexes: Vec<Index>,
    pub ddl: Option<String>,
    /// A cell being edited: which row, which column.
    pub editing: Option<(usize, usize)>,
    /// What the visual builder has been told so far.
    pub builder: Builder,
    /// Which measurement the chart is drawing, when the result has several.
    pub chart_col: Option<usize>,
    /// The measurement a SAVED chart asked for, by name. Matched to a column
    /// once the result arrives: a name survives a statement that reordered its
    /// columns, and an index does not.
    pub chart_measure: Option<String>,
    /// A write waiting to be confirmed: the statement the ENGINE said it would
    /// run, why it is dangerous, and the plan to repeat with `confirm`.
    pub pending_write: Option<PendingWrite>,
    /// The last statement this tab's browse fields PRODUCED. Kept so a hand
    /// edit can be told from an untouched one: if the box still holds this, the
    /// fields may rewrite it; if it does not, somebody typed in there and their
    /// words win.
    pub rendered: String,
    /// When this tab is a DASHBOARD rather than a result, the board it holds.
    ///
    /// A board tab has no statement, no grid and no row; it draws saved charts
    /// on a grid of twelve columns. Carried on `Tab` rather than in a parallel
    /// list so that everything already true of a tab — it belongs to a
    /// connection, it has a title, it survives a restart, Ctrl-Tab reaches
    /// it — stays true of a board without being written twice.
    pub board: Option<crate::board::Board>,
}

/// A short-lived message in the corner.
///
/// Toasts exist so that something which worked can say so without taking the
/// status line hostage, and so a failure is seen even when the eye is on the
/// grid. What a toast must NEVER be is the only place an error appears: every
/// one of these is also in the console log, which does not expire.
pub struct Toast {
    pub id: u64,
    pub text: String,
    pub bad: bool,
    pub born: Instant,
}

impl Toast {
    /// Three seconds for good news, eight for bad — long enough to read twice.
    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.born) > Duration::from_secs(if self.bad { 8 } else { 3 })
    }
}

/// Where the grid should sit when the bar is dragged to window position `x`.
///
/// The offset GPUI wants is NEGATIVE — it is how far the content has been moved
/// left — and it is clamped to the real overflow, so a drag past either end
/// stops at the end instead of running off into blank space.
///
/// The pointer grabs the MIDDLE of the thumb, so the thumb lands under the
/// pointer rather than starting there, which is what makes a drag feel like it
/// is holding something.
pub fn hbar_offset(x: f32, left: f32, track: f32, total: f32, max: f32) -> f32 {
    if total <= 0.0 || track <= 0.0 {
        return 0.0;
    }
    let frac = (track / total).clamp(0.05, 1.0);
    let at = ((x - left) / track - frac / 2.0).clamp(0.0, 1.0 - frac);
    -(at * total).min(max).max(0.0)
}

/// Where a sideways scroll lands, clamped to what there is.
///
/// GPUI's scroll offset is NEGATIVE as the content moves left, so the range is
/// `-max ..= 0` and not the other way round — which is the kind of thing that
/// looks right, compiles, and scrolls nowhere.
pub fn clamped_scroll(now: f32, dx: f32, max: f32) -> f32 {
    if max <= 0.0 {
        return 0.0;
    }
    (now + dx).clamp(-max, 0.0)
}

/// Score `text` against a typed `needle`, letters in order but not adjacent.
///
/// `None` means it does not match at all. A lower score is a better match: a
/// run of letters together beats the same letters scattered, and a match at the
/// start of a word beats one in the middle, which is what makes "sq" find
/// "sqlite" before "mysql: select".
pub fn fuzzy_score(text: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let pat: Vec<char> = needle.to_lowercase().chars().collect();
    let (mut hi, mut score, mut last) = (0usize, 0i32, None::<usize>);
    for p in &pat {
        let found = hay[hi..].iter().position(|c| c == p)? + hi;
        score += match last {
            // Adjacent: free. A gap costs what it skips.
            Some(l) if found == l + 1 => 0,
            Some(l) => (found - l) as i32,
            // The first letter costs its distance from a word boundary.
            None => {
                let boundary = found == 0
                    || matches!(hay[found - 1], ' ' | '.' | '_' | '-' | '/' | ':');
                if boundary {
                    0
                } else {
                    found as i32 + 2
                }
            }
        };
        last = Some(found);
        hi = found + 1;
    }
    Some(score)
}

/// Render a row as a statement a PERSON can read, for the clipboard.
///
/// This generates SQL, which the client otherwise refuses to do — the
/// difference is where it goes. A statement the client would RUN has to be
/// right in the driver's dialect, and getting that wrong runs the wrong thing.
/// This one goes to the clipboard, where a human reads it, fixes the quoting
/// their database wants and runs it themselves. It is offered only by drivers
/// that said they speak SQL.
///
/// Literals are ANSI: a quote doubles, a NULL becomes `IS NULL` rather than
/// `= NULL`, which matches nothing in every dialect that has ever shipped.
pub fn copy_as_select(table: &str, columns: &[Column], row: &[Value]) -> String {
    let mut parts = Vec::new();
    for (i, c) in columns.iter().enumerate() {
        let v = row.get(i).unwrap_or(&Value::Null);
        parts.push(match v {
            Value::Null => format!("{} IS NULL", c.name),
            _ => format!("{} = {}", c.name, sql_literal(v)),
        });
    }
    if parts.is_empty() {
        return format!("SELECT * FROM {table};");
    }
    format!("SELECT * FROM {table}\n WHERE {};", parts.join("\n   AND "))
}

/// The same row as an INSERT — for copying it into another table or another
/// database. NULL is spelled out, because an omitted column takes a default and
/// that is a different row.
pub fn copy_as_insert(table: &str, columns: &[Column], row: &[Value]) -> String {
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    let vals: Vec<String> = (0..columns.len())
        .map(|i| sql_literal(row.get(i).unwrap_or(&Value::Null)))
        .collect();
    format!(
        "INSERT INTO {table} ({})\nVALUES ({});",
        names.join(", "),
        vals.join(", ")
    )
}

fn sql_literal(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

/// What the visual builder holds.
///
/// It holds CHOICES, not SQL. Turning them into a statement is `render` below,
/// which uses the driver's own template and the driver's own comparison list —
/// so the builder never learns a dialect, and a driver that spells its limit
/// `TOP n` up by the SELECT gets it there without the builder knowing.
#[derive(Default, Clone)]
pub struct Builder {
    /// The columns to show. Empty means every column, which is also what a
    /// plain browse means, so an untouched builder produces the browse.
    pub columns: Vec<String>,
    pub conds: Vec<Cond>,
    pub order: Option<(String, bool)>,
}

#[derive(Clone)]
pub struct Cond {
    pub col: String,
    /// An index into the driver's `builder_ops`.
    pub op: usize,
    pub value: String,
}

impl Builder {
    /// Whether anything has been chosen. An untouched builder must produce
    /// exactly the browse statement, not a differently spelled equivalent.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty() && self.conds.is_empty() && self.order.is_none()
    }

    /// The projection and the clauses, for the driver's template.
    ///
    /// A condition whose operator takes a value and has none is LEFT OUT rather
    /// than rendered half-written: `WHERE name =` makes every run fail, and
    /// `WHERE name = \'\'` quietly returns nothing, and neither is what a
    /// half-typed filter means.
    pub fn render(&self, ops: &[BuilderOp]) -> (String, Vec<String>) {
        let columns = self.columns.join(", ");
        let mut clauses = Vec::new();
        let conds: Vec<String> = self
            .conds
            .iter()
            .filter_map(|c| {
                let op = ops.get(c.op)?;
                if c.col.is_empty() || (op.takes_value() && c.value.trim().is_empty()) {
                    return None;
                }
                Some(op.render(&c.col, c.value.trim()))
            })
            .collect();
        if !conds.is_empty() {
            clauses.push(format!("WHERE {}", conds.join(" AND ")));
        }
        if let Some((col, asc)) = &self.order {
            if !col.is_empty() {
                clauses.push(format!("ORDER BY {col} {}", if *asc { "ASC" } else { "DESC" }));
            }
        }
        (columns, clauses)
    }
}

/// A write the engine has described but not done.
///
/// It holds the ENGINE's statement, not a guess at one: the whole reason the
/// unconfirmed call exists is that what a client thinks the driver will build
/// and what the driver builds are two different strings, and the reassuring one
/// is the client's.
#[derive(Debug, Clone)]
pub struct PendingWrite {
    pub statement: String,
    pub reason: Option<String>,
    pub key: Vec<(String, Value)>,
    pub set: Vec<(String, Value)>,
    pub delete: bool,
    pub row: usize,
}

/// The key that addresses one loaded row, or why there is none.
///
/// `Err` carries the sentence shown to the person. It is deliberately not an
/// Option: "this is not editable" with no reason is how a user concludes the
/// program is broken.
pub fn row_key(tab: &Tab, row: usize) -> Result<Vec<(String, Value)>, String> {
    if tab.object.is_none() {
        return Err("this tab is a statement, not a table — there is no single object to write back to"
            .into());
    }
    if tab.meta_columns.is_empty() {
        return Err("the engine has not described this object's columns yet".into());
    }
    let key: Vec<&TableColumn> = tab.meta_columns.iter().filter(|c| c.primary).collect();
    if key.is_empty() {
        return Err(format!(
            "{} has no primary key, so a row in it cannot be addressed exactly — \
             matching on the other columns would be a guess",
            tab.object.as_ref().map(|(n, _)| n.as_str()).unwrap_or("this object")
        ));
    }
    let Some(values) = tab.rows.get(row) else {
        return Err("that row is not loaded".into());
    };
    let mut out = Vec::new();
    for k in key {
        // The key column has to be IN the result. `SELECT name FROM t` can be
        // read and cannot be written back, and saying so is better than
        // addressing the row by whatever did come back.
        let Some(i) = tab.columns.iter().position(|c| c.name == k.name) else {
            return Err(format!(
                "{} is part of the key but is not in this result, so the row cannot be addressed",
                k.name
            ));
        };
        out.push((k.name.clone(), values.get(i).cloned().unwrap_or(Value::Null)));
    }
    Ok(out)
}

/// Whether one column may be changed: not part of the key, and known to the
/// engine. A column the engine never mentioned is one this result invented —
/// an expression, a count — and there is nothing to write it to.
pub fn column_editable(tab: &Tab, col: usize) -> bool {
    let Some(c) = tab.columns.get(col) else { return false };
    tab.meta_columns
        .iter()
        .find(|m| m.name == c.name)
        .map_or(false, |m| !m.primary)
}

/// Order the loaded rows by one column.
///
/// This sorts the PAGE, not the result — the client does not rewrite the
/// statement, because the statement is the driver's and a client that appended
/// an ORDER BY would be a client that knows SQL. The header says which it is.
///
/// Within a column, values are compared by what they ARE: numbers numerically,
/// so 9 comes before 10; everything else by its text. A NULL sorts last either
/// way, because "no value" is not a small value.
pub fn sort_rows(rows: &mut [Vec<Value>], col: usize, ascending: bool) {
    rows.sort_by(|a, b| {
        let (x, y) = (a.get(col), b.get(col));
        let ord = match (x, y) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, _) | (Some(Value::Null), _) if !matches!(y, None | Some(Value::Null)) => {
                return std::cmp::Ordering::Greater
            }
            (_, None) | (_, Some(Value::Null)) if !matches!(x, None | Some(Value::Null)) => {
                return std::cmp::Ordering::Less
            }
            (Some(Value::Number(p)), Some(Value::Number(q))) => p
                .as_f64()
                .unwrap_or(f64::NAN)
                .partial_cmp(&q.as_f64().unwrap_or(f64::NAN))
                .unwrap_or(std::cmp::Ordering::Equal),
            (Some(p), Some(q)) => {
                let (ps, qs) = (cell_sort_key(p), cell_sort_key(q));
                ps.cmp(&qs)
            }
            _ => std::cmp::Ordering::Equal,
        };
        if ascending {
            ord
        } else {
            ord.reverse()
        }
    });
}

fn cell_sort_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.to_lowercase(),
        other => other.to_string().to_lowercase(),
    }
}

impl Tab {
    pub fn new(id: u64, conn_name: String, driver: String, title: String, meta: Option<&DriverMeta>) -> Self {
        let fields = meta
            .map(|m| {
                m.query_form
                    .iter()
                    .map(|f| (f.key.clone(), f.default.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Tab {
            id,
            conn_name,
            driver,
            title,
            object: None,
            kind: String::new(),
            sql: String::new(),
            fields,
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
            view: ResultView::default_for(meta),
            sub: SubTab::Documents,
            expanded: Default::default(),
            widths: HashMap::new(),
            sort: None,
            detail: None,
            meta_columns: Vec::new(),
            indexes: Vec::new(),
            ddl: None,
            editing: None,
            builder: Builder::default(),
            chart_col: None,
            chart_measure: None,
            pending_write: None,
            rendered: String::new(),
            board: None,
        }
    }

    /// Clear everything the previous run produced, keeping the query.
    pub fn reset_result(&mut self) {
        self.state = RunState::Running;
        self.error = None;
        self.rows.clear();
        self.columns.clear();
        self.from = 0;
        self.total = None;
        self.affected = None;
        self.selected = None;
        self.expanded.clear();
        // A new result is not the old one re-ordered: its columns may differ.
        self.sort = None;
        self.detail = None;
        // A new result may not have the column the chart was drawing.
        self.chart_col = None;
    }

    /// The statement this tab would run: assembled from the engine's own
    /// template when the driver declares a form, the typed text otherwise.
    pub fn statement(&self, meta: Option<&DriverMeta>, limit: usize) -> String {
        match meta {
            Some(m) if m.is_structured() => {
                let (name, schema) = match &self.object {
                    Some((n, s)) => (n.as_str(), s.as_deref()),
                    // No object: the title is the best name there is, which is
                    // what a tab opened with `+` has.
                    None => (self.title.as_str(), None),
                };
                lode_proto::render_query(m, &self.fields, name, schema, limit)
            }
            _ => self.sql.clone(),
        }
    }

    /// The document behind row `i`, for the Tree and JSON views.
    ///
    /// A document driver already sends the whole record in a `_document`
    /// column — rebuilding one by zipping the flattened columns back together
    /// would lose the field order and any key that did not become a column.
    /// Falling back to that zip is still right for a relational row, which has
    /// no document of its own.
    pub fn document(&self, i: usize) -> Value {
        let row = match self.rows.get(i) {
            Some(r) => r,
            None => return Value::Null,
        };
        if let Some(ci) = self.columns.iter().position(|c| c.name == "_document") {
            if let Some(v) = row.get(ci) {
                if let Value::String(s) = v {
                    if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                        return parsed;
                    }
                }
                if !v.is_null() {
                    return v.clone();
                }
            }
        }
        let mut m = serde_json::Map::new();
        for (ci, c) in self.columns.iter().enumerate() {
            if c.name == "_document" {
                continue;
            }
            m.insert(c.name.clone(), row.get(ci).cloned().unwrap_or(Value::Null));
        }
        Value::Object(m)
    }

    /// The columns a GRID should show: everything except the whole-document
    /// column, which is the source for the other two views and noise here.
    pub fn grid_columns(&self) -> Vec<(usize, &Column)> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name != "_document")
            .collect()
    }
}

/// One thing the engine is doing, or did.
///
/// A query that takes thirty seconds must not take the window with it, and when
/// it fails the reason has to still be somewhere. Both are the same list.
pub struct Task {
    pub id: u64,
    pub label: String,
    pub started: Instant,
    pub elapsed: Option<Duration>,
    pub error: Option<String>,
    /// Shown expanded under its row; a failure starts expanded, because the
    /// point of keeping it is that it is read.
    pub open: bool,
}

impl Task {
    pub fn new(id: u64, label: String) -> Self {
        Task {
            id,
            label,
            started: Instant::now(),
            elapsed: None,
            error: None,
            open: false,
        }
    }

    pub fn running(&self) -> bool {
        self.elapsed.is_none()
    }

    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// How long it has taken so far, or took.
    pub fn duration(&self) -> Duration {
        self.elapsed.unwrap_or_else(|| self.started.elapsed())
    }

    pub fn finish(&mut self, elapsed: Duration, error: Option<String>) {
        self.elapsed = Some(elapsed);
        self.open = error.is_some();
        self.error = error;
    }
}

/// Human duration: milliseconds while small, seconds once that stops reading.
pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

/// The short name for a JSON value's type, as a badge.
///
/// Top-level fields get the DRIVER's own type name where it gave one — a
/// MongoDB `ObjectId` is not "string" and saying so would be the client
/// inventing a type system. Nested values fall back to the JSON shape, which is
/// all anyone has for them.
pub fn type_badge(v: &Value, declared: Option<&str>) -> &'static str {
    if let Some(d) = declared {
        let d = d.trim();
        if !d.is_empty() && d != "json" {
            return match d.to_ascii_lowercase().as_str() {
                "objectid" | "oid" => "oid",
                "date" | "datetime" | "timestamp" => "date",
                "int" | "int32" | "int64" | "long" | "integer" => "int",
                "double" | "decimal" | "float" | "number" => "num",
                "bool" | "boolean" => "bool",
                "array" => "array",
                "object" | "document" => "object",
                _ => "field",
            };
        }
    }
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_f64() {
                "num"
            } else {
                "int"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A one-line preview of a value, for a collapsed tree node.
pub fn preview(v: &Value) -> String {
    match v {
        Value::Object(m) => format!("{{{}}}", m.len()),
        Value::Array(a) => format!("[{}]", a.len()),
        Value::Null => "null".into(),
        Value::String(s) => {
            let one = s.replace('\n', "⏎");
            if one.chars().count() > 80 {
                format!("{}…", one.chars().take(80).collect::<String>())
            } else {
                one
            }
        }
        other => other.to_string(),
    }
}

/// Whether a tree/list entry survives the sidebar filter. Case-insensitive
/// substring, which is what a filter box is for; anything cleverer would need
/// explaining.
pub fn matches_filter(text: &str, filter: &str) -> bool {
    let f = filter.trim();
    f.is_empty() || text.to_lowercase().contains(&f.to_lowercase())
}

/// Whether a node, or anything under it, survives the filter — so filtering a
/// tree does not hide the branch a match lives on.
pub fn node_matches(node: &Node, filter: &str) -> bool {
    if matches_filter(&node.name, filter) {
        return true;
    }
    node.children.iter().any(|c| node_matches(c, filter))
}

#[cfg(test)]
mod tests {

    /// Dragging the bar lands somewhere sensible and never past the ends.
    ///
    /// The bar had no drag at all — it was two coloured divs. A bar that looks
    /// like a control and is not one is worse than no bar: somebody tries it,
    /// nothing happens, and they conclude the grid does not scroll sideways.
    #[test]
    fn dragging_the_bar_stays_within_what_there_is_to_scroll() {
        // A grid twice as wide as the window showing it.
        let (left, track, total) = (300.0_f32, 900.0_f32, 1800.0_f32);
        let max = total - track;

        // Hard left, and well past it: both stop at zero.
        assert_eq!(hbar_offset(left, left, track, total, max), 0.0);
        assert_eq!(hbar_offset(left - 500.0, left, track, total, max), 0.0);

        // Hard right, and well past it: both stop at the real overflow.
        assert_eq!(hbar_offset(left + track, left, track, total, max), -max);
        assert_eq!(hbar_offset(left + track + 500.0, left, track, total, max), -max);

        // The middle of the track shows the middle of the grid.
        let mid = hbar_offset(left + track / 2.0, left, track, total, max);
        assert!(mid < 0.0 && mid > -max, "{mid}");
        assert!((mid + max / 2.0).abs() < 1.0, "the middle should be the middle: {mid}");

        // Monotonic: further right is never further left.
        let mut last = 1.0_f32;
        let mut x = left - 50.0;
        while x <= left + track + 50.0 {
            let v = hbar_offset(x, left, track, total, max);
            assert!(v <= last + 0.001, "went backwards at {x}: {v} after {last}");
            last = v;
            x += 10.0;
        }

        // Nothing to scroll: nothing moves, and no division by zero.
        assert_eq!(hbar_offset(left + 10.0, left, track, track, 0.0), 0.0);
        assert_eq!(hbar_offset(left + 10.0, left, 0.0, 0.0, 0.0), 0.0);
    }
    /// The offset is negative as the content moves LEFT. Getting the sign wrong
    /// here compiles and scrolls nowhere, which is why it is a function with a
    /// test rather than three lines inside a click handler.
    #[test]
    fn a_sideways_scroll_stays_inside_what_there_is() {
        // Right by one column, from the start.
        assert_eq!(clamped_scroll(0.0, -154.0, 1748.0), -154.0);
        // And back again, no further than the start.
        assert_eq!(clamped_scroll(-154.0, 154.0, 1748.0), 0.0);
        assert_eq!(clamped_scroll(0.0, 1000.0, 1748.0), 0.0);
        // Never past the last column.
        assert_eq!(clamped_scroll(-1700.0, -1000.0, 1748.0), -1748.0);
        // A grid that fits does not move at all, whichever way it is pushed.
        assert_eq!(clamped_scroll(0.0, -500.0, 0.0), 0.0);
        assert_eq!(clamped_scroll(0.0, 500.0, 0.0), 0.0);
    }

    use super::*;
    use serde_json::json;

    fn meta(kind: &str, sql: bool, structured: bool) -> DriverMeta {
        serde_json::from_value(json!({
            "kind": kind, "display": kind,
            "caps": { "sql": sql },
            "query_template": if structured { "{\"find\": \"{name}\"{fragments}}" } else { "" },
            "query_form": if structured {
                json!([{ "key": "filter", "label": "Filter", "fragment": "\"filter\": {value}" },
                       { "key": "limit", "label": "Limit", "fragment": "\"limit\": {value}", "default": "50" }])
            } else { json!([]) },
        }))
        .unwrap()
    }

    /// A document store opens as a document; a relational engine as a grid.
    /// From the capability flag, not from a driver name.
    #[test]
    fn the_opening_view_follows_the_engines_own_capability() {
        assert_eq!(ResultView::default_for(Some(&meta("mongodb", false, true))), ResultView::Tree);
        assert_eq!(ResultView::default_for(Some(&meta("postgres", true, false))), ResultView::Table);
        assert_eq!(ResultView::default_for(None), ResultView::Table, "unknown falls back to the grid");
    }

    /// A new tab starts with the form's declared defaults, so the limit box is
    /// not empty on a driver that said what it should be.
    #[test]
    fn a_tab_starts_with_the_declared_defaults() {
        let m = meta("mongodb", false, true);
        let t = Tab::new(1, "c".into(), "mongodb".into(), "db.coll".into(), Some(&m));
        assert_eq!(t.fields.get("limit").map(String::as_str), Some("50"));
        assert_eq!(t.fields.get("filter").map(String::as_str), Some(""));
    }

    /// The statement comes from the engine's template for a structured driver
    /// and from the typed text otherwise.
    #[test]
    fn the_statement_is_assembled_or_typed_according_to_the_driver() {
        let mongo = meta("mongodb", false, true);
        let mut t = Tab::new(1, "c".into(), "mongodb".into(), "schoolexams.ratings".into(), Some(&mongo));
        t.object = Some(("ratings".into(), Some("schoolexams".into())));
        assert_eq!(t.statement(Some(&mongo), 200), "{\"find\": \"ratings\", \"limit\": 50}");
        t.fields.insert("filter".into(), "{\"a\": 1}".into());
        assert_eq!(
            t.statement(Some(&mongo), 200),
            "{\"find\": \"ratings\", \"filter\": {\"a\": 1}, \"limit\": 50}"
        );

        let pg = meta("postgres", true, false);
        let mut u = Tab::new(2, "c".into(), "postgres".into(), "t".into(), Some(&pg));
        u.sql = "SELECT 1".into();
        assert_eq!(u.statement(Some(&pg), 200), "SELECT 1", "a typed driver runs what was typed");
    }

    /// A document driver sends the whole record; the tree must use it rather
    /// than reassemble one out of the flattened columns.
    #[test]
    fn the_document_comes_from_the_drivers_own_column() {
        let m = meta("mongodb", false, true);
        let mut t = Tab::new(1, "c".into(), "mongodb".into(), "x".into(), Some(&m));
        t.columns = vec![
            Column { name: "name".into(), type_name: String::new() },
            Column { name: "_document".into(), type_name: "json".into() },
        ];
        t.rows = vec![vec![json!("ada"), json!("{\"name\":\"ada\",\"deep\":{\"x\":1}}")]];
        let doc = t.document(0);
        assert_eq!(doc["deep"]["x"], json!(1), "a nested key no column carried survived");
        assert_eq!(t.grid_columns().len(), 1, "_document is not a grid column");
    }

    /// A relational row has no document of its own, so one is built from the
    /// columns — every column, in order, including nulls.
    #[test]
    fn a_relational_row_becomes_an_object() {
        let m = meta("postgres", true, false);
        let mut t = Tab::new(1, "c".into(), "postgres".into(), "x".into(), Some(&m));
        t.columns = vec![
            Column { name: "id".into(), type_name: "int4".into() },
            Column { name: "note".into(), type_name: "text".into() },
        ];
        t.rows = vec![vec![json!(1), Value::Null]];
        assert_eq!(t.document(0), json!({ "id": 1, "note": null }));
        assert_eq!(t.grid_columns().len(), 2);
    }

    /// Numbers compare as numbers and NULL goes last, both ways — a NULL is not
    /// a small value and sorting it to the top would say it was.
    fn tabcol(name: &str, primary: bool) -> TableColumn {
        TableColumn { name: name.into(), type_name: String::new(), primary }
    }

    fn table_tab() -> Tab {
        let m = meta("sqlite", true, false);
        let mut t = Tab::new(1, "c".into(), "sqlite".into(), "people".into(), Some(&m));
        t.object = Some(("people".into(), Some("main".into())));
        t.columns = vec![col("id"), col("name")];
        t.rows = vec![vec![json!(7), json!("Ada")]];
        t.meta_columns = vec![tabcol("id", true), tabcol("name", false)];
        t
    }

    fn ops() -> Vec<BuilderOp> {
        vec![
            BuilderOp { label: "=".into(), fragment: "{col} = {value}".into() },
            BuilderOp { label: "IS NULL".into(), fragment: "{col} IS NULL".into() },
        ]
    }

    /// An untouched builder has to produce the browse EXACTLY — not a
    /// differently spelled equivalent, which would make opening the Builder tab
    /// and touching nothing change what runs.
    /// A column with one stray string in it is not a measurement. Treating it
    /// as one would silently draw that row as zero, which is a chart that is
    /// wrong rather than a chart that is missing.
    #[test]
    fn a_column_with_a_stray_string_is_not_a_measurement() {
        let cols = vec![col("name"), col("n")];
        let good = vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]];
        let shape = ChartShape::of(&cols, &good).expect("two numbers make a measurement");
        assert_eq!(shape.values, vec![1]);
        assert_eq!(shape.label, 0);

        let bad = vec![vec![json!("a"), json!(1)], vec![json!("b"), json!("oops")]];
        let e = ChartShape::of(&cols, &bad).unwrap_err();
        assert!(e.contains("no column"), "{e}");
    }

    /// A NULL among numbers does not disqualify the column — it is a missing
    /// measurement, not a non-measurement.
    #[test]
    fn nulls_do_not_stop_a_column_being_numeric() {
        let cols = vec![col("name"), col("n")];
        let rows = vec![vec![json!("a"), json!(1)], vec![json!("b"), Value::Null]];
        assert_eq!(ChartShape::of(&cols, &rows).unwrap().values, vec![1]);
    }

    #[test]
    fn an_empty_result_says_so_rather_than_drawing_nothing() {
        let e = ChartShape::of(&[col("a")], &[]).unwrap_err();
        assert!(e.contains("nothing has been loaded"), "{e}");
    }

    /// The axis starts at zero unless the data goes below it. A bar chart that
    /// starts its axis elsewhere exaggerates every difference on it.
    #[test]
    fn the_axis_includes_zero() {
        let rows = vec![vec![json!(10)], vec![json!(12)]];
        assert_eq!(value_range(&rows, 0), (0.0, 12.0));

        let with_negatives = vec![vec![json!(-5)], vec![json!(12)]];
        assert_eq!(value_range(&with_negatives, 0), (-5.0, 12.0));
    }

    /// All the same value still draws: a flat chart is a fact about the data,
    /// and bars of zero height look like a broken window.
    #[test]
    fn a_flat_column_still_has_a_range() {
        let rows = vec![vec![json!(7)], vec![json!(7)]];
        let (lo, hi) = value_range(&rows, 0);
        assert!(hi > lo);
    }

    #[test]
    fn an_untouched_builder_changes_nothing() {
        let b = Builder::default();
        assert!(b.is_empty());
        let (cols, clauses) = b.render(&ops());
        assert_eq!(cols, "");
        assert!(clauses.is_empty());
    }

    #[test]
    fn conditions_become_one_where_joined_with_and() {
        let b = Builder {
            columns: vec!["id".into(), "name".into()],
            conds: vec![
                Cond { col: "balance".into(), op: 0, value: "0".into() },
                Cond { col: "note".into(), op: 1, value: String::new() },
            ],
            order: Some(("balance".into(), false)),
        };
        let (cols, clauses) = b.render(&ops());
        assert_eq!(cols, "id, name");
        assert_eq!(clauses, vec!["WHERE balance = 0 AND note IS NULL", "ORDER BY balance DESC"]);
    }

    /// A filter someone is halfway through typing is left OUT. Rendering it
    /// half-written either breaks every run or silently matches nothing, and
    /// both are worse than it not being there yet.
    #[test]
    fn a_half_typed_condition_is_left_out() {
        let b = Builder {
            conds: vec![Cond { col: "name".into(), op: 0, value: "   ".into() }],
            ..Default::default()
        };
        let (_, clauses) = b.render(&ops());
        assert!(clauses.is_empty(), "{clauses:?}");
    }

    /// An operator that takes no value does not need one — the box is not shown
    /// for it, so requiring one would make it unusable.
    #[test]
    fn an_operator_without_a_value_still_renders() {
        let b = Builder {
            conds: vec![Cond { col: "note".into(), op: 1, value: String::new() }],
            ..Default::default()
        };
        let (_, clauses) = b.render(&ops());
        assert_eq!(clauses, vec!["WHERE note IS NULL"]);
    }

    #[test]
    fn a_row_is_addressed_by_its_key_and_nothing_else() {
        let t = table_tab();
        let k = row_key(&t, 0).expect("a keyed table addresses its row");
        assert_eq!(k.len(), 1, "only the key, not every column");
        assert_eq!(k[0].0, "id");
        assert_eq!(k[0].1, json!(7));
    }

    /// Every refusal carries a sentence. "Not editable" with no reason is how
    /// somebody decides the program is broken.
    #[test]
    fn a_table_with_no_key_says_why_it_cannot_be_edited() {
        let mut t = table_tab();
        t.meta_columns = vec![tabcol("id", false), tabcol("name", false)];
        let e = row_key(&t, 0).unwrap_err();
        assert!(e.contains("no primary key"), "{e}");
        assert!(e.contains("guess"), "it has to say what the alternative would be: {e}");
    }

    /// A key column missing from the RESULT is just as unaddressable as a table
    /// with no key — `SELECT name FROM people` is readable and not writable.
    #[test]
    fn a_key_column_left_out_of_the_result_cannot_be_written_back() {
        let mut t = table_tab();
        t.columns = vec![col("name")];
        t.rows = vec![vec![json!("Ada")]];
        let e = row_key(&t, 0).unwrap_err();
        assert!(e.contains("id"), "{e}");
        assert!(e.contains("cannot be addressed"), "{e}");
    }

    #[test]
    fn a_typed_tab_is_not_a_table() {
        let m = meta("sqlite", true, false);
        let t = Tab::new(1, "c".into(), "sqlite".into(), "x".into(), Some(&m));
        let e = row_key(&t, 0).unwrap_err();
        assert!(e.contains("statement, not a table"), "{e}");
    }

    #[test]
    fn the_key_column_is_not_editable_and_an_invented_one_is_not_either() {
        let mut t = table_tab();
        assert!(!column_editable(&t, 0), "the key addresses the row; changing it moves it");
        assert!(column_editable(&t, 1));
        t.columns.push(col("row_number"));
        assert!(
            !column_editable(&t, 2),
            "a column the engine never mentioned has nothing behind it to write to"
        );
    }

    #[test]
    fn letters_in_order_find_a_thing_and_a_tighter_run_wins() {
        assert!(fuzzy_score("Export as CSV", "ecsv").is_some());
        assert!(fuzzy_score("Export as CSV", "zz").is_none());
        // Everything matches the empty needle, so an empty palette shows all.
        assert_eq!(fuzzy_score("anything", ""), Some(0));

        let tight = fuzzy_score("sqlite", "sql").unwrap();
        let loose = fuzzy_score("set query limit", "sql").unwrap();
        assert!(tight < loose, "a run together beats letters scattered: {tight} vs {loose}");
    }

    /// A match at the start of a WORD beats one buried mid-token — which is why
    /// typing "qu" finds "Saved queries" rather than "Unique index".
    #[test]
    fn a_word_boundary_beats_the_middle_of_a_word() {
        let boundary = fuzzy_score("saved queries", "qu").unwrap();
        let middle = fuzzy_score("unique", "qu").unwrap();
        assert!(boundary < middle, "{boundary} vs {middle}");
    }

    /// NULL has to become `IS NULL`: `= NULL` matches nothing, in every dialect
    /// that has ever shipped, so a copied row would silently select none.
    #[test]
    fn a_copied_row_says_is_null_and_doubles_a_quote() {
        let cols = vec![col("id"), col("name"), col("note")];
        let row = vec![json!(7), json!("O'Hara"), Value::Null];
        let sel = copy_as_select("main.people", &cols, &row);
        assert!(sel.contains("id = 7"), "{sel}");
        assert!(sel.contains("name = \'O\'\'Hara\'"), "{sel}");
        assert!(sel.contains("note IS NULL"), "{sel}");
        assert!(!sel.contains("= NULL"), "a NULL compared with = matches nothing: {sel}");

        let ins = copy_as_insert("main.people", &cols, &row);
        assert!(ins.contains("(id, name, note)"), "{ins}");
        assert!(ins.contains("NULL)"), "an omitted column takes a default, which is a different row: {ins}");
    }

    /// Bad news stays up long enough to read; good news gets out of the way.
    #[test]
    fn a_failure_outlives_a_success() {
        let now = Instant::now();
        let born = now - Duration::from_secs(5);
        let good = Toast { id: 1, text: "saved".into(), bad: false, born };
        let bad = Toast { id: 2, text: "refused".into(), bad: true, born };
        assert!(good.expired(now));
        assert!(!bad.expired(now));
    }

    fn col(name: &str) -> Column {
        Column { name: name.into(), type_name: String::new() }
    }

    #[test]
    fn a_column_sorts_by_what_the_values_are() {
        let mut rows = vec![
            vec![json!(10)],
            vec![json!(9)],
            vec![Value::Null],
            vec![json!(100)],
        ];
        sort_rows(&mut rows, 0, true);
        assert_eq!(rows[0][0], json!(9), "9 before 10, not \"10\" before \"9\"");
        assert_eq!(rows[1][0], json!(10));
        assert_eq!(rows[2][0], json!(100));
        assert_eq!(rows[3][0], Value::Null, "NULL last ascending");

        sort_rows(&mut rows, 0, false);
        assert_eq!(rows[0][0], json!(100));
        assert_eq!(rows[3][0], Value::Null, "NULL last descending too");
    }

    #[test]
    fn text_sorts_case_insensitively() {
        let mut rows = vec![vec![json!("beta")], vec![json!("Alpha")], vec![json!("gamma")]];
        sort_rows(&mut rows, 0, true);
        assert_eq!(rows[0][0], json!("Alpha"));
        assert_eq!(rows[1][0], json!("beta"));
        assert_eq!(rows[2][0], json!("gamma"));
    }

    /// A re-run throws the ordering away: the new result's columns may not even
    /// be the same ones.
    #[test]
    fn a_new_result_forgets_the_sort() {
        let m = meta("postgres", true, false);
        let mut t = Tab::new(1, "c".into(), "postgres".into(), "x".into(), Some(&m));
        t.sort = Some((2, false));
        t.widths.insert(0, 300.0);
        t.reset_result();
        assert!(t.sort.is_none());
        assert_eq!(t.widths.get(&0), Some(&300.0), "a width the user dragged is theirs to keep");
    }

    #[test]
    fn type_badges_prefer_what_the_driver_said() {
        assert_eq!(type_badge(&json!("x"), Some("ObjectId")), "oid");
        assert_eq!(type_badge(&json!("x"), Some("timestamp")), "date");
        assert_eq!(type_badge(&json!(1), Some("int8")), "field", "an unmapped declared type is still a field");
        // Nothing declared, or the catch-all document type: fall back to JSON.
        assert_eq!(type_badge(&json!("x"), None), "string");
        assert_eq!(type_badge(&json!(1.5), Some("json")), "num");
        assert_eq!(type_badge(&Value::Null, Some("")), "null");
        assert_eq!(type_badge(&json!([1]), None), "array");
        assert_eq!(type_badge(&json!({}), None), "object");
    }

    #[test]
    fn a_collapsed_node_previews_its_size_not_its_contents() {
        assert_eq!(preview(&json!({"a": 1, "b": 2})), "{2}");
        assert_eq!(preview(&json!([1, 2, 3])), "[3]");
        assert_eq!(preview(&json!("short")), "short");
        assert_eq!(preview(&json!("a\nb")), "a⏎b");
        let long = "x".repeat(200);
        assert!(preview(&json!(long)).chars().count() <= 81);
    }

    #[test]
    fn durations_switch_units_where_milliseconds_stop_reading() {
        assert_eq!(fmt_duration(Duration::from_millis(18)), "18 ms");
        assert_eq!(fmt_duration(Duration::from_millis(999)), "999 ms");
        assert_eq!(fmt_duration(Duration::from_millis(30_000)), "30.0 s");
    }

    /// Filtering a tree must not hide the branch a match lives on.
    #[test]
    fn a_filter_keeps_the_branch_its_match_is_on() {
        let tree = Node {
            name: "schoolexams".into(),
            kind: "database".into(),
            schema: None,
            children: vec![
                Node { name: "ratings".into(), kind: "collection".into(), schema: None, children: vec![] },
                Node { name: "exams".into(), kind: "collection".into(), schema: None, children: vec![] },
            ],
        };
        assert!(node_matches(&tree, ""), "an empty filter keeps everything");
        assert!(node_matches(&tree, "RAT"), "case-insensitive, and the parent is kept");
        assert!(!node_matches(&tree, "zzz"));
        assert!(matches_filter("schoolexams", "exam"));
    }

    #[test]
    fn a_task_holds_its_failure_open() {
        let mut t = Task::new(1, "find x".into());
        assert!(t.running());
        t.finish(Duration::from_millis(30_000), Some("Connection reset by peer".into()));
        assert!(!t.running());
        assert!(t.failed());
        assert!(t.open, "a failure starts expanded — the point of keeping it is that it is read");
        assert_eq!(fmt_duration(t.duration()), "30.0 s");

        let mut ok = Task::new(2, "find y".into());
        ok.finish(Duration::from_millis(18), None);
        assert!(!ok.failed());
        assert!(!ok.open);
    }

    /// The row shows every sub-tab and marks the ones that do nothing yet. The
    /// test exists so that a sub-tab cannot quietly start LOOKING available
    /// before it is: a tab that is drawn the same as a working one and does
    /// nothing on a click is worse than one that is visibly not ready.
    #[test]
    fn the_row_says_which_sub_tabs_actually_do_something() {
        assert_eq!(SubTab::all().len(), 4);
        for s in [SubTab::Documents, SubTab::Schema, SubTab::Builder] {
            assert!(s.implemented(), "{} is built", s.label());
        }
        assert!(
            !SubTab::Aggregate.implemented(),
            "Aggregate is laid out, not built"
        );
    }
}
