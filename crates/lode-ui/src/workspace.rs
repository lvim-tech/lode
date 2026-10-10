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
    /// The text that was actually SENT, which is not always `sql`.
    ///
    /// With a selection the editor holds a whole file and only part of it is
    /// run. The engine's refusal, the history row and the confirmation card all
    /// have to be about the part that ran, or they are about something that
    /// never reached the database.
    pub ran: String,
    /// Whether this run is already in the engine's history.
    ///
    /// The record used to be written when the state arrived, which is BEFORE
    /// the page — so a SELECT was filed with no row count at all, because the
    /// engine's `affected` is for writes. Now a read waits for its first page
    /// and files the row count it actually got, and this says the filing has
    /// happened so the second page does not file it again.
    pub recorded: bool,
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
    /// A statement the engine called destructive, waiting for a person.
    ///
    /// The console has always asked before running one; a tab did not, so the
    /// same `DROP` went straight through when it was typed in the tab's box
    /// instead. `(statement, reason)` — the reason is the engine's sentence,
    /// which is what makes somebody stop.
    pub confirm: Option<(String, String)>,
    pub selected: Option<usize>,
    /// Which COLUMN the cell cursor is in, as an index into `columns`.
    ///
    /// A row cursor answers "which record"; a cell cursor answers "which
    /// value", and copying one value was the thing this grid could not do at
    /// all — the only way out was an export of everything.
    pub cursor_col: usize,
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
    /// Which document the JSON view has open for editing, if any.
    ///
    /// A whole document at once, not a field at a time: for a document store
    /// that IS the unit somebody thinks in, and all its changed fields go in
    /// one `row.write` — one classification, one confirmation, one audit row.
    pub editing_doc: Option<usize>,
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

/// One audit record, as the LINES it is drawn on.
///
/// Pulled out of the drawing for the same reason the settings groups were: a
/// box that holds two lines of text and is one line tall cannot be told from a
/// correct one without looking, and looking is the thing that is not available
/// here. The renderer builds from this list and takes its HEIGHT from the
/// length of it, so a record with two parts gets a box for two.
///
/// The history panel draws two plain lines and reads correctly; the audit panel
/// drew the same two lines with the first one wrapped in a flex row, and left
/// the height to be inferred. This removes the inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditPart {
    /// The fixed-width outcome column, on the first line only.
    pub outcome: Option<String>,
    pub text: String,
    /// Whether this part WRAPS instead of being cut. A refusal shortened to fit
    /// is a refusal that has stopped explaining itself.
    pub wraps: bool,
}

/// The parts of one audit record, in the order they are drawn.
///
/// `open` adds the reason, the full statement and the shape — the record being
/// looked at is the one that may need more than a line.
pub fn audit_parts(a: &lode_proto::AuditRow, open: bool) -> Vec<AuditPart> {
    let mut parts = Vec::new();
    parts.push(AuditPart {
        outcome: Some(a.outcome.clone()),
        text: if a.statement.is_empty() {
            format!("({} \u{2014} not kept)", a.path)
        } else {
            a.statement.replace('\n', " ")
        },
        wraps: false,
    });
    parts.push(AuditPart {
        outcome: None,
        text: format!(
            "{} \u{b7} {}{}{}{}",
            a.conn.clone().unwrap_or_else(|| "\u{2014}".into()),
            a.path,
            a.policy.as_deref().map(|p| format!(" \u{b7} {p}")).unwrap_or_default(),
            a.affected.map(|n| format!(" \u{b7} {n} row(s)")).unwrap_or_default(),
            if a.confirmed { " \u{b7} confirmed" } else { "" },
        ),
        wraps: false,
    });
    if open {
        if let Some(reason) = a.reason.clone() {
            parts.push(AuditPart { outcome: None, text: reason, wraps: true });
        }
        if !a.statement.is_empty() {
            parts.push(AuditPart { outcome: None, text: a.statement.clone(), wraps: true });
        }
        if let Some(fp) = a.fingerprint.clone() {
            parts.push(AuditPart { outcome: None, text: format!("shape {fp}"), wraps: false });
        }
    }
    parts
}

/// How tall a record's box has to be for the parts it holds.
///
/// Only the parts that do NOT wrap can be counted — a wrapped one is as tall as
/// the text makes it, and the box must be free to grow rather than be told a
/// number that is too small. So this is the FLOOR, applied to the closed rows
/// that make up the list, and an open row sizes itself to its content.
pub fn audit_row_h(parts: &[AuditPart], line_h: f32, pad: f32) -> f32 {
    let fixed = parts.iter().filter(|p| !p.wraps).count().max(1) as f32;
    fixed * line_h + pad * 2.0
}

/// The horizontal scrollbar's arithmetic, as one function.
///
/// The thumb was coming out nearly full while columns were cut off the right —
/// which is one fault with two faces: if the visible width is believed to be
/// larger than it is, the thumb (visible ÷ total) is too long AND the furthest
/// offset (total − visible) is too small, so the last columns cannot be reached.
/// Both symptoms, one number.
///
/// So the number is passed in and the arithmetic is here, where it can be
/// checked without a window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hbar {
    /// The thumb's share of the track, 0..1.
    pub frac: f32,
    /// Where the thumb starts, 0..1.
    pub at: f32,
    /// The furthest the content can be moved left, in pixels.
    pub max: f32,
}

/// `None` when there is nothing to scroll — and then there must be no bar
/// either, because a bar that cannot move is a control that lies.
pub fn hbar(total: f32, shown: f32, offset_x: f32) -> Option<Hbar> {
    if !(total > 0.0) || !(shown > 0.0) {
        return None;
    }
    let max = total - shown;
    if max <= 1.0 {
        return None;
    }
    // A thumb is never allowed to vanish: below a few percent it stops being
    // grabbable, and an ungrabbable thumb is the bar not working again.
    let frac = (shown / total).clamp(0.05, 1.0);
    // `offset_x` is GPUI's, so it is negative or zero.
    let at = ((-offset_x) / total).clamp(0.0, 1.0 - frac);
    Some(Hbar { frac, at, max })
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
            confirm: None,
            cursor_col: 0,
            ran: String::new(),
            recorded: false,
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
            editing_doc: None,
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
        self.recorded = false;
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
    /// The engine call this task is watching, once the engine has named it. It
    /// is what a Stop on the task's own row sends `query.cancel` for — without
    /// it the only way to stop a statement is the tab it was started from, and
    /// a task list that shows something running but cannot stop it is a list of
    /// things to wait for.
    pub call_id: Option<u64>,
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
            call_id: None,
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
        Value::String(_) => "str",
        Value::Array(_) => "array",
        // A TAGGED value is not an ordinary object. The engine sends an
        // ObjectId as `{"__oid": …}` and a date as `{"$date": …}` precisely so
        // a client can name them, and calling either one "object" throws away
        // the thing the tagging was for — and shows somebody `{"$date": …}`
        // where a date belongs.
        Value::Object(m) => {
            // THE ENGINE'S OWN TAGGING, which is `{"__oid": hex}` and
            // `{"__ext": "$date", "v": text}` — checked against what it
            // actually sends rather than taken from another client's
            // screenshot, which is where the first guess at this came from and
            // why `{"$date": …}` was the wrong key to look for.
            if m.contains_key("__oid") {
                return "oid";
            }
            if let Some(ext) = m.get("__ext").and_then(Value::as_str) {
                return match ext {
                    "$date" => "date",
                    "$numberDecimal" | "$numberDouble" => "num",
                    "$numberLong" | "$numberInt" => "int",
                    "$binary" => "bin",
                    "$timestamp" => "ts",
                    _ => "ext",
                };
            }
            // The plain extended-JSON forms too, since another engine or an
            // external driver may send those instead.
            if m.contains_key("$oid") {
                "oid"
            } else if m.contains_key("$date") {
                "date"
            } else if m.contains_key("$numberDecimal") || m.contains_key("$numberDouble") {
                "num"
            } else if m.contains_key("$numberLong") || m.contains_key("$numberInt") {
                "int"
            } else if m.contains_key("$binary") {
                "bin"
            } else {
                "object"
            }
        }
    }
}

