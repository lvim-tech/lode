//! A dashboard open in the client: the layout, what each tile has loaded, and
//! the arithmetic of dragging a tile around a grid of twelve columns.
//!
//! The arithmetic lives here, apart from the drawing, because it is the part
//! that can be WRONG in ways a screenshot does not show — a tile that snaps one
//! column left of the pointer, a resize that walks off the right edge, a drop
//! that lands on top of another tile. Those are assertions, not impressions.
//!
//! The engine is still the authority: it refuses a board whose tiles overlap or
//! run past the edge, whatever this file computes. What this file is for is
//! making the drag feel right and never ASKING the engine to store something it
//! will refuse.

use std::collections::HashMap;

use lode_proto::{Column, Dashboard, SavedChart, Tile, TimeFit, TileKind};
use serde_json::Value;

use crate::workspace::RunState;

/// The grid's width. Mirrors `dashboard::COLUMNS` in the engine, and the engine
/// sends its own value with every board — this is the fallback for a board that
/// has not been loaded yet.
pub const COLUMNS: u32 = 12;

/// The smallest a tile may be, in cells. One column of a twelve-column grid is
/// about eighty pixels: wide enough for a divider, too narrow for a chart, and
/// it is not this file's business to say which.
pub const MIN_W: u32 = 1;
pub const MIN_H: u32 = 1;

/// One dashboard, open.
pub struct Board {
    /// The board's name in the store — what `store.dashboards.save` is called
    /// with.
    pub name: String,
    pub dash: Dashboard,
    /// What the engine said is wrong with it, usually a chart that was deleted
    /// from under it. Shown, not hidden: a layout somebody spent time on should
    /// be repairable.
    pub stale: Option<String>,
    pub columns: u32,
    /// The saved charts on this connection, so a tile can find the statement
    /// behind the name it holds.
    pub charts: Vec<SavedChart>,
    /// What each chart tile has loaded, by tile index.
    ///
    /// Keyed by index rather than by chart name because the same chart may
    /// legitimately appear twice on a board, at two sizes or under two
    /// headings, and the two tiles are then showing the same data and must not
    /// share one slot of state.
    pub panes: HashMap<usize, Pane>,
    /// How often to run the board again, if at all.
    ///
    /// Only while its tab is in FRONT. A board left open in a background tab
    /// refreshing every ten seconds is a query every ten seconds against a
    /// database nobody is looking at, which is a cost somebody else pays.
    pub refresh_every: Option<std::time::Duration>,
    /// When it was last run, so the interval is an interval and not a race with
    /// the frame rate.
    pub refreshed_at: Option<std::time::Instant>,
    /// Whether tiles can be moved and resized. Off by default: a board is for
    /// reading, and a layout that rearranges itself because somebody dragged
    /// across it to select a number is a layout that fights its reader.
    pub editing: bool,
    pub drag: Option<Drag>,
    /// Changed since it was last saved.
    pub dirty: bool,
    /// Which tile is selected, for the delete and the heading box.
    pub selected: Option<usize>,
}

