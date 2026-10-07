//! The type scale, and the sizes that have to move with it.
//!
//! Every text size in this client used to be a literal at the point of use —
//! 87 of them, mostly `px(11.0)`. That is the same fault the colours had and
//! the same cure: "make the font bigger" should be ONE value, not a sweep
//! through eighty-seven places, each of which is a chance to miss one and leave
//! a panel a pixel out of step with its neighbour.
//!
//! Deliberately NOT in the theme. A palette that moved the metrics would make
//! changing colour scheme a change of layout, and the generator's header says
//! as much: radii, spacing and font sizes are facts about the APPLICATION, the
//! same for all 48 palettes. This is where they live.
//!
//! One number drives it. The steps are named by their job rather than by their
//! size, so a step can be re-tuned without every call site reading as a lie.

use gpui::{px, Pixels};

/// The base size, when nothing has been chosen.
///
/// 14, up from the 12 that `body` effectively was. The sizes this replaced ran
/// 9–12 px on a 2560×1440 screen and were reported as "extremely small"; every
/// step is two pixels bigger than the literal it replaces.
pub const DEFAULT_BASE: f32 = 14.0;

/// Below this the steps stop being a hierarchy and start being hidden text.
/// `TINY` is `base - 2`, so a base of 10 would put the smallest step at 8.
pub const MIN_BASE: f32 = 11.0;

/// Above this a panel holds four rows and the result grid holds six.
pub const MAX_BASE: f32 = 24.0;

/// The named steps, and the lengths that have to grow with them.
///
/// `Copy`, and one `f32` wide, so it can be taken by value into a closure
/// without a borrow of the workspace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    base: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics { base: DEFAULT_BASE }
    }
}

impl Metrics {
    /// Clamped on the way in, so a bad value in a session file cannot produce a
    /// window that is unreadable in either direction — and the clamp is here
    /// rather than at the settings control, because the session file is the
    /// other way a number gets in.
    pub fn new(base: f32) -> Metrics {
        // `clamp` passes NaN through — a NaN size lays nothing out and draws
        // nothing, which looks like a dead window rather than a bad setting.
        let base = if base.is_finite() {
            base.clamp(MIN_BASE, MAX_BASE)
        } else {
            DEFAULT_BASE
        };
        Metrics { base }
    }

    pub fn base(self) -> f32 {
        self.base
    }

    pub fn with_base(self, base: f32) -> Metrics {
        Metrics::new(base)
    }

    // ── the steps ────────────────────────────────────────────────────────────

    /// Section headings and anything that names a group.
    pub fn heading(self) -> Pixels {
        px(self.base + 1.0)
    }

    /// The interface: a connection, a table, a field label, a cell.
    pub fn body(self) -> Pixels {
        px(self.base)
    }

    /// The second line of a list row — what a thing IS, under what it is called.
    pub fn small(self) -> Pixels {
        px(self.base - 1.0)
    }

    /// Counters, glyph badges, the state word on an audit row.
    ///
    /// The floor of the scale. There is no step below it: the 9 px that used to
    /// exist was under the threshold of being read at all, and text too small to
    /// read is not a hierarchy, it is hidden.
    pub fn tiny(self) -> Pixels {
        px(self.base - 2.0)
    }

    /// The icon rail's glyphs, which carry no text beside them and so have to
    /// be read at a glance.
    pub fn icon(self) -> Pixels {
        px(self.base + 5.0)
    }

    // ── the lengths that must move with the type ─────────────────────────────

    /// One row of the result grid.
    ///
    /// Derived, because this is the pair that goes wrong first: 13 px of text in
    /// a 20 px row is pinched, and the pinch is invisible in a screenshot of
    /// the old size. 1.7× is what the old 20 px was against the old 12 px body.
    pub fn row_h(self) -> f32 {
        (self.base * 1.7).round()
    }

    /// A grid column, before anybody drags it. Widths a person has set are
    /// remembered and are NOT rescaled: a column somebody sized is a decision,
    /// and re-deriving it would undo that decision every time the font moved.
    pub fn col_w(self) -> f32 {
        (self.base * 11.0).round()
    }

