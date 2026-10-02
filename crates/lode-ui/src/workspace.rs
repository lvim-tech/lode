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

use lode_proto::{Column, DriverMeta, Index, Node, SavedQuery, TableColumn};
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
}

impl ResultView {
    pub fn label(self) -> &'static str {
        match self {
            ResultView::Tree => "Tree",
            ResultView::Table => "Table",
            ResultView::Json => "JSON",
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
        matches!(self, SubTab::Documents | SubTab::Schema)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum RunState {
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
    /// A write waiting to be confirmed: the statement the ENGINE said it would
    /// run, why it is dangerous, and the plan to repeat with `confirm`.
    pub pending_write: Option<PendingWrite>,
    /// The last statement this tab's browse fields PRODUCED. Kept so a hand
    /// edit can be told from an untouched one: if the box still holds this, the
    /// fields may rewrite it; if it does not, somebody typed in there and their
    /// words win.
    pub rendered: String,
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
            pending_write: None,
            rendered: String::new(),
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
        for s in [SubTab::Documents, SubTab::Schema] {
            assert!(s.implemented(), "{} is built", s.label());
        }
        for s in [SubTab::Aggregate, SubTab::Builder] {
            assert!(!s.implemented(), "{} is laid out, not built", s.label());
        }
    }
}