/// One chart tile's result.
#[derive(Default)]
pub struct Pane {
    pub call_id: Option<u64>,
    pub state: RunState,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub error: Option<String>,
    /// What became of the board's time range for this chart. Anything but
    /// `Applied` is SHOWN on the tile: a range that silently applies to four
    /// charts out of six is worse than one that says which two it missed.
    pub fit: TimeFit,
    /// The statement as it was run, after the time range went in. Shown on
    /// demand, because "why is this chart showing that" is answered by it.
    pub statement: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragMode {
    Move,
    Resize,
}

/// A drag in progress.
pub struct Drag {
    pub tile: usize,
    pub mode: DragMode,
    /// Where the pointer was when it started.
    pub from: (f32, f32),
    /// The tile's cell rectangle when it started. The drag is computed from
    /// THIS every frame rather than accumulated, so a sequence of rounded steps
    /// cannot drift away from the pointer.
    pub start: (u32, u32, u32, u32),
}

/// One cell's size in pixels, from the canvas's width.
pub fn cell_w(canvas_w: f32, columns: u32, gap: f32) -> f32 {
    let columns = columns.max(1) as f32;
    ((canvas_w - gap * (columns - 1.0)) / columns).max(1.0)
}

/// Where a tile's rectangle lands on the canvas, in pixels.
pub fn tile_px(t: &Tile, cw: f32, rh: f32, gap: f32) -> (f32, f32, f32, f32) {
    (
        t.x as f32 * (cw + gap),
        t.y as f32 * (rh + gap),
        t.w as f32 * cw + (t.w.saturating_sub(1)) as f32 * gap,
        t.h as f32 * rh + (t.h.saturating_sub(1)) as f32 * gap,
    )
}

/// Where a MOVE would put the tile: cells, snapped and clamped to the grid.
///
/// Computed from the drag's starting rectangle and the TOTAL pointer movement,
/// never from the previous frame: accumulating rounded steps is how a tile ends
/// up a column away from the pointer after a long drag.
pub fn moved(start: (u32, u32, u32, u32), d: (f32, f32), cw: f32, rh: f32, gap: f32, columns: u32) -> (u32, u32) {
    let (x0, y0, w, _h) = start;
    let dx = (d.0 / (cw + gap)).round() as i64;
    let dy = (d.1 / (rh + gap)).round() as i64;
    let max_x = columns.saturating_sub(w) as i64;
    let x = (x0 as i64 + dx).clamp(0, max_x) as u32;
    // No bottom: a board is as tall as it needs to be and scrolls.
    let y = (y0 as i64 + dy).max(0) as u32;
    (x, y)
}

/// Where a RESIZE would leave the tile: its new width and height in cells.
///
/// The left/top edges do not move, so a resize can never push the tile off the
/// left of the grid; the right edge is clamped to the last column.
pub fn resized(start: (u32, u32, u32, u32), d: (f32, f32), cw: f32, rh: f32, gap: f32, columns: u32) -> (u32, u32) {
    let (x, _y, w0, h0) = start;
    let dw = (d.0 / (cw + gap)).round() as i64;
    let dh = (d.1 / (rh + gap)).round() as i64;
    let max_w = columns.saturating_sub(x).max(MIN_W) as i64;
    let w = (w0 as i64 + dw).clamp(MIN_W as i64, max_w) as u32;
    let h = (h0 as i64 + dh).max(MIN_H as i64) as u32;
    (w, h)
}

/// Whether a candidate rectangle would sit on top of any tile but `ignore`.
///
/// The engine refuses an overlapping board outright. This is so the client
/// never ASKS it to: a drop that would overlap is simply not taken, and the
/// tile stays where it was.
pub fn collides(tiles: &[Tile], ignore: usize, rect: (u32, u32, u32, u32)) -> bool {
    let (x, y, w, h) = rect;
    tiles.iter().enumerate().any(|(i, t)| {
        i != ignore && x < t.x + t.w && t.x < x + w && y < t.y + t.h && t.y < y + h
    })
}

/// The first row at which a `w`-wide tile fits across the whole grid.
///
/// Where a new tile goes when nobody has said where. Below everything, rather
/// than in the first gap: a tile that appears in a hole in the middle of a board
/// is a tile somebody has to go looking for.
pub fn next_row(tiles: &[Tile]) -> u32 {
    tiles.iter().map(|t| t.y + t.h).max().unwrap_or(0)
}

impl Board {
    pub fn new(name: String, dash: Dashboard, stale: Option<String>, columns: u32) -> Board {
        Board {
            name,
            dash,
            stale,
            columns: if columns == 0 { COLUMNS } else { columns },
            charts: Vec::new(),
            panes: HashMap::new(),
            refresh_every: None,
            refreshed_at: None,
            editing: false,
            drag: None,
            dirty: false,
            selected: None,
        }
    }

    /// The saved chart a tile names, or `None` when it has gone away.
    pub fn chart_of(&self, t: &Tile) -> Option<&SavedChart> {
        self.charts.iter().find(|c| c.name == t.chart)
    }

    /// Add a tile at the bottom of the board.
    pub fn push(&mut self, kind: TileKind, chart: &str, text: &str, w: u32, h: u32) {
        let y = next_row(&self.dash.tiles);
        self.dash.tiles.push(Tile {
            kind,
            x: 0,
            y,
            w: w.min(self.columns),
            h,
            chart: chart.to_string(),
            text: text.to_string(),
        });
        self.dirty = true;
        self.selected = Some(self.dash.tiles.len() - 1);
    }

