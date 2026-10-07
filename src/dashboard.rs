// lode/dashboard: a board of saved charts, laid out on a grid of 12 columns.
//
// A saved CHART is one statement read as a picture. A DASHBOARD is an
// arrangement of several of them, plus the dividers and text that say what the
// reader is looking at, plus one time range that they all answer to.
//
// It lives in the engine's store for the same reason saved charts and saved
// queries do: a board somebody built is something they will want from the
// editor as well, and a per-client file cannot be shared.
//
// TILES REFERENCE A CHART BY NAME, never by position in a list. An index would
// move the moment a chart was renamed, removed or re-sorted, and the board
// would go on drawing confidently — with the wrong chart under the old heading.
// A name that no longer resolves is an error with the name in it, which is a
// thing somebody can fix.
//
// WHAT THE ENGINE REFUSES, and why it is the engine that refuses it:
//
//   * a tile naming a chart that does not exist on this connection
//   * a tile outside the 12 columns, or with no width or height
//   * two tiles overlapping
//   * a time range whose ends are not timestamps, or that runs backwards
//
// A client could check all four. The point is that it does not have to be
// trusted to: a board saved by a client with a bug is a board that is wrong
// every time it is opened, by every client, for as long as it exists.

use serde::{Deserialize, Serialize};

/// The grid is twelve columns wide. Twelve because it divides by 2, 3, 4 and 6,
/// which is every arrangement anybody actually asks for — halves, thirds,
/// quarters, and a sidebar next to a wide one.
pub const COLUMNS: u32 = 12;

/// One dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    #[serde(default)]
    pub tiles: Vec<Tile>,
    /// The range every chart on this board answers to, when there is one.
    #[serde(default)]
    pub time: Option<TimeRange>,
}

/// One thing on the board.
///
/// `kind` is a tag rather than a shape per variant, so a board written by a
/// newer client is REJECTED by name ("unknown tile kind 'map'") rather than
/// silently losing the tile — a dashboard that quietly drops what it does not
/// understand is a dashboard that looks complete and is not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tile {
    pub kind: TileKind,
    /// Column of the left edge, 0-based.
    pub x: u32,
    /// Row of the top edge, 0-based. Rows are not limited: a board is as tall
    /// as it needs to be and scrolls.
    pub y: u32,
    /// Width in columns.
    pub w: u32,
    /// Height in rows.
    pub h: u32,
    /// For a chart tile: the saved chart's NAME on this connection.
    #[serde(default)]
    pub chart: String,
    /// For a text tile: what it says. For a chart tile: a heading to draw over
    /// it, or empty to use the chart's own name.
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TileKind {
    /// A saved chart, named.
    Chart,
    /// A rule across the board, to separate one group of charts from the next.
    Divider,
    /// Words. A board of numbers with nothing saying what they are is a board
    /// only its author can read.
    Text,
}

/// The range the whole board is asking about.
///
/// `column` is which column carries the time, by NAME, for the same reason a
/// chart names its measurement by name. `from` and `to` are RFC 3339 / ISO 8601
/// timestamps and are validated as such before anything is stored — see
/// `parse_timestamp`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    pub column: String,
    pub from: String,
    pub to: String,
}

/// A very strict reader for `YYYY-MM-DD` or `YYYY-MM-DDThh:mm[:ss]`, with an
/// optional trailing `Z`.
///
/// Strict on purpose, and written here rather than taken from a date library,
/// because of what the value is FOR: it is pasted into a statement by the
/// driver's own time-filter template, so "is this really a timestamp" is a
/// safety question and not a convenience one. Anything that is not exactly a
/// timestamp is refused, by this function, before it can reach a template.
///
/// Returns the canonical form, so what gets stored and what gets rendered are
/// the same string.
pub fn parse_timestamp(s: &str) -> anyhow::Result<String> {
    let s = s.trim();
    let bad = || anyhow::anyhow!("'{s}' is not a timestamp — use YYYY-MM-DD or YYYY-MM-DDThh:mm:ss");
    let core = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = match core.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (core, None),
    };
    let d: Vec<&str> = date.split('-').collect();
    if d.len() != 3 || d[0].len() != 4 || d[1].len() != 2 || d[2].len() != 2 {
        return Err(bad());
    }
    let year: u32 = d[0].parse().map_err(|_| bad())?;
    let month: u32 = d[1].parse().map_err(|_| bad())?;
    let day: u32 = d[2].parse().map_err(|_| bad())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    let Some(time) = time else {
        return Ok(format!("{year:04}-{month:02}-{day:02}"));
    };
    let t: Vec<&str> = time.split(':').collect();
    if !(2..=3).contains(&t.len()) || t.iter().any(|p| p.len() != 2) {
        return Err(bad());
    }
    let hh: u32 = t[0].parse().map_err(|_| bad())?;
    let mm: u32 = t[1].parse().map_err(|_| bad())?;
    let ss: u32 = if t.len() == 3 { t[2].parse().map_err(|_| bad())? } else { 0 };
    if hh > 23 || mm > 59 || ss > 60 {
        return Err(bad());
    }
    Ok(format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}"))
}

