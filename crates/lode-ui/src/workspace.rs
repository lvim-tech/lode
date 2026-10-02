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

use lode_proto::{Column, DriverMeta, Node, SavedQuery};
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
        matches!(self, SubTab::Documents)
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

    #[test]
    fn only_documents_is_implemented_but_the_row_holds_the_rest() {
        assert_eq!(SubTab::all().len(), 4);
        assert!(SubTab::Documents.implemented());
        for s in [SubTab::Schema, SubTab::Aggregate, SubTab::Builder] {
            assert!(!s.implemented(), "{} is laid out, not built", s.label());
        }
    }
}
