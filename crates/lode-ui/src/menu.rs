//! The context menu: what a right click offers, and where.
//!
//! WHY IT MATTERS MORE THAN IT LOOKS. Half of what this client can do has no
//! visible control at all. A connection can be renamed only by opening its
//! window; a node's DDL can be copied only by opening a tab and switching
//! views; a row can be copied only as part of an export. Every one of them is
//! one line of code away from being reachable, and the thing they are missing
//! is a place to put it.
//!
//! WHAT IS IN THIS FILE: the decision of WHICH items a target offers, as a pure
//! function with a test, and the state of an open menu. What it looks like is
//! drawn in `app.rs` beside the palette, because it is the same shape — an
//! overlay, drawn last, closed by escape — and two of those would drift.

use serde_json::Value;

/// What was right-clicked.
///
/// The target carries what the ITEMS need, so deciding the list is a pure
/// function of it rather than a reach back into the workspace: `menu_for` can
/// then be tested, which is the only way a list of eleven items per target
/// stays correct.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A saved connection, whether it is open, and whether the engine is new
    /// enough to file it in a folder.
    Connection { name: String, live: bool, folders: bool },
    /// A node of the schema tree.
    Node { conn: String, name: String, schema: Option<String>, kind: String, has_ddl: bool },
    /// A row of the grid: which tab, which row, and whether this result can be
    /// written to at all.
    Row { tab: u64, row: usize, editable: bool },
    /// A tab of the strip.
    Tab { tab: u64, others: bool },
    /// Something saved under a name, on a connection.
    Saved { conn: String, name: String, kind: SavedKind },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedKind {
    Query,
    Chart,
    Board,
}

/// What one item does. Carried out by the workspace — this file decides nothing
/// about how.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    Connect(String),
    Disconnect(String),
    EditConnection(String),
    RemoveConnection(String),
    RefreshConnection(String),
    /// Ask which folder to file it in. An empty answer takes it out of one.
    MoveToFolder(String),
    /// Put a name on the clipboard. The commonest thing anybody wants from a
    /// tree and the one thing it could not do.
    CopyText(String),
    OpenNode { conn: String, name: String, schema: Option<String>, kind: String },
    /// The node's fields and indexes, open or shut.
    InsideNode { conn: String, name: String, schema: Option<String>, kind: String },
    CopyDdl { conn: String, name: String, schema: Option<String> },
    InsertIntoEditor(String),
    CopyRowTsv { tab: u64, row: usize },
    CopyRowSelect { tab: u64, row: usize },
    CopyRowInsert { tab: u64, row: usize },
    ShowRow { tab: u64, row: usize },
    EditRow { tab: u64, row: usize },
    DeleteRow { tab: u64, row: usize },
    CloseTab(u64),
    CloseOtherTabs(u64),
    DuplicateTab(u64),
    OpenSaved { conn: String, name: String, kind: SavedKind },
    RemoveSaved { conn: String, name: String, kind: SavedKind },
}

/// One line of the menu.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// The catalogue key, not the words — the menu is chrome like everything
    /// else.
    pub key: &'static str,
    pub act: Act,
    /// False when the item belongs on this target but cannot be used now. SHOWN
    /// and dimmed rather than hidden: a menu whose length changes with the
    /// state is a menu nobody learns the shape of, and "why is Delete row not
    /// there" has no answer while "why is it grey" has one.
    pub enabled: bool,
}

impl Item {
    fn on(key: &'static str, act: Act) -> Self {
        Item { key, act, enabled: true }
    }
    fn off(key: &'static str, act: Act) -> Self {
        Item { key, act, enabled: false }
    }
}