impl Dashboard {
    /// Everything the engine will not store, checked in one place.
    ///
    /// `charts` is the set of saved chart names on this connection. Passed in
    /// rather than looked up here so this stays a pure function and the test
    /// does not need a database to say what the rules are.
    pub fn validate(&mut self, charts: &[String]) -> anyhow::Result<()> {
        if let Some(t) = &mut self.time {
            if t.column.trim().is_empty() {
                anyhow::bail!("a time range needs the name of the column that carries the time");
            }
            // Canonicalised in place, so what is stored is what will be
            // rendered — a range that read one way on save and another on load
            // would be a board that quietly moved.
            t.from = parse_timestamp(&t.from)?;
            t.to = parse_timestamp(&t.to)?;
            if t.from > t.to {
                anyhow::bail!("the time range ends before it starts: {} to {}", t.from, t.to);
            }
        }

        for (i, tile) in self.tiles.iter().enumerate() {
            let who = tile.describe(i);
            if tile.w == 0 || tile.h == 0 {
                anyhow::bail!("{who} has no size — a tile {}×{} would draw nothing", tile.w, tile.h);
            }
            if tile.x + tile.w > COLUMNS {
                anyhow::bail!(
                    "{who} runs past the right edge: it starts at column {} and is {} wide, and the grid is {COLUMNS} columns",
                    tile.x,
                    tile.w
                );
            }
            if tile.kind == TileKind::Chart {
                if tile.chart.trim().is_empty() {
                    anyhow::bail!("{who} is a chart tile with no chart named");
                }
                if !charts.iter().any(|c| c == &tile.chart) {
                    // The name is in the message, because the name is the thing
                    // that can be fixed.
                    anyhow::bail!(
                        "{who} names the saved chart '{}', which does not exist on this connection",
                        tile.chart
                    );
                }
            }
        }

        // Overlap. A board where two tiles share a cell draws one on top of the
        // other, and the one underneath is simply gone — which looks like a
        // chart that stopped working rather than a layout that is wrong.
        for (i, a) in self.tiles.iter().enumerate() {
            for (j, b) in self.tiles.iter().enumerate().skip(i + 1) {
                if a.overlaps(b) {
                    anyhow::bail!("{} and {} overlap", a.describe(i), b.describe(j));
                }
            }
        }
        Ok(())
    }

    /// The saved charts this board needs, in the order they are laid out.
    /// What a client asks the store for when it opens the board.
    pub fn charts_used(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for t in &self.tiles {
            if t.kind == TileKind::Chart && !out.contains(&t.chart) {
                out.push(t.chart.clone());
            }
        }
        out
    }
}

impl Tile {
    /// How a refusal names this tile. The chart's name where there is one,
    /// because that is what somebody will look for on the board.
    fn describe(&self, i: usize) -> String {
        match self.kind {
            TileKind::Chart if !self.chart.is_empty() => format!("the tile for chart '{}'", self.chart),
            TileKind::Chart => format!("chart tile {}", i + 1),
            TileKind::Divider => format!("divider {}", i + 1),
            TileKind::Text => format!("text tile {}", i + 1),
        }
    }

    fn overlaps(&self, other: &Tile) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(kind: TileKind, chart: &str, x: u32, y: u32, w: u32, h: u32) -> Tile {
        Tile {
            kind,
            x,
            y,
            w,
            h,
            chart: chart.into(),
            text: String::new(),
        }
    }

    fn board(tiles: Vec<Tile>) -> Dashboard {
        Dashboard { tiles, time: None }
    }

    #[test]
    fn a_board_of_charts_that_exist_and_fit_is_accepted() {
        let mut d = board(vec![
            tile(TileKind::Chart, "revenue", 0, 0, 6, 4),
            tile(TileKind::Chart, "signups", 6, 0, 6, 4),
            tile(TileKind::Divider, "", 0, 4, 12, 1),
            tile(TileKind::Text, "", 0, 5, 12, 2),
        ]);
        d.validate(&["revenue".into(), "signups".into()]).unwrap();
        assert_eq!(d.charts_used(), vec!["revenue".to_string(), "signups".to_string()]);
    }