    /// Remove a tile, keeping the selection sane.
    ///
    /// The panes are keyed by INDEX, so removing a tile renumbers every tile
    /// after it — and a pane left behind under its old key would then be drawn
    /// on the wrong tile. Dropping them all is the cheap correct answer: the
    /// board reloads, which is what it does on open anyway.
    pub fn remove(&mut self, i: usize) {
        if i >= self.dash.tiles.len() {
            return;
        }
        self.dash.tiles.remove(i);
        self.panes.clear();
        self.selected = None;
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(x: u32, y: u32, w: u32, h: u32) -> Tile {
        Tile {
            kind: TileKind::Chart,
            x,
            y,
            w,
            h,
            chart: "c".into(),
            text: String::new(),
        }
    }

    /// A drag of exactly one cell moves exactly one cell, and a drag of nothing
    /// moves nothing. The second half is the one that matters: a click that is
    /// read as a one-pixel drag must not nudge the tile.
    #[test]
    fn a_move_snaps_to_the_cell_the_pointer_is_over() {
        let (cw, rh, gap) = (100.0, 60.0, 8.0);
        let start = (2, 1, 3, 2);
        assert_eq!(moved(start, (0.0, 0.0), cw, rh, gap, 12), (2, 1));
        assert_eq!(moved(start, (108.0, 0.0), cw, rh, gap, 12), (3, 1));
        assert_eq!(moved(start, (-108.0, 68.0), cw, rh, gap, 12), (1, 2));
        // Just under half a cell is still the same cell.
        assert_eq!(moved(start, (50.0, 0.0), cw, rh, gap, 12), (2, 1));
    }

    /// The right edge of the grid is a wall, not a suggestion — a tile dragged
    /// at it stops with its own right edge on the last column.
    #[test]
    fn a_move_cannot_push_a_tile_off_the_grid() {
        let (cw, rh, gap) = (100.0, 60.0, 8.0);
        let start = (8, 0, 4, 2);
        assert_eq!(moved(start, (10_000.0, 0.0), cw, rh, gap, 12), (8, 0));
        assert_eq!(moved(start, (-10_000.0, -10_000.0), cw, rh, gap, 12), (0, 0));
        // A narrower tile may go further right.
        assert_eq!(moved((8, 0, 1, 2), (10_000.0, 0.0), cw, rh, gap, 12), (11, 0));
    }

    /// Computed from the START, every frame. Accumulating rounded steps is how
    /// a tile ends up a column away from the pointer after a long drag, and the
    /// assertion is that twenty small steps land where one big one does.
    #[test]
    fn a_long_drag_lands_where_the_pointer_is() {
        let (cw, rh, gap) = (100.0, 60.0, 8.0);
        let start = (0, 0, 2, 2);
        let total = 20.0 * 54.0;
        let one_go = moved(start, (total, 0.0), cw, rh, gap, 12);
        let mut acc = (0.0, 0.0);
        let mut step_by_step = start;
        for _ in 0..20 {
            acc.0 += 54.0;
            let (x, y) = moved(start, acc, cw, rh, gap, 12);
            step_by_step = (x, y, start.2, start.3);
        }
        assert_eq!((step_by_step.0, step_by_step.1), one_go);
    }

    #[test]
    fn a_resize_keeps_the_left_edge_and_stops_at_the_last_column() {
        let (cw, rh, gap) = (100.0, 60.0, 8.0);
        let start = (6, 0, 3, 2);
        assert_eq!(resized(start, (108.0, 0.0), cw, rh, gap, 12), (4, 2));
        assert_eq!(resized(start, (10_000.0, 0.0), cw, rh, gap, 12), (6, 2));
        // Never smaller than one cell, however far back it is dragged.
        assert_eq!(resized(start, (-10_000.0, -10_000.0), cw, rh, gap, 12), (1, 1));
    }

    #[test]
    fn a_resize_grows_downwards_without_limit() {
        let (cw, rh, gap) = (100.0, 60.0, 8.0);
        assert_eq!(resized((0, 0, 2, 2), (0.0, 68.0 * 30.0), cw, rh, gap, 12), (2, 32));
    }

    /// Collision, both ways round, and the near miss: tiles that TOUCH do not
    /// overlap, or a board could never be packed.
    #[test]
    fn a_drop_onto_another_tile_is_a_collision_and_one_beside_it_is_not() {
        let tiles = vec![t(0, 0, 6, 4), t(6, 0, 6, 4)];
        assert!(collides(&tiles, 0, (4, 2, 6, 4)), "half over the second tile");
        // Moving tile 0 onto exactly where tile 0 already is, is not a collision
        // with itself.
        assert!(!collides(&tiles, 0, (0, 0, 6, 4)));
        // Edge to edge.
        assert!(!collides(&tiles, 99, (0, 4, 12, 2)));
        assert!(!collides(&tiles, 99, (0, 0, 0, 0)));
    }

    #[test]
    fn a_new_tile_goes_below_everything_rather_than_into_a_hole() {
        let tiles = vec![t(0, 0, 6, 4), t(6, 2, 6, 6)];
        assert_eq!(next_row(&tiles), 8);
        assert_eq!(next_row(&[]), 0);
    }

    /// Twelve columns and eleven gaps have to add up to the canvas, or the last
    /// tile hangs over the edge.
    #[test]
    fn the_cells_and_the_gaps_add_up_to_the_canvas() {
        let w = 1000.0;
        let cw = cell_w(w, 12, 8.0);
        let total = cw * 12.0 + 8.0 * 11.0;
        assert!((total - w).abs() < 0.01, "{total} != {w}");
        // A full-width tile is the whole canvas.
        let full = t(0, 0, 12, 1);
        let (_, _, tw, _) = tile_px(&full, cw, 60.0, 8.0);
        assert!((tw - w).abs() < 0.01, "{tw} != {w}");
    }

    /// Removing a tile renumbers the ones after it, so the panes keyed by index
    /// have to go — a pane left under its old key draws the wrong chart's data
    /// on a tile.
    #[test]
    fn removing_a_tile_drops_the_loaded_panes() {
        let mut b = Board::new("ops".into(), Dashboard::default(), None, 12);
        b.dash.tiles = vec![t(0, 0, 6, 4), t(6, 0, 6, 4)];
        b.panes.insert(0, Pane::default());
        b.panes.insert(1, Pane::default());
        b.remove(0);
        assert_eq!(b.dash.tiles.len(), 1);
        assert!(b.panes.is_empty(), "a pane under a stale index draws on the wrong tile");
        assert!(b.dirty);
    }
}