/// The menu for a target.
pub fn menu_for(target: &Target) -> Vec<Item> {
    match target {
        Target::Connection { name, live, folders } => {
            let n = name.clone();
            vec![
                if *live {
                    Item::on("menu.disconnect", Act::Disconnect(n.clone()))
                } else {
                    Item::on("menu.connect", Act::Connect(n.clone()))
                },
                if *live {
                    Item::on("menu.refresh", Act::RefreshConnection(n.clone()))
                } else {
                    Item::off("menu.refresh", Act::RefreshConnection(n.clone()))
                },
                Item::on("menu.edit", Act::EditConnection(n.clone())),
                // The FOLDER lives in the engine's store, so an engine that
                // does not have the field cannot be asked — dimmed rather than
                // hidden, so the menu is the same shape against either.
                if *folders {
                    Item::on("menu.move_to", Act::MoveToFolder(n.clone()))
                } else {
                    Item::off("menu.move_to", Act::MoveToFolder(n.clone()))
                },
                Item::on("menu.copy_name", Act::CopyText(n.clone())),
                Item::on("menu.remove", Act::RemoveConnection(n)),
            ]
        }
        Target::Node { conn, name, schema, kind, has_ddl } => {
            let open = Act::OpenNode {
                conn: conn.clone(),
                name: name.clone(),
                schema: schema.clone(),
                kind: kind.clone(),
            };
            let inside = Act::InsideNode {
                conn: conn.clone(),
                name: name.clone(),
                schema: schema.clone(),
                kind: kind.clone(),
            };
            let ddl = Act::CopyDdl { conn: conn.clone(), name: name.clone(), schema: schema.clone() };
            vec![
                Item::on("menu.open", open),
                Item::on("menu.inside", inside),
                Item::on("menu.insert_name", Act::InsertIntoEditor(name.clone())),
                Item::on("menu.copy_name", Act::CopyText(name.clone())),
                // The engine answers DDL only for the drivers that have it, and
                // the node says which it is.
                if *has_ddl {
                    Item::on("menu.copy_ddl", ddl)
                } else {
                    Item::off("menu.copy_ddl", ddl)
                },
            ]
        }
        Target::Row { tab, row, editable } => {
            let (tab, row) = (*tab, *row);
            vec![
                Item::on("menu.show_row", Act::ShowRow { tab, row }),
                Item::on("menu.copy_row", Act::CopyRowTsv { tab, row }),
                Item::on("menu.copy_select", Act::CopyRowSelect { tab, row }),
                Item::on("menu.copy_insert", Act::CopyRowInsert { tab, row }),
                // A result that is not a table cannot be written to, and the
                // engine is the one that says so — see `caps.edit`.
                if *editable {
                    Item::on("menu.edit_row", Act::EditRow { tab, row })
                } else {
                    Item::off("menu.edit_row", Act::EditRow { tab, row })
                },
                if *editable {
                    Item::on("menu.delete_row", Act::DeleteRow { tab, row })
                } else {
                    Item::off("menu.delete_row", Act::DeleteRow { tab, row })
                },
            ]
        }
        Target::Tab { tab, others } => {
            let tab = *tab;
            vec![
                Item::on("menu.close_tab", Act::CloseTab(tab)),
                if *others {
                    Item::on("menu.close_others", Act::CloseOtherTabs(tab))
                } else {
                    Item::off("menu.close_others", Act::CloseOtherTabs(tab))
                },
                Item::on("menu.duplicate_tab", Act::DuplicateTab(tab)),
            ]
        }
        Target::Saved { conn, name, kind } => vec![
            Item::on(
                "menu.open",
                Act::OpenSaved { conn: conn.clone(), name: name.clone(), kind: *kind },
            ),
            Item::on("menu.copy_name", Act::CopyText(name.clone())),
            Item::on(
                "menu.remove",
                Act::RemoveSaved { conn: conn.clone(), name: name.clone(), kind: *kind },
            ),
        ],
    }
}

/// An open menu: what it is about, where it was opened, and which line is
/// under the keyboard.
pub struct Open {
    pub target: Target,
    pub at: (f32, f32),
    pub sel: usize,
}

impl Open {
    pub fn new(target: Target, at: (f32, f32)) -> Self {
        Open { target, at, sel: 0 }
    }

    pub fn items(&self) -> Vec<Item> {
        menu_for(&self.target)
    }

    /// Move the highlight, skipping what cannot be used — stopping on a dimmed
    /// line would look like a menu that has stopped responding.
    pub fn step(&mut self, by: isize) {
        let items = self.items();
        let n = items.len();
        if n == 0 {
            return;
        }
        let mut at = self.sel as isize;
        for _ in 0..n {
            at = (at + by).rem_euclid(n as isize);
            if items[at as usize].enabled {
                self.sel = at as usize;
                return;
            }
        }
    }

    /// What pressing Enter would do, if anything.
    pub fn chosen(&self) -> Option<Act> {
        self.items().get(self.sel).filter(|i| i.enabled).map(|i| i.act.clone())
    }
}

/// One row of a result as tab-separated values.
///
/// TSV because that is what a spreadsheet and a terminal both understand, and
/// because a row copied as CSV has to decide about quoting — a decision that is
/// wrong for one of the two.
pub fn row_tsv(columns: &[String], row: &[Value]) -> String {
    let _ = columns;
    row.iter().map(cell_text).collect::<Vec<_>>().join("\t")
}

/// The same, with the column names above it.
pub fn page_tsv(columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = columns.join("\t");
    for r in rows {
        out.push('\n');
        out.push_str(&row_tsv(columns, r));
    }
    out
}

