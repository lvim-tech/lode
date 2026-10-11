//! The schema diagram: where the boxes go, apart from the drawing.
//!
//! Every function here is pure and tested, for the same reason the chart's
//! arithmetic is: a layout that puts two tables on top of each other, or that
//! never terminates on a schema with a cycle in it, is not something to find
//! out by looking at it. The drawing is in `app.rs`, where the theme is.
//!
//! The BOXES are drawn as ordinary elements, not painted on the canvas; only
//! the lines between them are painted. A table's name and its columns are
//! text, and text wants the text system, hover, selection and a click — all of
//! which an element has and a painted rectangle does not.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// One table in the diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub schema: Option<String>,
    /// Its columns, in the order the engine gave them, with a mark for the key
    /// and for a reference.
    pub columns: Vec<Col>,
    /// Where its box is, in diagram coordinates. Moved by dragging.
    pub at: (f32, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Col {
    pub name: String,
    pub primary: bool,
    /// The table this column points at, when it does.
    pub points_at: Option<String>,
}

/// One reference, by table name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: String,
    pub from_col: String,
    pub to: String,
    pub to_col: String,
}

/// The edges of a set of tables, read off their columns.
///
/// Only edges whose BOTH ends are in the diagram: a reference to a table that
/// was not loaded would be a line to nowhere, and a line to nowhere reads as a
/// bug in the layout rather than as a table that is missing.
pub fn edges(tables: &[Table]) -> Vec<Edge> {
    let present: BTreeSet<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let mut out = Vec::new();
    for t in tables {
        for c in &t.columns {
            let Some(target) = c.points_at.as_deref() else { continue };
            if !present.contains(target) {
                continue;
            }
            out.push(Edge {
                from: t.name.clone(),
                from_col: c.name.clone(),
                to: target.to_string(),
                to_col: String::new(),
            });
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Which column of the layout each table belongs in.
///
/// A table goes to the RIGHT of everything it points at, so the arrows read
/// left to right and the tables nothing depends on end up at the left edge.
/// That is the arrangement every schema diagram worth reading has, and the
/// reason is that a foreign key is a dependency: `orders` needs `customers` to
/// exist, so `customers` comes first.
///
/// A CYCLE — two tables pointing at each other, which a schema is allowed to
/// have — would make "to the right of everything it points at" impossible. The
/// cycle is broken by layering what can be layered and putting whatever is
/// left in the column after the deepest thing it depends on, which is the
/// honest approximation: the picture is still readable and no table is
/// omitted.
pub fn layers(tables: &[Table], edges: &[Edge]) -> BTreeMap<String, usize> {
    // How many of its dependencies are still unplaced, and who depends on it.
    let mut depends_on: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut depended_by: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for t in tables {
        depends_on.entry(t.name.as_str()).or_default();
        depended_by.entry(t.name.as_str()).or_default();
    }
    for e in edges {
        // A self-reference is not a dependency on another layer: a tree table
        // pointing at its own parent would otherwise never be placeable.
        if e.from == e.to {
            continue;
        }
        depends_on.entry(e.from.as_str()).or_default().insert(e.to.as_str());
        depended_by.entry(e.to.as_str()).or_default().insert(e.from.as_str());
    }
    let mut layer: BTreeMap<String, usize> = BTreeMap::new();
    let mut left: BTreeMap<&str, usize> =
        depends_on.iter().map(|(k, v)| (*k, v.len())).collect();
    let mut ready: VecDeque<&str> =
        left.iter().filter(|(_, n)| **n == 0).map(|(k, _)| *k).collect();
    while let Some(name) = ready.pop_front() {
        let at = depends_on
            .get(name)
            .map(|deps| {
                deps.iter()
                    .filter_map(|d| layer.get(*d))
                    .map(|l| l + 1)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        layer.insert(name.to_string(), at);
        for dependant in depended_by.get(name).cloned().unwrap_or_default() {
            if let Some(n) = left.get_mut(dependant) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    ready.push_back(dependant);
                }
            }
        }
    }
    // Whatever is left is in a cycle. Placed after the deepest thing it
    // depends on that DID get placed, which keeps the arrows mostly
    // left-to-right without pretending the cycle is not there.
    for t in tables {
        if layer.contains_key(&t.name) {
            continue;
        }
        let at = depends_on
            .get(t.name.as_str())
            .map(|deps| {
                deps.iter().filter_map(|d| layer.get(*d)).map(|l| l + 1).max().unwrap_or(0)
            })
            .unwrap_or(0);
        layer.insert(t.name.clone(), at);
    }
    layer
}

/// The width a box gets. Fixed, so the columns of the layout line up.
pub const BOX_W: f32 = 180.0;
/// The space between two columns of boxes, which is where the lines run.
pub const GAP_X: f32 = 90.0;
/// The space between two boxes in one column.
pub const GAP_Y: f32 = 24.0;

/// Where every box goes, from its layer and the height each one needs.
///
/// `height_of` is given rather than computed because the height depends on how
/// many columns are being SHOWN, which is a drawing decision — a box listing
/// forty columns is a box as tall as the window.
pub fn place(
    tables: &[Table],
    layer: &BTreeMap<String, usize>,
    height_of: impl Fn(&Table) -> f32,
) -> BTreeMap<String, (f32, f32)> {
    // Grouped by layer, in a stable order: the same schema must lay out the
    // same way twice, or the picture jumps about every time it is reopened.
    let mut by_layer: BTreeMap<usize, Vec<&Table>> = BTreeMap::new();
    for t in tables {
        by_layer.entry(layer.get(&t.name).copied().unwrap_or(0)).or_default().push(t);
    }
    let mut out = BTreeMap::new();
    for (l, mut group) in by_layer {
        group.sort_by(|a, b| a.name.cmp(&b.name));
        let x = l as f32 * (BOX_W + GAP_X);
        let mut y = 0.0f32;
        for t in group {
            out.insert(t.name.clone(), (x, y));
            y += height_of(t) + GAP_Y;
        }
    }
    out
}

/// How tall a box is for `shown` columns, at this line height.
pub fn box_h(shown: usize, line_h: f32, head_h: f32) -> f32 {
    head_h + shown as f32 * line_h
}

/// How much of the diagram there is, so the canvas can be that big.
pub fn extent(
    places: &BTreeMap<String, (f32, f32)>,
    heights: &BTreeMap<String, f32>,
) -> (f32, f32) {
    let mut w = 0.0f32;
    let mut h = 0.0f32;
    for (name, (x, y)) in places {
        w = w.max(x + BOX_W);
        h = h.max(y + heights.get(name).copied().unwrap_or(0.0));
    }
    (w, h)
}

/// Where a line between two boxes starts and ends.
///
/// From the RIGHT edge of the one that points and into the LEFT edge of the
/// one pointed at, when the target is to the right; the other way round when
/// it is to the left. A line that always left by the right edge would cross
/// its own box whenever the layout put the target behind it — which happens
/// wherever there is a cycle.
pub fn line_ends(
    from: (f32, f32),
    from_h: f32,
    to: (f32, f32),
    to_h: f32,
) -> ((f32, f32), (f32, f32)) {
    let fy = from.1 + from_h / 2.0;
    let ty = to.1 + to_h / 2.0;
    if to.0 >= from.0 {
        ((from.0 + BOX_W, fy), (to.0, ty))
    } else {
        ((from.0, fy), (to.0 + BOX_W, ty))
    }
}

/// The zoom steps offered, as percentages of the natural size.
pub const ZOOMS: &[f32] = &[0.5, 0.75, 1.0, 1.5];

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, points: &[(&str, &str)]) -> Table {
        Table {
            name: name.into(),
            schema: None,
            columns: std::iter::once(Col {
                name: "id".into(),
                primary: true,
                points_at: None,
            })
            .chain(points.iter().map(|(col, target)| Col {
                name: (*col).into(),
                primary: false,
                points_at: Some((*target).to_string()),
            }))
            .collect(),
            at: (0.0, 0.0),
        }
    }

    /// A reference to a table that is not in the picture is not drawn.
    ///
    /// A line to nowhere reads as a bug in the layout rather than as a table
    /// somebody did not load.
    #[test]
    fn an_edge_needs_both_of_its_ends() {
        let tables = vec![t("orders", &[("customer_id", "customers")]), t("customers", &[])];
        let e = edges(&tables);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].from, "orders");
        assert_eq!(e[0].to, "customers");

        // The same table alone: the reference has nowhere to go.
        let alone = vec![t("orders", &[("customer_id", "customers")])];
        assert!(edges(&alone).is_empty());
        // And nothing from nothing.
        assert!(edges(&[]).is_empty());
    }

    /// A table goes to the right of everything it points at.
    #[test]
    fn the_arrows_read_left_to_right() {
        let tables = vec![
            t("lines", &[("order_id", "orders"), ("product_id", "products")]),
            t("orders", &[("customer_id", "customers")]),
            t("customers", &[]),
            t("products", &[]),
        ];
        let e = edges(&tables);
        let l = layers(&tables, &e);
        assert_eq!(l["customers"], 0);
        assert_eq!(l["products"], 0);
        assert_eq!(l["orders"], 1, "{l:?}");
        // `lines` points at `orders` (layer 1) and `products` (layer 0), so it
        // goes after the DEEPEST of them.
        assert_eq!(l["lines"], 2, "{l:?}");
    }

    /// A cycle is laid out anyway, and every table is placed.
    ///
    /// Two tables pointing at each other is legal and not rare — a user with a
    /// current_order and an order with a user. A layout that hung, or that
    /// dropped one of them, would be worse than an approximate picture.
    #[test]
    fn a_cycle_does_not_hang_and_loses_nobody() {
        let tables = vec![
            t("users", &[("current_order", "orders")]),
            t("orders", &[("user_id", "users")]),
            t("products", &[]),
        ];
        let e = edges(&tables);
        let l = layers(&tables, &e);
        assert_eq!(l.len(), 3, "{l:?}");
        assert!(l.contains_key("users") && l.contains_key("orders"));
        assert_eq!(l["products"], 0);

        // A SELF-reference is not a dependency on another layer: a tree table
        // would otherwise never be placeable at all.
        let tree = vec![t("nodes", &[("parent_id", "nodes")])];
        let l = layers(&tree, &edges(&tree));
        assert_eq!(l["nodes"], 0, "{l:?}");
    }

    /// The same schema lays out the same way twice.
    #[test]
    fn a_layout_does_not_move_when_nothing_changed() {
        let tables = vec![
            t("b", &[("a_id", "a")]),
            t("a", &[]),
            t("c", &[("a_id", "a")]),
        ];
        let e = edges(&tables);
        let l = layers(&tables, &e);
        let h = |_: &Table| 100.0;
        let first = place(&tables, &l, h);
        let again = place(&tables, &l, h);
        assert_eq!(first, again);
        // `a` is alone on the left; `b` and `c` share the next column and do
        // not overlap.
        assert_eq!(first["a"].0, 0.0);
        assert_eq!(first["b"].0, BOX_W + GAP_X);
        assert_eq!(first["c"].0, BOX_W + GAP_X);
        assert!(first["c"].1 >= first["b"].1 + 100.0, "{first:?}");
        // In NAME order, so reopening it gives the same picture.
        assert!(first["b"].1 < first["c"].1);
    }

    /// The extent covers every box, so nothing is drawn outside the canvas.
    #[test]
    fn the_canvas_is_as_big_as_the_picture() {
        let mut places = BTreeMap::new();
        places.insert("a".to_string(), (0.0, 0.0));
        places.insert("b".to_string(), (270.0, 124.0));
        let mut heights = BTreeMap::new();
        heights.insert("a".to_string(), 100.0);
        heights.insert("b".to_string(), 80.0);
        let (w, h) = extent(&places, &heights);
        assert_eq!(w, 270.0 + BOX_W);
        assert_eq!(h, 124.0 + 80.0);
        // Nothing is nothing.
        assert_eq!(extent(&BTreeMap::new(), &BTreeMap::new()), (0.0, 0.0));
    }

    /// A line leaves by the edge that faces its target.
    #[test]
    fn a_line_does_not_cross_its_own_box() {
        // Target to the RIGHT: out of the right edge, into the left.
        let (a, b) = line_ends((0.0, 0.0), 100.0, (270.0, 0.0), 80.0);
        assert_eq!(a.0, BOX_W);
        assert_eq!(b.0, 270.0);
        assert_eq!(a.1, 50.0, "the middle of the box it leaves");
        assert_eq!(b.1, 40.0, "the middle of the box it enters");
        // Target to the LEFT — which a cycle produces: out of the left edge,
        // into the right.
        let (a, b) = line_ends((270.0, 0.0), 100.0, (0.0, 0.0), 80.0);
        assert_eq!(a.0, 270.0);
        assert_eq!(b.0, BOX_W);
    }

    /// A box is as tall as what it shows.
    #[test]
    fn a_box_is_as_tall_as_the_columns_in_it() {
        assert_eq!(box_h(0, 18.0, 30.0), 30.0, "a box with nothing in it is its heading");
        assert_eq!(box_h(4, 18.0, 30.0), 30.0 + 72.0);
        // The zoom steps include the natural size, or there would be no way
        // back to it.
        assert!(ZOOMS.contains(&1.0));
    }
}