    /// The height of one row in the icon rail.
    pub fn rail_h(self) -> f32 {
        (self.base * 2.9).round()
    }

    /// The side panel's width. Grows with the type, because a panel that stayed
    /// 280 px while the text in it grew would simply hold fewer words.
    pub fn panel_w(self) -> f32 {
        (self.base * 20.0).round()
    }

    /// How wide one line of free text in a panel may be.
    ///
    /// An explicit number, not `w_full`, and that is not a style choice. GPUI
    /// truncates text only when it is measured against a DEFINITE width
    /// (`known_dimensions.width`, or `AvailableSpace::Definite`); a percentage
    /// width inside a flex chain is neither at measure time, so `w_full` +
    /// `text_ellipsis` clips the text and never draws the ellipsis. Giving the
    /// line a real width is what turns a hard cut into a `…`.
    ///
    /// `px_2` of padding on each side is what every panel row uses.
    pub fn line_w(self) -> f32 {
        self.panel_w() - 16.0
    }

    /// The outcome column of an audit row — "disconnected" is the longest word
    /// that goes in it.
    pub fn outcome_w(self) -> f32 {
        (self.base * 5.6).round()
    }

    /// Horizontal padding inside a grid cell — half a character, roughly.
    pub fn cell_pad(self) -> f32 {
        (self.base * 0.45).round()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The steps are ordered, and the floor is the floor. A scale whose `small`
    /// could overtake its `body` would be a scale in name only.
    #[test]
    fn the_steps_stay_in_order_at_every_base() {
        let mut b = MIN_BASE;
        while b <= MAX_BASE {
            let m = Metrics::new(b);
            assert!(m.tiny() < m.small(), "base {b}");
            assert!(m.small() < m.body(), "base {b}");
            assert!(m.body() < m.heading(), "base {b}");
            assert!(m.heading() < m.icon(), "base {b}");
            b += 0.5;
        }
    }

    /// The reported complaint, as an assertion: every step is two pixels bigger
    /// than the literal it replaced (11 → 13, 10 → 12, 12 → 14, 17 → 19), and
    /// nothing is 9 any more.
    #[test]
    fn the_default_is_two_pixels_up_from_what_it_replaced() {
        let m = Metrics::default();
        assert_eq!(m.small(), px(13.0), "was px(11.0)");
        assert_eq!(m.tiny(), px(12.0), "was px(10.0), and px(9.0) is gone");
        assert_eq!(m.body(), px(14.0), "was px(12.0)");
        assert_eq!(m.heading(), px(15.0), "was px(13.0)");
        assert_eq!(m.icon(), px(19.0), "was px(17.0)");
    }

    /// Text must not outgrow the row it sits in. This is the check that a
    /// screenshot at one size cannot make.
    #[test]
    fn a_row_always_has_room_for_the_text_in_it() {
        let mut b = MIN_BASE;
        while b <= MAX_BASE {
            let m = Metrics::new(b);
            assert!(
                m.row_h() >= f32::from(m.body()) + 4.0,
                "base {b}: a {}px row cannot hold {}px text",
                m.row_h(),
                f32::from(m.body())
            );
            b += 0.5;
        }
    }

    /// A number from a session file written by hand, or by an older version,
    /// cannot make the window unreadable either way.
    #[test]
    fn a_base_from_outside_is_clamped() {
        assert_eq!(Metrics::new(0.0).base(), MIN_BASE);
        assert_eq!(Metrics::new(-40.0).base(), MIN_BASE);
        assert_eq!(Metrics::new(400.0).base(), MAX_BASE);
        assert_eq!(Metrics::new(f32::NAN).base(), DEFAULT_BASE, "NaN must not survive as a size");
        assert_eq!(Metrics::new(f32::INFINITY).base(), DEFAULT_BASE);
        assert_eq!(Metrics::new(15.0).base(), 15.0);
    }
}