/// One cell as text, for the clipboard.
///
/// A tab or a newline INSIDE a value would break the format it is being put
/// into, so they become spaces. Lossy, and deliberately: a paste that moves a
/// column over by one is worse than a paste that loses a line break.
fn cell_text(v: &Value) -> String {
    let raw = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    raw.replace(['\t', '\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_offers_what_it_can_do_now() {
        let shut =
            menu_for(&Target::Connection { name: "prod".into(), live: false, folders: true });
        let keys: Vec<&str> = shut.iter().map(|i| i.key).collect();
        assert_eq!(
            keys,
            vec![
                "menu.connect",
                "menu.refresh",
                "menu.edit",
                "menu.move_to",
                "menu.copy_name",
                "menu.remove"
            ]
        );
        // Refresh means nothing on a connection that is not open — SHOWN and
        // dimmed, so the menu is the same shape either way.
        assert!(!shut[1].enabled);

        let open = menu_for(&Target::Connection { name: "prod".into(), live: true, folders: true });
        assert_eq!(open[0].key, "menu.disconnect");
        assert!(open[1].enabled);
        assert_eq!(open.len(), shut.len(), "the shape does not change with the state");

        // An engine too old for folders dims that line and keeps the shape.
        let old =
            menu_for(&Target::Connection { name: "prod".into(), live: true, folders: false });
        assert_eq!(old.len(), open.len());
        assert!(!old.iter().find(|i| i.key == "menu.move_to").unwrap().enabled);
    }

    #[test]
    fn a_node_offers_its_ddl_only_where_the_engine_has_one() {
        let with = menu_for(&Target::Node {
            conn: "c".into(),
            name: "users".into(),
            schema: Some("public".into()),
            kind: "table".into(),
            has_ddl: true,
        });
        assert!(with.iter().find(|i| i.key == "menu.copy_ddl").unwrap().enabled);
        let without = menu_for(&Target::Node {
            conn: "c".into(),
            name: "users".into(),
            schema: None,
            kind: "collection".into(),
            has_ddl: false,
        });
        assert!(!without.iter().find(|i| i.key == "menu.copy_ddl").unwrap().enabled);
        assert_eq!(with.len(), without.len());
    }

    #[test]
    fn a_row_offers_writing_only_where_it_is_allowed() {
        let ro = menu_for(&Target::Row { tab: 1, row: 4, editable: false });
        for key in ["menu.show_row", "menu.copy_row", "menu.copy_select", "menu.copy_insert"] {
            assert!(ro.iter().find(|i| i.key == key).unwrap().enabled, "{key}");
        }
        for key in ["menu.edit_row", "menu.delete_row"] {
            assert!(!ro.iter().find(|i| i.key == key).unwrap().enabled, "{key}");
        }
        let rw = menu_for(&Target::Row { tab: 1, row: 4, editable: true });
        assert!(rw.iter().all(|i| i.enabled));
    }

    #[test]
    fn close_others_needs_others() {
        let alone = menu_for(&Target::Tab { tab: 7, others: false });
        assert!(!alone.iter().find(|i| i.key == "menu.close_others").unwrap().enabled);
        let crowd = menu_for(&Target::Tab { tab: 7, others: true });
        assert!(crowd.iter().find(|i| i.key == "menu.close_others").unwrap().enabled);
    }

    /// The highlight never rests on a line that cannot be used, and it comes
    /// back round rather than stopping at the end.
    #[test]
    fn the_highlight_skips_what_is_dimmed() {
        let mut open = Open::new(Target::Row { tab: 1, row: 0, editable: false }, (0.0, 0.0));
        assert_eq!(open.sel, 0);
        for _ in 0..4 {
            open.step(1);
        }
        // Items 4 and 5 are dimmed, so four steps from 0 wrap back to 0.
        assert_eq!(open.sel, 0, "wrapped past the two dimmed lines");
        assert!(open.chosen().is_some());

        // Backwards lands on the last USABLE line, which is 3.
        let mut open = Open::new(Target::Row { tab: 1, row: 0, editable: false }, (0.0, 0.0));
        open.step(-1);
        assert_eq!(open.sel, 3);

        // A menu whose every line is usable still moves one at a time.
        let mut open = Open::new(Target::Tab { tab: 1, others: true }, (0.0, 0.0));
        open.step(1);
        assert_eq!(open.sel, 1);
        open.step(1);
        assert_eq!(open.sel, 2);
        open.step(1);
        assert_eq!(open.sel, 0);
    }

    /// A tab or a newline inside a value must not move a column over by one.
    #[test]
    fn a_copied_row_keeps_its_shape() {
        use serde_json::json;
        let cols = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let row = vec![json!("one\ttwo"), json!("line\nbreak"), Value::Null];
        let tsv = row_tsv(&cols, &row);
        assert_eq!(tsv.matches('\t').count(), 2, "exactly two separators: {tsv:?}");
        assert!(!tsv.contains('\n'));
        assert_eq!(tsv, "one two\tline break\t");

        let page = page_tsv(&cols, &[row]);
        assert_eq!(page.lines().count(), 2, "a heading and a row");
        assert!(page.starts_with("a\tb\tc\n"));

        // Numbers and booleans come out as themselves, not as JSON strings.
        let row = vec![json!(12), json!(true), json!(1.5)];
        assert_eq!(row_tsv(&cols, &row), "12\ttrue\t1.5");
    }
}