/// What a COLUMN holds, and in how many of the rows that came back.
///
/// A document store declares no column types, so the only honest source is the
/// page in hand — and because it is the page and not the collection, the count
/// it is out of has to be carried with it and said. A bare "92%" with no
/// denominator invites somebody to believe it is about the whole collection.
///
/// The type is the one that is not `null` and not missing; when a column holds
/// more than one, `mixed` is the truthful answer rather than whichever came
/// first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSummary {
    /// The word for what this column holds — the same vocabulary the Tree
    /// view's badges use, because one dictionary for three places is the whole
    /// point.
    pub kind: &'static str,
    /// How many rows of the page have this field at all.
    pub present: usize,
    /// Out of how many. Carried so the percentage can never be shown bare.
    pub total: usize,
}

impl ColumnSummary {
    /// Rounded, and only meaningful beside `total`.
    pub fn percent(&self) -> u32 {
        if self.total == 0 {
            return 0;
        }
        ((self.present as f64 / self.total as f64) * 100.0).round() as u32
    }
}

/// Summarise one column across the rows that came back.
///
/// `declared` is whatever the engine said the column is; a driver that knows
/// beats a guess from values every time, which is why it is asked first.
pub fn column_summary(
    rows: &[Vec<Value>],
    col: usize,
    declared: Option<&str>,
) -> ColumnSummary {
    let total = rows.len();
    let mut present = 0usize;
    let mut kind: Option<&'static str> = None;
    let mut mixed = false;
    for r in rows {
        let Some(v) = r.get(col) else { continue };
        // A missing field and a field that is there holding null are different
        // things, and for a document store the difference is the point.
        if v.is_null() {
            continue;
        }
        present += 1;
        let k = type_badge(v, declared);
        match kind {
            None => kind = Some(k),
            Some(prev) if prev != k => mixed = true,
            _ => {}
        }
    }
    ColumnSummary {
        kind: if mixed { "mixed" } else { kind.unwrap_or("null") },
        present,
        total,
    }
}

/// A time window ending now, as the two timestamps the engine wants.
///
/// The engine canonicalises the FORMAT — `time_filter` is each driver's own
/// template over `{col}`, `{from}` and `{to}` — but somebody asking for "the
/// last hour" should not have to type two timestamps to get it, and typing them
/// is how the wrong hour gets asked for.
///
/// UTC, always, and said so in the text: a board shared between two people in
/// two places must mean one window, and a local time in a stored dashboard
/// means two.
pub fn window_ending_now(seconds: u64, now: std::time::SystemTime) -> (String, String) {
    let end = now;
    let start = end - std::time::Duration::from_secs(seconds);
    (stamp(start), stamp(end))
}

/// `YYYY-MM-DDThh:mm:ss`, in UTC, from a `SystemTime`.
///
/// Written out rather than taken from a date crate: this is the one place the
/// client needs a calendar, the arithmetic is fixed by the Gregorian rules, and
/// a dependency for twenty lines is a dependency to keep up to date for ever.
pub fn stamp(at: std::time::SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rest / 3600, (rest % 3600) / 60, rest % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
}

/// Days since 1970-01-01 to a calendar date.
///
/// Howard Hinnant's `civil_from_days`, which is the algorithm every date
/// library uses: it shifts the year to start in March so the leap day is the
/// last day of the year and the month lengths become one linear expression.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Walk a console history.
///
/// `at` is how far BACK from the prompt: `None` is the empty prompt, `Some(0)`
/// the line before it. Up goes further back and stops at the oldest; down comes
/// forward and returns to the empty prompt rather than to the newest line,
/// because the way out of a history is a prompt with nothing in it.
pub fn history_step(at: Option<usize>, back: bool, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match (at, back) {
        (None, true) => Some(0),
        (None, false) => None,
        (Some(i), true) => Some((i + 1).min(len - 1)),
        (Some(0), false) => None,
        (Some(i), false) => Some(i - 1),
    }
}

/// Move the cell cursor sideways, within the columns there are.
///
/// It STOPS at the ends rather than wrapping. A row cursor wraps harmlessly —
/// the row above the first is the last — but a cell cursor that wraps turns one
/// press too many into a jump across the whole table, and the thing somebody
/// was reading is gone.
pub fn step_col(now: usize, by: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    (now as isize + by).clamp(0, count as isize - 1) as usize
}

/// Which tab is already showing this object, if any.
///
/// Clicking a table four times made four tabs of it. The strip filled with
/// copies of one table, and the copies were not even equivalent — three of them
/// held a stale result nobody had re-run.
///
/// A DASHBOARD tab is never a match: it is named after a board, not an object,
/// and two boards can be open at once by design.
pub fn find_tab_for(tabs: &[Tab], conn: &str, name: &str, schema: Option<&str>) -> Option<usize> {
    tabs.iter().position(|t| {
        t.board.is_none()
            && t.conn_name == conn
            && t.object.as_ref().map_or(false, |(n, s)| {
                n == name && s.as_deref() == schema
            })
    })
}

/// Move a tab to the front of the most-recently-used list.
///
/// The list is how ctrl-tab works: pressing it twice must return to where it
/// started, which is what "most recent" means and what stepping left and right
/// through the strip does not do. Twelve tabs and the one you were just in is
/// three presses of `h` away.
pub fn mru_touch(mru: &mut Vec<u64>, tab: u64) {
    mru.retain(|x| *x != tab);
    mru.insert(0, tab);
}

/// The tab ctrl-tab should go to: the most recent OTHER one that still exists.
pub fn mru_next(mru: &[u64], now: u64, alive: &[u64]) -> Option<u64> {
    mru.iter()
        .copied()
        .find(|id| *id != now && alive.contains(id))
        .or_else(|| alive.iter().copied().find(|id| *id != now))
}

/// A name for a tab holding a typed statement.
///
/// The first words of it, on one line. Not the whole thing: a tab titled by a
/// four-line query makes the tab strip as wide as the query, and not "Draft"
/// either — three drafts called Draft cannot be told apart, which is the same
/// fault the saved-query name had.
pub fn draft_title(sql: &str) -> String {
    let one_line = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return "Draft".into();
    }
    let cut: String = one_line.chars().take(40).collect();
    if cut.chars().count() < one_line.chars().count() {
        format!("{}…", cut.trim_end())
    } else {
        cut
    }
}