    /// The reason tiles name charts rather than index them: the message has to
    /// contain the name, or nobody can fix it.
    #[test]
    fn a_chart_that_does_not_exist_is_refused_by_name() {
        let mut d = board(vec![tile(TileKind::Chart, "revenue", 0, 0, 6, 4)]);
        let e = d.validate(&["signups".into()]).unwrap_err().to_string();
        assert!(e.contains("revenue"), "{e}");
        assert!(e.contains("does not exist"), "{e}");
    }

    #[test]
    fn a_tile_past_the_right_edge_is_refused_and_says_where_the_edge_is() {
        let mut d = board(vec![tile(TileKind::Chart, "revenue", 8, 0, 6, 4)]);
        let e = d.validate(&["revenue".into()]).unwrap_err().to_string();
        assert!(e.contains("12 columns"), "{e}");
    }

    #[test]
    fn a_tile_with_no_size_is_refused() {
        let mut d = board(vec![tile(TileKind::Divider, "", 0, 0, 12, 0)]);
        assert!(d.validate(&[]).unwrap_err().to_string().contains("no size"));
    }

    /// Overlap, both ways round, and the near misses that must NOT be refused:
    /// tiles that touch are not tiles that overlap.
    #[test]
    fn two_tiles_on_the_same_cell_are_refused_and_both_are_named() {
        let mut d = board(vec![
            tile(TileKind::Chart, "revenue", 0, 0, 6, 4),
            tile(TileKind::Chart, "signups", 4, 2, 6, 4),
        ]);
        let e = d
            .validate(&["revenue".into(), "signups".into()])
            .unwrap_err()
            .to_string();
        assert!(e.contains("revenue") && e.contains("signups"), "{e}");
        assert!(e.contains("overlap"), "{e}");
    }

    #[test]
    fn tiles_that_merely_touch_are_fine() {
        let mut d = board(vec![
            tile(TileKind::Chart, "a", 0, 0, 6, 4),
            // Edge to edge horizontally.
            tile(TileKind::Chart, "b", 6, 0, 6, 4),
            // Directly underneath.
            tile(TileKind::Chart, "c", 0, 4, 12, 4),
        ]);
        d.validate(&["a".into(), "b".into(), "c".into()]).unwrap();
    }

    // ── the time range ───────────────────────────────────────────────────────

    #[test]
    fn a_time_range_is_canonicalised_so_it_cannot_move_between_save_and_load() {
        let mut d = Dashboard {
            tiles: vec![],
            time: Some(TimeRange {
                column: "created_at".into(),
                from: "2026-01-05".into(),
                to: "2026-02-01 14:30Z".into(),
            }),
        };
        d.validate(&[]).unwrap();
        let t = d.time.unwrap();
        assert_eq!(t.from, "2026-01-05");
        assert_eq!(t.to, "2026-02-01T14:30:00");
    }

    /// Everything that is not exactly a timestamp. This is the check that keeps
    /// arbitrary text out of a driver's time-filter template, so it is a safety
    /// test, not a parsing one — hence the SQL in the list.
    #[test]
    fn anything_that_is_not_a_timestamp_is_refused() {
        for s in [
            "",
            "yesterday",
            "2026-1-5",
            "2026-13-01",
            "2026-01-32",
            "2026-01-05T25:00:00",
            "2026-01-05T10:70",
            "2026-01-05'; DROP TABLE people; --",
            "' OR '1'='1",
            "2026-01-05T10:00:00+02:00",
        ] {
            assert!(parse_timestamp(s).is_err(), "{s:?} was accepted as a timestamp");
        }
    }

    #[test]
    fn a_range_that_runs_backwards_is_refused() {
        let mut d = Dashboard {
            tiles: vec![],
            time: Some(TimeRange {
                column: "ts".into(),
                from: "2026-02-01".into(),
                to: "2026-01-01".into(),
            }),
        };
        assert!(d.validate(&[]).unwrap_err().to_string().contains("ends before it starts"));
    }

    #[test]
    fn a_time_range_without_a_column_is_refused() {
        let mut d = Dashboard {
            tiles: vec![],
            time: Some(TimeRange {
                column: "  ".into(),
                from: "2026-01-01".into(),
                to: "2026-02-01".into(),
            }),
        };
        assert!(d.validate(&[]).unwrap_err().to_string().contains("column"));
    }

    /// A board written by a NEWER client must not be read with its unknown
    /// tiles silently dropped: a dashboard that quietly loses what it does not
    /// understand looks complete and is not.
    #[test]
    fn a_tile_kind_from_the_future_fails_to_parse_rather_than_vanishing() {
        let json = r#"{"tiles":[{"kind":"map","x":0,"y":0,"w":6,"h":4}]}"#;
        let e = serde_json::from_str::<Dashboard>(json).unwrap_err().to_string();
        assert!(e.contains("map"), "the error must name what it did not understand: {e}");
    }
}