/// What a run of JSON text IS, for colouring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonRole {
    /// A field's name — the string to the left of a colon.
    Key,
    Str,
    Num,
    /// `true`, `false`, `null`.
    Word,
    /// Braces, brackets, commas, colons, and the whitespace between them.
    Punct,
}

/// Split JSON text into coloured runs.
///
/// The JSON view was ONE colour: a document of forty fields was forty lines of
/// the same ink, and finding a value in it meant reading it. A key, a string, a
/// number and a keyword are four different things and the editor's own syntax
/// colours already name all four.
///
/// A KEY is a string with a colon after it (past any whitespace), which is the
/// whole of what distinguishes one in JSON — there is no other way to tell.
///
/// Lengths are in BYTES and they COVER THE TEXT: the sum of them is the text's
/// length, with no gap and no overlap, because a highlight range that misses a
/// byte boundary is a panic in the text system rather than a wrong colour.
pub fn json_runs(text: &str) -> Vec<(usize, JsonRole)> {
    let b = text.as_bytes();
    let mut out: Vec<(usize, JsonRole)> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        let role = match b[i] {
            b'"' => {
                // Over the string, honouring escapes — a `\"` does not end it.
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                i = i.min(b.len());
                // A colon after it, past whitespace, makes it a key.
                let mut j = i;
                while j < b.len() && (b[j] as char).is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && b[j] == b':' {
                    JsonRole::Key
                } else {
                    JsonRole::Str
                }
            }
            c if c == b'-' || c.is_ascii_digit() => {
                i += 1;
                while i < b.len()
                    && (b[i].is_ascii_digit()
                        || b[i] == b'.'
                        || b[i] == b'e'
                        || b[i] == b'E'
                        || b[i] == b'+'
                        || b[i] == b'-')
                {
                    i += 1;
                }
                JsonRole::Num
            }
            c if c.is_ascii_alphabetic() => {
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                JsonRole::Word
            }
            _ => {
                // Everything else, one byte at a time: a multi-byte character
                // inside no string is not valid JSON, and stepping a byte keeps
                // the run boundaries on char boundaries for the ASCII that is.
                i += 1;
                while i < b.len() && !text.is_char_boundary(i) {
                    i += 1;
                }
                JsonRole::Punct
            }
        };
        let len = i - start;
        // Merge with the run before it when they say the same thing, so a page
        // of punctuation is one run rather than two hundred.
        match out.last_mut() {
            Some((l, r)) if *r == role => *l += len,
            _ => out.push((len, role)),
        }
    }
    out
}

/// Which rows of the palette to draw, so the chosen one is among them.
///
/// It drew the FIRST fourteen and highlighted the chosen one — so pressing
/// ctrl-n fifteen times moved a highlight nobody could see, and enter then took
/// a command that was never on screen. The window slides to keep the selection
/// inside it, with a row of context above where there is one.
pub fn palette_window(sel: usize, n: usize, visible: usize) -> std::ops::Range<usize> {
    if n == 0 || visible == 0 {
        return 0..0;
    }
    let visible = visible.min(n);
    // Keep one row of context above the selection, so the row before it is
    // visible and the list does not read as if the selection were the first
    // thing there is.
    let start = sel.saturating_sub(visible.saturating_sub(2)).min(n - visible);
    let start = start.min(sel);
    start..(start + visible)
}

/// The mark a tab carries for what its statement is doing.
///
/// A tab said nothing about its own state: a query running in the third tab and
/// one that failed in the fifth looked exactly like the four that were idle, so
/// the only way to find out was to go and look. `None` for the states that need
/// no mark — an idle tab and a finished one are the ordinary case, and a mark on
/// every tab is a row of marks nobody reads.
pub fn tab_mark(state: RunState) -> Option<&'static str> {
    match state {
        RunState::Running => Some("\u{f021}"),
        RunState::Failed => Some("\u{f00d}"),
        RunState::Cancelled => Some("\u{f05e}"),
        RunState::Idle | RunState::Done => None,
    }
}

/// Which edge a column's values line up against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Numbers go RIGHT; everything else goes left.
///
/// Not a style preference. A column of right-aligned numbers lines its digits
/// up by place value, so `9`, `81` and `1204` can be compared by their width
/// alone — which is most of what somebody scanning a column of amounts is
/// doing. Left-aligned, the same three are read one at a time.
///
/// A MIXED column stays left: there is no place value to line up when half the
/// cells are words, and the mixture is itself worth seeing.
pub fn cell_align(kind: &str) -> Align {
    match kind {
        "int" | "num" => Align::Right,
        _ => Align::Left,
    }
}

/// How many lines a column heading needs.
///
/// Two when there is something to say beyond the name: the type it holds, or
/// that some of its cells are empty. One otherwise — a header that is always
/// two lines tall spends a row of the grid on white space.
pub fn header_rows(sum: &ColumnSummary) -> usize {
    if sum.kind == "null" && sum.present == sum.total {
        1
    } else {
        2
    }
}

/// A one-line preview of a value, for a collapsed tree node.
pub fn preview(v: &Value) -> String {
    match v {
        // A tagged value is NAMED, not counted: `{"$date": …}` has one key and
        // calling it `{1}` tells somebody nothing at all.
        Value::Object(m) if type_badge(v, None) != "object" => {
            // The VALUE inside the tag, which is the whole reason for tagging:
            // `{"__ext": "$date", "v": "2026-09-15T08:00:00Z"}` is a date and
            // reads as one.
            if let Some(hex) = m.get("__oid").or_else(|| m.get("$oid")).and_then(Value::as_str) {
                return hex.to_string();
            }
            if let Some(inner) = m.get("v") {
                return match inner {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
            }
            if let Some(d) = m.get("$date") {
                return d.as_str().map(str::to_string).unwrap_or_else(|| d.to_string());
            }
            type_badge(v, None).to_string()
        }
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

/// One document, as a line somebody can tell apart from the next one.
///
/// `{6}` is the number of fields and nothing else: thirty documents in a row
/// all say `{6}`, `{5}`, `{7}` and none of them can be told from another
/// without being opened. The first few fields, named and valued, is what makes
/// the list a list of THINGS.
///
/// Cut by characters at the end rather than by a count of fields, so the line
/// fills the row it has instead of stopping short or running over.
pub fn doc_preview(v: &Value, budget: usize) -> String {
    let Value::Object(m) = v else { return preview(v) };
    if m.is_empty() {
        return "{}".into();
    }
    let mut out = String::from("{ ");
    let mut first = true;
    for (k, val) in m {
        if out.chars().count() >= budget {
            break;
        }
        if !first {
            out.push_str(", ");
        }
        first = false;
        let shown = preview(val);
        // A long string inside a preview is cut hard: the point of this line is
        // to tell documents apart, and one field filling it defeats that.
        let shown: String = if shown.chars().count() > 28 {
            format!("{}\u{2026}", shown.chars().take(28).collect::<String>())
        } else {
            shown
        };
        let shown = if matches!(val, Value::String(_)) {
            format!("\"{shown}\"")
        } else {
            shown
        };
        out.push_str(&format!("{k}: {shown}"));
    }
    if out.chars().count() > budget {
        out = out.chars().take(budget).collect();
        out.push('\u{2026}');
    } else {
        out.push_str(" }");
    }
    out
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

/// How long to wait before trying to start the engine again, by attempt.
///
/// **A crashed engine is not the end of the window.** The drivers run in
/// another process precisely so that one of them dying takes nothing else with
/// it, and a client that then says "restart to reconnect" throws away the one
/// advantage of that design. So it tries again — but not forever and not in a
/// tight loop: an engine that cannot start (a missing binary, a broken store)
/// must not become a spawn storm. Three tries, a second apart and then wider,
/// and after that the window says so and offers the button.
pub fn respawn_delay(attempt: u32) -> Option<std::time::Duration> {
    match attempt {
        0 => Some(std::time::Duration::from_secs(1)),
        1 => Some(std::time::Duration::from_secs(3)),
        2 => Some(std::time::Duration::from_secs(8)),
        _ => None,
    }
}

/// Read a typed port, or say what is wrong with it.
///
/// `parse().unwrap_or(22)` is what both tunnel windows did, and it is the worst
/// possible answer: somebody who types `2222 ` with a stray character, or `22o`,
/// gets a profile that silently points at port 22 — a different machine's SSH,
/// or nothing — and the only symptom is a connection that fails later with a
/// message about the host. An empty field IS the default, because a field
/// nobody touched is a question nobody answered; anything else must parse.
pub fn parse_port(text: &str, default: u16) -> Result<u16, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(default);
    }
    match t.parse::<u16>() {
        Ok(0) => Err("port 0 is not a port".into()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("\"{t}\" is not a port number (1-65535)")),
    }
}

/// What one word from the engine means for a task that is watching a call.
#[derive(Debug, PartialEq, Eq)]
pub enum Settle {
    /// The engine is still working; the task keeps counting.
    Running,
    /// It is over. `Some(reason)` is what the row shows in the error colour.
    Finished(Option<String>),
}

/// Read a `query.state` into the task's outcome.
///
/// Used on BOTH paths: the call that still has a tab, and the ORPHAN — a call
/// whose tab was closed while it ran. The orphan is the reason this exists.
/// Its state used to arrive, find no tab, and return early, leaving the task
/// counting upwards for the rest of the session; and `pump()` asks for a
/// repaint for as long as any task is running, so one orphan meant a window
/// that never stopped redrawing.
///
/// A cancelled call is finished with a REASON rather than silently: a row that
/// just stops counting cannot be told apart from one that succeeded.
pub fn task_settled(state: &str, error: Option<&str>) -> Settle {
    match state {
        "done" => Settle::Finished(None),
        "failed" => Settle::Finished(Some(
            error.filter(|e| !e.trim().is_empty()).unwrap_or("query failed").to_string(),
        )),
        "cancelled" => Settle::Finished(Some("cancelled".to_string())),
        _ => Settle::Running,
    }
}

/// Whether a log line is one somebody has to be told about.
///
/// `info` is the running commentary — every connect, every save, every query.
/// Counting it would leave the badge permanently lit, which is the same as not
/// having one. Anything else is a warning or a failure, and those are what the
/// count is for.
pub fn counts_as_unread(level: &str) -> bool {
    level != "info"
}

/// The padlock, closed.
pub const LINK_LOCKED: &str = "\u{f023}";
/// The padlock, open — a link somebody could read off the wire.
pub const LINK_OPEN: &str = "\u{f09c}";

/// What the padlock beside a connection shows, and what it says when asked.
pub struct Link {
    pub glyph: &'static str,
    /// The catalogue key for the tooltip, not a sentence — see `i18n`.
    pub tip: &'static str,
    /// False for a link carrying credentials and rows in the clear. The caller
    /// says it out loud once, at connect; the glyph says it for as long as the
    /// connection is open.
    pub safe: bool,
}

/// Read a connection's transport into the one thing a person needs from it.
///
/// Three outcomes, not four. A tunnel and TLS are both "nobody on the wire can
/// read this", so they share the closed padlock and differ only in the words;
/// `encrypted == false` is the one that matters, and a tunnel flag alongside it
/// does NOT soften it — the engine is the one that says whether the bytes are
/// encrypted, and when it says no, no is the answer.
pub fn link_label(encrypted: bool, tunneled: bool) -> Link {
    match (encrypted, tunneled) {
        (false, _) => Link { glyph: LINK_OPEN, tip: "tip.link_plain", safe: false },
        (true, true) => Link { glyph: LINK_LOCKED, tip: "tip.link_tunnel", safe: true },
        (true, false) => Link { glyph: LINK_LOCKED, tip: "tip.link_tls", safe: true },
    }
}

/// The engine's refusal of an unconfirmed destructive statement, read back as
/// the reason it gave.
///
/// The client asks `query.classify` first, but a statement can turn dangerous
/// between the two calls (a view replaced under it), and an engine older than
/// `classify` only ever refuses at `execute`. Either way the answer carries the
/// reason, and a reason is something to show rather than a red line to read.
pub fn needs_confirm_from_error(error: &str) -> Option<String> {
    error
        .strip_prefix("needs confirming:")
        .map(|rest| rest.trim().to_string())
        .filter(|rest| !rest.is_empty())
}

/// Which connection a command typed into the console belongs to.
///
/// **The one in front of the person, not the one that happened to connect
/// first.** With two databases open, the console sent every line to
/// `conns.first()` — so a `DROP` typed while looking at the second one landed
/// in the first. The active tab names its connection; a tab whose connection
/// has since dropped yields nothing, and the caller says so rather than
/// quietly choosing somebody else's database.
pub fn pick_target(active_tab_conn: Option<&str>, connected: &[String]) -> Option<String> {
    match active_tab_conn {
        Some(name) if connected.iter().any(|c| c == name) => Some(name.to_string()),
        Some(_) => None,
        None => connected.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::pick_target;

    /// Three tries, each further apart, and then the window stops trying.
    #[test]
    fn the_engine_is_given_three_chances() {
        assert_eq!(super::respawn_delay(0), Some(std::time::Duration::from_secs(1)));
        assert!(super::respawn_delay(1).unwrap() > super::respawn_delay(0).unwrap());
        assert!(super::respawn_delay(2).unwrap() > super::respawn_delay(1).unwrap());
        assert_eq!(super::respawn_delay(3), None, "a fourth try is a spawn storm");
    }

    /// A timestamp this client writes is one the engine's templates accept, and
    /// it is in UTC.
    #[test]
    fn a_stamp_is_utc_and_the_right_shape() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(super::stamp(UNIX_EPOCH), "1970-01-01T00:00:00");
        // A leap year, and the day after the leap day.
        assert_eq!(
            super::stamp(UNIX_EPOCH + Duration::from_secs(951_868_800)),
            "2000-03-01T00:00:00"
        );
        // 2026-10-10T12:34:56 — a date in the ordinary range, worked out by
        // hand from the epoch so the test does not use the thing it checks.
        let d = 20_736 * 86_400 + 12 * 3600 + 34 * 60 + 56;
        assert_eq!(super::stamp(UNIX_EPOCH + Duration::from_secs(d)), "2026-10-10T12:34:56");
        // The shape, whatever the instant: nineteen characters and a `T`.
        let s = super::stamp(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(s.len(), 19, "{s}");
        assert_eq!(&s[10..11], "T");
    }

    /// A preset is a window that ENDS NOW and starts the asked-for distance
    /// back, in that order.
    #[test]
    fn a_preset_window_runs_backwards_from_now() {
        use std::time::{Duration, UNIX_EPOCH};
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (from, to) = super::window_ending_now(3600, now);
        assert_eq!(to, super::stamp(now));
        assert_eq!(from, super::stamp(now - Duration::from_secs(3600)));
        assert!(from < to, "{from} .. {to}");
        // A week is a week.
        let (w, _) = super::window_ending_now(7 * 86_400, now);
        assert_eq!(w, super::stamp(now - Duration::from_secs(604_800)));
    }

    /// Up walks back through the console's history; down comes out of it.
    #[test]
    fn the_console_history_has_a_way_out() {
        assert_eq!(super::history_step(None, true, 3), Some(0), "the line before the prompt");
        assert_eq!(super::history_step(Some(0), true, 3), Some(1));
        assert_eq!(super::history_step(Some(2), true, 3), Some(2), "stops at the oldest");
        // Forward, and out to an EMPTY prompt rather than to the newest line.
        assert_eq!(super::history_step(Some(1), false, 3), Some(0));
        assert_eq!(super::history_step(Some(0), false, 3), None);
        assert_eq!(super::history_step(None, false, 3), None);
        // Nothing to walk.
        assert_eq!(super::history_step(None, true, 0), None);
    }

    /// The cell cursor stops at the edges of the table.
    #[test]
    fn the_cell_cursor_does_not_wrap() {
        assert_eq!(super::step_col(0, 1, 3), 1);
        assert_eq!(super::step_col(2, 1, 3), 2, "one press too many stays put");
        assert_eq!(super::step_col(0, -1, 3), 0);
        assert_eq!(super::step_col(1, -1, 3), 0);
        // A big jump is clamped, not wrapped.
        assert_eq!(super::step_col(0, 99, 3), 2);
        assert_eq!(super::step_col(2, -99, 3), 0);
        // No columns at all is an answer.
        assert_eq!(super::step_col(5, 1, 0), 0);
    }

    /// One tab per object, and a dashboard is not an object.
    #[test]
    fn an_object_already_open_is_found() {
        let mut a = table_tab();
        a.conn_name = "prod".into();
        a.object = Some(("users".into(), Some("public".into())));
        let mut b = table_tab();
        b.conn_name = "prod".into();
        b.object = Some(("orders".into(), None));
        let mut draft = table_tab();
        draft.conn_name = "prod".into();
        draft.object = None;
        let tabs = vec![a, b, draft];

        assert_eq!(super::find_tab_for(&tabs, "prod", "users", Some("public")), Some(0));
        assert_eq!(super::find_tab_for(&tabs, "prod", "orders", None), Some(1));
        // The SCHEMA is part of the identity: two tables of one name in two
        // schemas are two tables.
        assert_eq!(super::find_tab_for(&tabs, "prod", "users", None), None);
        assert_eq!(super::find_tab_for(&tabs, "prod", "users", Some("other")), None);
        // So is the connection: the same table on staging is not this one.
        assert_eq!(super::find_tab_for(&tabs, "staging", "users", Some("public")), None);
        // A typed draft is not an object and must not be reused.
        assert_eq!(super::find_tab_for(&tabs, "prod", "", None), None);
    }

    /// Ctrl-tab goes to the tab you were last in, and pressing it twice comes
    /// back — which is the whole difference between a most-recent list and
    /// stepping along the strip.
    #[test]
    fn ctrl_tab_returns_where_it_came_from() {
        let mut mru = Vec::new();
        for id in [1, 2, 3] {
            super::mru_touch(&mut mru, id);
        }
        assert_eq!(mru, vec![3, 2, 1]);
        // Touching one already in the list moves it, never duplicates it.
        super::mru_touch(&mut mru, 1);
        assert_eq!(mru, vec![1, 3, 2]);

        let alive = vec![1, 2, 3];
        let there = super::mru_next(&mru, 1, &alive).unwrap();
        assert_eq!(there, 3, "the one before this");
        super::mru_touch(&mut mru, there);
        assert_eq!(super::mru_next(&mru, there, &alive), Some(1), "and back again");
    }

    /// A closed tab is not somewhere to go, even while the list still names it.
    #[test]
    fn a_closed_tab_is_not_a_destination() {
        let mru = vec![9, 2, 1];
        let alive = vec![1, 2];
        assert_eq!(super::mru_next(&mru, 1, &alive), Some(2));
        // Nothing in the list is alive: fall back to any other open tab rather
        // than refusing to move.
        assert_eq!(super::mru_next(&[], 1, &alive), Some(2));
        // One tab only: there is nowhere else, and that is an answer.
        assert_eq!(super::mru_next(&[1], 1, &[1]), None);
        assert_eq!(super::mru_next(&[], 1, &[]), None);
    }

    /// A tab gets a name from its statement, on one line and not too long.
    #[test]
    fn a_draft_is_named_after_what_it_says() {
        assert_eq!(super::draft_title(""), "Draft");
        assert_eq!(super::draft_title("   \n  "), "Draft");
        assert_eq!(super::draft_title("select 1"), "select 1");
        // Several lines become one: a tab title with a newline in it makes the
        // strip as tall as the statement.
        assert_eq!(super::draft_title("select *\n  from t"), "select * from t");
        let long = "select a, b, c, d, e, f, g, h, i, j, k from a_long_table_name";
        let got = super::draft_title(long);
        assert!(got.chars().count() <= 41, "{got:?}");
        assert!(got.ends_with('…'), "{got:?}");
    }

    /// The runs cover the text exactly, and name the four things apart.
    #[test]
    fn json_colours_cover_every_byte() {
        use super::JsonRole::*;
        let text = "{\n  \"name\": \"ada\",\n  \"n\": -12.5e3,\n  \"ok\": true,\n  \"x\": null\n}";
        let runs = super::json_runs(text);
        assert_eq!(
            runs.iter().map(|(l, _)| *l).sum::<usize>(),
            text.len(),
            "the runs must cover the text: {runs:?}"
        );
        // Every boundary is a char boundary, which is what the text system
        // asserts on.
        let mut at = 0usize;
        for (l, _) in &runs {
            at += l;
            assert!(text.is_char_boundary(at), "{at} is not a boundary");
        }
        let roles: Vec<super::JsonRole> = runs.iter().map(|(_, r)| *r).collect();
        assert!(roles.contains(&Key), "{roles:?}");
        assert!(roles.contains(&Str));
        assert!(roles.contains(&Num));
        assert!(roles.contains(&Word));
        assert!(roles.contains(&Punct));
    }

    /// A string holding an escaped quote, a colon or a brace does not confuse
    /// the scanner — and a key is a string with a colon AFTER it, not one with
    /// a colon in it.
    #[test]
    fn a_quoted_colon_is_not_a_key() {
        use super::JsonRole::*;
        let text = r#"{"a:b": "say \"hi\": now"}"#;
        let runs = super::json_runs(text);
        assert_eq!(runs.iter().map(|(l, _)| *l).sum::<usize>(), text.len(), "{runs:?}");
        let keys = runs.iter().filter(|(_, r)| *r == Key).count();
        assert_eq!(keys, 1, "only `\"a:b\"` is a key: {runs:?}");
        assert_eq!(runs.iter().filter(|(_, r)| *r == Str).count(), 1);
    }

    /// Text that is not JSON at all still comes back whole, because an engine
    /// can answer with anything and this must not drop a byte of it.
    #[test]
    fn rubbish_is_still_covered() {
        for text in ["", "{", "\"unterminated", "привет", "{\"k\": \"стойност\"}", "-", "12e"] {
            let runs = super::json_runs(text);
            assert_eq!(
                runs.iter().map(|(l, _)| *l).sum::<usize>(),
                text.len(),
                "{text:?} -> {runs:?}"
            );
            let mut at = 0usize;
            for (l, _) in &runs {
                at += l;
                assert!(text.is_char_boundary(at), "{text:?} broke at {at}");
            }
        }
    }

    /// The chosen palette row is always on screen.    /// The chosen palette row is always on screen.
    #[test]
    fn the_palette_shows_the_row_it_has_chosen() {
        // Everything fits: the whole list, from the top.
        assert_eq!(super::palette_window(0, 5, 14), 0..5);
        assert_eq!(super::palette_window(4, 5, 14), 0..5);
        // More rows than room: the window slides.
        for sel in 0..40 {
            let w = super::palette_window(sel, 40, 14);
            assert!(w.contains(&sel), "sel {sel} fell outside {w:?}");
            assert_eq!(w.len(), 14, "sel {sel} gave {w:?}");
            assert!(w.end <= 40);
        }
        // The last row puts the window at the bottom, not past it.
        assert_eq!(super::palette_window(39, 40, 14), 26..40);
        // Degenerate cases answer rather than panic.
        assert_eq!(super::palette_window(0, 0, 14), 0..0);
        assert_eq!(super::palette_window(3, 10, 0), 0..0);
    }

    /// A tab is marked for what needs saying and nothing else.    /// A tab is marked for what needs saying and nothing else.
    #[test]
    fn a_tab_is_marked_only_when_there_is_something_to_say() {
        assert!(super::tab_mark(super::RunState::Running).is_some());
        assert!(super::tab_mark(super::RunState::Failed).is_some());
        assert!(super::tab_mark(super::RunState::Cancelled).is_some());
        assert_eq!(super::tab_mark(super::RunState::Idle), None);
        assert_eq!(super::tab_mark(super::RunState::Done), None, "a finished tab is the normal one");
        // The three marks differ, or they would say the same thing.
        let marks = [
            super::tab_mark(super::RunState::Running).unwrap(),
            super::tab_mark(super::RunState::Failed).unwrap(),
            super::tab_mark(super::RunState::Cancelled).unwrap(),
        ];
        assert_ne!(marks[0], marks[1]);
        assert_ne!(marks[1], marks[2]);
        assert_ne!(marks[0], marks[2]);
    }

    /// Numbers line up on the right, and nothing else does.    /// Numbers line up on the right, and nothing else does.
    #[test]
    fn a_column_of_numbers_lines_up_by_place_value() {
        use super::Align;
        assert_eq!(super::cell_align("int"), Align::Right);
        assert_eq!(super::cell_align("num"), Align::Right);
        for other in ["string", "bool", "date", "oid", "object", "array", "null", "mixed"] {
            assert_eq!(super::cell_align(other), Align::Left, "{other}");
        }
    }

    /// A heading is two lines only when it has something to say on the second.
    #[test]
    fn a_heading_earns_its_second_line() {
        use super::ColumnSummary;
        // Nothing known and nothing missing: one line.
        assert_eq!(
            super::header_rows(&ColumnSummary { kind: "null", present: 10, total: 10 }),
            1
        );
        // A type to name.
        assert_eq!(
            super::header_rows(&ColumnSummary { kind: "int", present: 10, total: 10 }),
            2
        );
        // Or gaps to report, even with no type.
        assert_eq!(
            super::header_rows(&ColumnSummary { kind: "null", present: 3, total: 10 }),
            2
        );
    }

    /// A mistyped port must not quietly become 22.    /// A mistyped port must not quietly become 22.
    #[test]
    fn a_port_that_is_not_a_number_is_refused() {
        assert_eq!(super::parse_port("2222", 22), Ok(2222));
        // Untouched means the default; that much IS an answer.
        assert_eq!(super::parse_port("", 22), Ok(22));
        assert_eq!(super::parse_port("  ", 1080), Ok(1080));
        // And everything else is a mistake worth saying out loud.
        assert!(super::parse_port("22o", 22).is_err());
        assert!(super::parse_port("-1", 22).is_err());
        assert!(super::parse_port("70000", 22).is_err());
        assert!(super::parse_port("0", 22).is_err());
        assert!(super::parse_port("22 22", 22).is_err());
    }

    /// A task whose tab is gone still has to be told its call is over, and a
    /// cancelled call says so rather than just stopping.
    #[test]
    fn a_task_is_told_how_its_call_ended() {
        use super::Settle;
        assert_eq!(super::task_settled("done", None), Settle::Finished(None));
        assert_eq!(
            super::task_settled("failed", Some("no such table")),
            Settle::Finished(Some("no such table".into()))
        );
        assert_eq!(
            super::task_settled("failed", None),
            Settle::Finished(Some("query failed".into())),
            "a failure with no words is still a failure"
        );
        assert_eq!(
            super::task_settled("failed", Some("   ")),
            Settle::Finished(Some("query failed".into()))
        );
        assert_eq!(
            super::task_settled("cancelled", None),
            Settle::Finished(Some("cancelled".into()))
        );
        assert_eq!(super::task_settled("running", None), Settle::Running);
    }

    /// The whole point of settling an orphan: a running task keeps the window
    /// repainting, so one that is never finished never lets it rest.
    #[test]
    fn a_settled_task_stops_running() {
        let mut t = super::Task::new(1, "x".into());
        assert!(t.running());
        assert_eq!(t.call_id, None);
        t.finish(std::time::Duration::from_millis(3), Some("cancelled".into()));
        assert!(!t.running());
        assert!(t.failed());
        assert!(t.open, "a failure is shown without being asked for");
    }

    /// The badge counts what somebody must look at, and nothing else.
    #[test]
    fn only_a_warning_lights_the_badge() {
        assert!(!super::counts_as_unread("info"));
        assert!(super::counts_as_unread("warn"));
        assert!(super::counts_as_unread("error"));
        assert!(super::counts_as_unread("debug"), "an unknown level is shown, not swallowed");
    }

    /// The padlock tells the truth in three cases, and a tunnel flag never
    /// talks the engine out of saying the link is in the clear.
    #[test]
    fn the_padlock_says_what_the_engine_said() {
        let tls = super::link_label(true, false);
        assert_eq!(tls.glyph, super::LINK_LOCKED);
        assert_eq!(tls.tip, "tip.link_tls");
        assert!(tls.safe);

        let tunnel = super::link_label(true, true);
        assert_eq!(tunnel.glyph, super::LINK_LOCKED);
        assert_eq!(tunnel.tip, "tip.link_tunnel", "a tunnel is worth naming as one");
        assert!(tunnel.safe);

        let plain = super::link_label(false, false);
        assert_eq!(plain.glyph, super::LINK_OPEN);
        assert_eq!(plain.tip, "tip.link_plain");
        assert!(!plain.safe);

        let lying = super::link_label(false, true);
        assert_eq!(lying.glyph, super::LINK_OPEN, "not encrypted is not encrypted");
        assert!(!lying.safe);
    }

    /// An engine that refuses at execute still says why, and that sentence is
    /// what the tab shows above its buttons.
    #[test]
    fn a_refusal_carries_the_reason_back() {
        assert_eq!(
            super::needs_confirm_from_error("needs confirming: this deletes every row"),
            Some("this deletes every row".to_string())
        );
        // Anything else is an ordinary error and stays one.
        assert_eq!(super::needs_confirm_from_error("syntax error at or near \"DROP\""), None);
        assert_eq!(super::needs_confirm_from_error("needs confirming:"), None);
    }

    /// The console follows the eye.
    #[test]
    fn the_target_is_the_tab_in_front_of_you() {
        let open = vec!["prod".to_string(), "staging".to_string()];
        assert_eq!(pick_target(Some("staging"), &open), Some("staging".into()));
        // No tab at all: the one connection there is — a console opened before
        // any tab means that one.
        assert_eq!(pick_target(None, &open), Some("prod".into()));
        // A tab whose connection dropped: nothing, rather than another database.
        assert_eq!(pick_target(Some("gone"), &open), None);
        assert_eq!(pick_target(None, &[]), None);
    }

    fn an_audit_row() -> lode_proto::AuditRow {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "path": "conn.connect",
            "statement": "mongodb 127.0.0.1:27017",
            "outcome": "connected",
            "conn": "neterra-mongo",
        }))
        .expect("an audit row parses")
    }

    /// A record drawn on two lines gets a box for two lines.
    ///
    /// The audit panel drew two lines in a box sized for one: records overlapped,
    /// descenders were cut, and a hovered second line lay across its neighbour.
    /// The history panel, which draws the same two lines as two plain children,
    /// was fine — so the fault was in this row and not in the scale.
    #[test]
    fn an_audit_record_is_as_tall_as_the_lines_it_has() {
        let a = an_audit_row();

        // Closed: the statement and the detail line. Two.
        let parts = audit_parts(&a, false);
        assert_eq!(parts.len(), 2, "{parts:#?}");
        assert_eq!(parts[0].outcome.as_deref(), Some("connected"));
        assert!(parts[0].text.contains("127.0.0.1"));
        assert_eq!(parts[1].outcome, None, "the outcome column is on the first line only");
        assert!(parts[1].text.contains("neterra-mongo") && parts[1].text.contains("conn.connect"));
        assert!(parts.iter().all(|p| !p.wraps), "a closed record is cut, not wrapped");

        // And the box is for TWO, not one.
        let (line_h, pad) = (20.0_f32, 2.0_f32);
        assert_eq!(audit_row_h(&parts, line_h, pad), 2.0 * line_h + 2.0 * pad);
        assert!(
            audit_row_h(&parts, line_h, pad) > line_h + 2.0 * pad,
            "one line of room for two lines of text is the bug"
        );

        // A record with no statement still has both lines.
        let mut empty = an_audit_row();
        empty.statement = String::new();
        let parts = audit_parts(&empty, false);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].text.contains("not kept"));

        // Opened: the reason and the shape join in, and the reason WRAPS — a
        // refusal shortened to fit has stopped explaining itself.
        let mut open_row = an_audit_row();
        open_row.reason = Some("read only: refuses a harmless INSERT".into());
        open_row.fingerprint = Some("a1b2c3".into());
        let parts = audit_parts(&open_row, true);
        assert!(parts.len() > 2, "{parts:#?}");
        assert!(parts.iter().any(|p| p.wraps), "the reason must be free to wrap");
        // A wrapping part is NOT counted into the fixed height, because its own
        // height is whatever the text needs.
        let fixed = parts.iter().filter(|p| !p.wraps).count();
        assert_eq!(audit_row_h(&parts, line_h, pad), fixed as f32 * line_h + 2.0 * pad);
    }

    /// A tooltip appears where text is CUT and nowhere else.
    ///
    /// A tip on every row is noise, and noise is worse than a gap — so the
    /// comparison leans toward "it fits" and a borderline line gets no tip
    /// rather than a wrong one.
    #[test]
    fn only_a_cut_line_offers_the_rest_of_itself() {
        // The arithmetic the window uses, with a monospaced ratio.
        let cut = |text: &str, size: f32, w: f32| -> bool {
            text.chars().count() as f32 * 0.6 * size > w * 1.02
        };

        // Comfortably inside: no tip.
        assert!(!cut("short", 13.0, 400.0));
        // Comfortably over: a tip.
        assert!(cut(&"x".repeat(200), 13.0, 400.0));

        // The two the user actually saw cut, at the width that cut them.
        assert!(cut("LvimBase_light", 12.0, 90.0), "the theme name was cut");
        assert!(cut("14 / 28 px", 12.0, 40.0), "the sizes were cut");

        // Exactly at the edge counts as fitting: the margin exists so that a
        // line which just fits is never given a tip it does not need.
        let w = 10.0 * 0.6 * 13.0;
        assert!(!cut(&"x".repeat(10), 13.0, w));

        // A character count, not a byte count — otherwise every Cyrillic row
        // would claim to be cut.
        let bg = "Размер на иконите";
        assert_eq!(bg.chars().count(), 17);
        assert!(bg.len() > 17, "it is longer in bytes, which is the trap");
        assert!(!cut(bg, 12.0, 200.0), "17 characters fit in 200px");
    }

    /// The three cases that mean anything, and the one that was wrong.
    #[test]
    fn the_scrollbar_describes_what_there_is_to_scroll() {
        // 1. Content NARROWER than the window: no scrolling, and so no bar.
        //    A bar that cannot move is a control that lies.
        assert_eq!(hbar(800.0, 1200.0, 0.0), None);
        assert_eq!(hbar(1200.0, 1200.0, 0.0), None);

        // 2. Slightly wider: the thumb is nearly full, and it still scrolls.
        //    This is the case that was indistinguishable from the bug — the
        //    difference is that `max` is small but REAL.
        let b = hbar(1300.0, 1200.0, 0.0).expect("there is something to scroll");
        assert!(b.frac > 0.9 && b.frac < 1.0, "frac {}", b.frac);
        assert_eq!(b.max, 100.0);

        // 3. Twice as wide: the thumb is half the track.
        let b = hbar(2400.0, 1200.0, 0.0).expect("scrolls");
        assert!((b.frac - 0.5).abs() < 0.001, "frac {}", b.frac);
        assert_eq!(b.max, 1200.0);
        assert_eq!(b.at, 0.0);

        // Scrolled to the far end, the thumb sits at the far end.
        let b = hbar(2400.0, 1200.0, -1200.0).expect("scrolls");
        assert!((b.at + b.frac - 1.0).abs() < 0.001, "at {} frac {}", b.at, b.frac);

        // THE BUG: believing the window is wider than it is gives a thumb that
        // is nearly full AND a `max` too small to reach the last columns. Both
        // faces of one wrong number.
        let truth = hbar(2400.0, 1200.0, 0.0).unwrap();
        let lie = hbar(2400.0, 2200.0, 0.0).unwrap();
        assert!(lie.frac > truth.frac, "the thumb grows as the lie grows");
        assert!(lie.max < truth.max, "and the reach shrinks at the same time");

        // A thumb never vanishes: a huge result still leaves something to grab.
        let b = hbar(100_000.0, 500.0, 0.0).expect("scrolls");
        assert!(b.frac >= 0.05, "frac {}", b.frac);

        // Nonsense in, nothing out — rather than a division by zero.
        assert_eq!(hbar(0.0, 100.0, 0.0), None);
        assert_eq!(hbar(100.0, 0.0, 0.0), None);
    }

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

    /// A document reads as a thing, not as a count of fields.
    ///
    /// Thirty rows of `{6}`, `{5}`, `{7}` cannot be told apart without opening
    /// every one, which is the opposite of what a list is for.
    #[test]
    fn a_document_preview_says_which_document_it_is() {
        let doc = json!({
            "country": "BG",
            "lang": "bg",
            "year_start": {"__ext": "$date", "v": "2026-09-15T08:00:00Z"},
        });
        let p = doc_preview(&doc, 120);
        assert!(p.contains("country"), "{p}");
        assert!(p.contains("BG"), "{p}");
        // A tagged value is NAMED in the preview, not counted: `{"$date": …}`
        // has one key and `{1}` says nothing.
        assert!(p.contains("2026-09-15"), "a date must read as a date: {p}");
        assert!(!p.contains("{1}"), "{p}");

        // Two documents that differ are distinguishable, which is the point.
        let other = json!({"country": "DE", "lang": "de"});
        assert_ne!(doc_preview(&doc, 120), doc_preview(&other, 120));

        // It fills its budget and stops, rather than running over.
        let big = json!({
            "a": "x".repeat(200), "b": "y".repeat(200), "c": "z".repeat(200),
        });
        let p = doc_preview(&big, 60);
        assert!(p.chars().count() <= 61, "{} chars", p.chars().count());
        assert!(p.ends_with('\u{2026}'), "a cut preview says it was cut: {p}");

        // An empty document is empty, not "{0}".
        assert_eq!(doc_preview(&json!({}), 120), "{}");

        // An ObjectId reads as its hex, which is how somebody recognises a row.
        let with_id = json!({"_id": {"__oid": "6a91cd1cea4393beaf983"}});
        assert!(doc_preview(&with_id, 120).contains("6a91cd1cea4393beaf983"));
    }

    /// A tagged value is named, not called "object".
    ///
    /// The engine sends an ObjectId as `{"__oid": …}` and a date as
    /// `{"$date": …}` so a client can name them. Calling either one "object"
    /// throws away exactly what the tagging was for, and puts `{"$date": …}`
    /// on screen where a date belongs.
    #[test]
    fn a_tagged_value_is_named_by_what_it_is() {
        // THE ENGINE'S OWN SHAPES, read off what it actually sends. The first
        // version of this looked for `{"$date": …}` — taken from another
        // client's screenshot — and missed every date this engine produces.
        assert_eq!(type_badge(&json!({"__oid": "6a91cd1cea4393beaf983"}), None), "oid");
        assert_eq!(
            type_badge(&json!({"__ext": "$date", "v": "2026-09-15T08:00:00Z"}), None),
            "date"
        );
        assert_eq!(type_badge(&json!({"__ext": "$numberLong", "v": "12"}), None), "int");
        assert_eq!(type_badge(&json!({"__ext": "$binary", "v": "AA=="}), None), "bin");
        // A tag this client does not know is still a tagged thing, not an
        // object: saying `ext` is a gap, saying `object` would be wrong.
        assert_eq!(type_badge(&json!({"__ext": "$whatever", "v": "x"}), None), "ext");
        // And the plain extended-JSON forms, in case another engine sends those.
        assert_eq!(type_badge(&json!({"$date": "2026-09-15T08:00:00Z"}), None), "date");
        assert_eq!(type_badge(&json!({"$numberLong": "12"}), None), "int");
        assert_eq!(type_badge(&json!({"$binary": {}}), None), "bin");
        // The VALUE comes out of the tag, which is what tagging is for.
        assert_eq!(
            preview(&json!({"__ext": "$date", "v": "2026-09-15T08:00:00Z"})),
            "2026-09-15T08:00:00Z"
        );
        assert_eq!(preview(&json!({"__oid": "6a91cd1c"})), "6a91cd1c");
        // An ordinary document is still an object.
        assert_eq!(type_badge(&json!({"a": 1}), None), "object");
    }

    /// A column's type and how much of the page actually has it.
    ///
    /// For a document store this is the most useful thing in the whole header:
    /// which fields are everywhere and which are not. It is computed from the
    /// PAGE, so the count it is out of travels with it — a bare "92%" invites
    /// somebody to read it as a fact about the collection.
    #[test]
    fn a_column_says_what_it_holds_and_in_how_many_rows() {
        let rows: Vec<Vec<Value>> = vec![
            vec![json!({"__oid": "a"}), json!("bg"), json!(null)],
            vec![json!({"__oid": "b"}), json!("de"), json!(7)],
            vec![json!({"__oid": "c"}), json!(null), json!(9)],
        ];

        let id = column_summary(&rows, 0, None);
        assert_eq!(id.kind, "oid");
        assert_eq!((id.present, id.total), (3, 3));
        assert_eq!(id.percent(), 100);

        // A null is NOT the field being present: for a document store the
        // difference between "missing" and "there and empty" is the point.
        let country = column_summary(&rows, 1, None);
        assert_eq!(country.kind, "str");
        assert_eq!((country.present, country.total), (2, 3));
        assert_eq!(country.percent(), 67);

        let note = column_summary(&rows, 2, None);
        assert_eq!(note.kind, "int");
        assert_eq!((note.present, note.total), (2, 3));

        // More than one type is "mixed", not whichever came first — that is the
        // answer somebody needs to see, since it is the surprising one.
        let messy: Vec<Vec<Value>> = vec![vec![json!(1)], vec![json!("x")]];
        assert_eq!(column_summary(&messy, 0, None).kind, "mixed");

        // What the driver declares beats a guess from values.
        let declared: Vec<Vec<Value>> = vec![vec![json!("2026-01-01")]];
        assert_eq!(column_summary(&declared, 0, Some("timestamp")).kind, "date");

        // Nothing at all does not divide by zero, and says nothing rather than
        // inventing a type.
        let empty: Vec<Vec<Value>> = vec![];
        let s = column_summary(&empty, 0, None);
        assert_eq!((s.kind, s.present, s.total, s.percent()), ("null", 0, 0, 0));
    }

    #[test]
    fn type_badges_prefer_what_the_driver_said() {
        assert_eq!(type_badge(&json!("x"), Some("ObjectId")), "oid");
        assert_eq!(type_badge(&json!("x"), Some("timestamp")), "date");
        assert_eq!(type_badge(&json!(1), Some("int8")), "field", "an unmapped declared type is still a field");
        // Nothing declared, or the catch-all document type: fall back to JSON.
        // "str", not "string": these words sit in a column header and beside
        // every field in the schema tree, where a long word costs room that the
        // name needs. One vocabulary, used in all three places.
        assert_eq!(type_badge(&json!("x"), None), "str");
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
