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

/// The icon rail's own size, independent of the text.
///
/// Its OWN number, not a step off `base`, and that is the point: the rail is
/// NAVIGATION, not content. Somebody who makes the text bigger to read a result
/// is not asking for bigger buttons, and somebody who wants the rail easier to
/// hit is not asking for a bigger grid. Two separate wishes, two separate
/// numbers.
///
/// Chosen for a wide screen and read at arm's length, not chosen cautiously:
/// at the old size the glyph took about a third of the rail's width and read
/// as a dot rather than as a sign. Somebody who finds it too big now has a
/// setting; somebody who finds it too small has to guess there is one.
pub const DEFAULT_ICON: f32 = 28.0;

/// Defaults this client has shipped before.
///
/// A session has always written the icon size out, chosen or not, so nobody is
/// distinguishable as "never picked one" — and raising the default would reach
/// nobody who had ever opened the window. A stored value equal to a default we
/// once shipped is therefore treated as "not chosen": nobody sets a size by
/// hand to exactly the number it already was.
///
/// It is written out from now on only when it DIFFERS from the current default,
/// so this list does not have to grow again.
pub const SHIPPED_ICON_DEFAULTS: [f32; 2] = [26.0, 28.0];


pub const MIN_ICON: f32 = 12.0;
pub const MAX_ICON: f32 = 40.0;

/// The named steps, and the lengths that have to grow with them.
///
/// `Copy`, and one `f32` wide, so it can be taken by value into a closure
/// without a borrow of the workspace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    base: f32,
    icon: f32,
    /// The panel's width when somebody has dragged one, else derived.
    panel: Option<f32>,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics {
            base: DEFAULT_BASE,
            icon: DEFAULT_ICON,
            panel: None,
        }
    }
}

impl Metrics {
    /// Clamped on the way in, so a bad value in a session file cannot produce a
    /// window that is unreadable in either direction — and the clamp is here
    /// rather than at the settings control, because the session file is the
    /// other way a number gets in.
    pub fn new(base: f32) -> Metrics {
        Metrics::default().with_base(base)
    }

    /// Clamp one number, with NaN sent to the default rather than through.
    ///
    /// `clamp` passes NaN along, and a NaN size lays nothing out and draws
    /// nothing — which looks like a dead window rather than like a bad setting.
    fn ok(v: f32, lo: f32, hi: f32, fallback: f32) -> f32 {
        if v.is_finite() {
            v.clamp(lo, hi)
        } else {
            fallback
        }
    }

    pub fn base(self) -> f32 {
        self.base
    }

    pub fn icon_size(self) -> f32 {
        self.icon
    }

    pub fn with_base(self, base: f32) -> Metrics {
        Metrics {
            base: Self::ok(base, MIN_BASE, MAX_BASE, DEFAULT_BASE),
            ..self
        }
    }

    pub fn with_icon(self, icon: f32) -> Metrics {
        Metrics {
            icon: Self::ok(icon, MIN_ICON, MAX_ICON, DEFAULT_ICON),
            ..self
        }
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

    /// An icon that sits BESIDE text, at the size of that text.
    ///
    /// Not the text size itself, and the difference is the whole point. A
    /// Font Awesome mark's ink fills less of its em box than a letter's does,
    /// so a glyph set to the same point size as the words next to it comes out
    /// visibly smaller — which is the complaint, and it was correct. The
    /// factor makes the two OPTICALLY equal rather than numerically equal.
    ///
    /// The one place this does not apply is the icon RAIL down the left edge,
    /// which has a number of its own that a person sets — see `icon_size`.
    /// Those marks sit beside no text and are the thing being pointed at, so
    /// tying them to a text step would take away the setting.
    pub fn icon_beside(self, text: Pixels) -> Pixels {
        px((f32::from(text) * 1.2).round())
    }

    /// The icon rail's glyphs. Its OWN number — see `DEFAULT_ICON`.
    pub fn icon(self) -> Pixels {
        px(self.icon)
    }

    /// The size for a control that is an ACTION rather than a label.
    ///
    /// The rail's size, wherever it appears: the pencil that edits a connection, the
    /// cross that closes a tab. They were drawn at a TEXT step — `tiny` and `small`
    /// — so on a palette with a quiet accent they were nearly invisible, and an
    /// action nobody can see is an action nobody knows is there.
    pub fn action(self) -> Pixels {
        px((self.icon * 0.72).round())
    }

    /// The square an action is clicked in.
    ///
    /// Bigger than the glyph on purpose. A target the size of its own drawing is a
    /// target people miss, and these sit at the end of a narrow row.
    pub fn action_box(self) -> f32 {
        (self.icon * 1.15).round()
    }

    /// The square an icon is drawn in.
    ///
    /// A BOX, not a line of text. Font Awesome's glyphs are drawn at different
    /// widths and ink heights, so six of them at one point size, each sitting
    /// on a text baseline, come out visibly different sizes — which is exactly
    /// what they looked like. Centred in a fixed square, what differs is only
    /// the drawing inside it.
    pub fn icon_box(self) -> f32 {
        (self.icon * 1.6).round()
    }

    /// The rail's width: the icon's box and the same air on both sides.
    ///
    /// DERIVED, so a bigger icon widens the rail by itself. A second number
    /// here would be a second place to set the same thing, and they would drift.
    pub fn rail_w(self) -> f32 {
        (self.icon_box() + self.icon * 1.0).round()
    }

    // ── the lengths that must move with the type ─────────────────────────────

    /// One row of the result grid.
    ///
    /// Derived, because this is the pair that goes wrong first: 13 px of text in
    /// a 20 px row is pinched, and the pinch is invisible in a screenshot of
    /// the old size. 1.7× is what the old 20 px was against the old 12 px body.
    pub fn row_h(self) -> f32 {
        // Never less than the line box it has to hold. 1.7 of the base is more
        // than 1.45 at every size, so this is a floor that currently never
        // binds — it is here so that changing either number cannot silently
        // make a grid row shorter than its own text.
        (self.base * 1.7).round().max(f32::from(self.line_h()))
    }

    /// A grid column, before anybody drags it. Widths a person has set are
    /// remembered and are NOT rescaled: a column somebody sized is a decision,
    /// and re-deriving it would undo that decision every time the font moved.
    pub fn col_w(self) -> f32 {
        (self.base * 11.0).round()
    }

    /// The leading for a line of panel text.
    ///
    /// A line box the SIZE of the text leaves nothing below the baseline, and
    /// the tails of `p`, `y` and `д` are drawn below the baseline — so they met
    /// the bottom of the row and were clipped. 1.45 is room for the descenders
    /// of the fonts this draws in, and it comes off the text size, so raising
    /// the text raises the line with it instead of leaving the question to come
    /// back.
    pub fn line_h(self) -> Pixels {
        px((self.base * 1.45).round())
    }

    /// The height of a row in a side panel.
    ///
    /// DERIVED, from whichever is taller: the text that goes in it, or the
    /// action that sits at its end. Chosen as a number, it stops following the
    /// scale the moment either grows — which is how descenders came to be
    /// clipped and how the pencil came to have nowhere to breathe.
    pub fn panel_row_h(self) -> f32 {
        let text = f32::from(self.body()) * 1.6;
        (text.max(self.action_box() + 4.0)).round()
    }

    /// The height of one rail button, with room around the icon.
    ///
    /// Taller than the box so six of them read as six BUTTONS. Touching
    /// vertically, they read as one column of marks.
    pub fn rail_h(self) -> f32 {
        (self.icon_box() + self.icon * 0.5).round()
    }

    /// The gap between rail buttons: none.
    ///
    /// They were spaced for a round, to stop six icons reading as one column of
    /// marks. Butted together is what was wanted: ONE unbroken column, which is
    /// what a navigation rail looks like. The separating is done by the filled
    /// background and the accent edge of whichever is chosen, and by the hover
    /// lighting exactly one button — so where one ends and the next begins is
    /// still answerable, without air to answer it.
    pub fn rail_gap(self) -> f32 {
        0.0
    }

    /// The side panel's width. Grows with the type, because a panel that stayed
    /// 280 px while the text in it grew would simply hold fewer words.
    pub fn panel_w(self) -> f32 {
        match self.panel {
            // A width somebody dragged is a DECISION, and re-deriving it from
            // the text size would undo that decision every time the font moved
            // — the same rule the grid's dragged columns already follow.
            Some(w) => w.clamp(self.panel_min(), self.panel_max()),
            None => (self.base * 20.0).round(),
        }
    }

    /// Narrow enough to be out of the way, never narrow enough to be useless:
    /// a panel too thin to show a connection's name is a panel nobody can read.
    pub fn panel_min(self) -> f32 {
        (self.base * 11.0).round()
    }

    /// Wide enough for an audit line, not so wide the result has nowhere left.
    pub fn panel_max(self) -> f32 {
        (self.base * 48.0).round()
    }

    pub fn with_panel(self, w: Option<f32>) -> Metrics {
        Metrics {
            panel: w.map(|w| Self::ok(w, self.panel_min(), self.panel_max(), self.base * 20.0)),
            ..self
        }
    }

    pub fn panel_set(self) -> Option<f32> {
        self.panel
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

    /// The label column in a form, where labels stand to the left of fields.
    ///
    /// One definition rather than a number repeated at every row: the labels
    /// have to line up with each other, and six copies of `120.0` line up only
    /// until somebody changes five of them. Wide enough for a translated label,
    /// which is longer than the English it replaced.
    pub fn label_w(self) -> f32 {
        (self.base * 9.0).round()
    }

    /// A form field, which holds a host name or a path and so wants room.
    pub fn field_w(self) -> f32 {
        (self.base * 16.0).round()
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

    /// The status bar's height: one line of text and the padding a cell uses,
    /// above and below.
    ///
    /// It was `px(22.0)`, which at a base of 20 is shorter than the text it
    /// holds — so the bar clipped its own descenders at exactly the setting
    /// somebody chooses because they could not read it.
    pub fn status_h(self) -> f32 {
        (f32::from(self.line_h()) + self.cell_pad() * 2.0).round()
    }

    /// The tab strip's height. Tall enough for the close action in it, by the
    /// same derivation `panel_row_h` uses — a tab with an action it cannot fit
    /// is a close button drawn over its own border.
    pub fn tab_h(self) -> f32 {
        (f32::from(self.line_h()).max(self.action_box() + 4.0)).round()
    }

    /// The height of one control: a button, a chip, a field.
    ///
    /// ONE number for all three, because a row of them at three heights reads
    /// as three unrelated things. Big enough for the text plus a little air,
    /// derived so it follows the scale.
    pub fn control_h(self) -> f32 {
        (self.base * 2.1).round()
    }

    /// The horizontal scrollbar's thickness. A grip, not a panel.
    pub fn hbar_h(self) -> f32 {
        (self.base * 0.6).round()
    }

    /// The row-number column in the grid.
    ///
    /// Wide enough for five digits, which is more rows than a page holds. It
    /// was `NUM_W = 44.0` and took part in the grid's total width, so at a
    /// large base the numbers overflowed a column the arithmetic still
    /// believed was 44 wide.
    pub fn num_w(self) -> f32 {
        (self.base * 3.2).round()
    }

    /// One level of indent in a tree.
    pub fn indent(self) -> f32 {
        (self.base * 0.85).round()
    }

    /// One level of a tree, in SPACES.
    ///
    /// `indent` is a fixed step that has nothing to say about the setting
    /// people actually set — the one that says the JSON view indents by four.
    /// A tree beside that JSON stepping by a different amount is two answers
    /// to one question, so the tree steps by the same number of spaces, in
    /// the width a space actually has at this size.
    pub fn indent_by(self, spaces: u8) -> f32 {
        (self.base * 0.55 * f32::from(spaces.max(1))).round()
    }

    /// The standard inset from a panel's edge, where `px_2` and `pl(10.0)` were
    /// both used to mean the same thing.
    pub fn gutter(self) -> f32 {
        (self.base * 0.6).round()
    }

    /// The box a tree's open/shut arrow sits in — the same box an inline icon
    /// gets, so the text after it lines up whether there is an arrow or not.
    pub fn marker_w(self) -> f32 {
        (self.base * 1.25).round()
    }

    /// How far a toast sits from the corner.
    pub fn toast_inset(self) -> f32 {
        (self.base * 1.2).round()
    }

    /// How far down the window the command palette hangs.
    pub fn palette_top(self) -> f32 {
        (self.base * 6.0).round()
    }

    /// The palette's height: fourteen lines and its field.
    pub fn palette_h(self) -> f32 {
        (f32::from(self.line_h()) * 14.0 + self.control_h()).round()
    }

    /// The height of one bar in a chart, and of one row of a chart's legend.
    pub fn chart_row_h(self) -> f32 {
        f32::from(self.line_h())
    }

    /// The size a separate window opens at.
    ///
    /// Derived like everything else, because a window sized in absolute pixels
    /// holds half a form at a large text size. `Window::Main` is the first
    /// window and is deliberately generous.
    pub fn window(self, kind: Window) -> (f32, f32) {
        let (w, h) = match kind {
            Window::Main => (92.0, 58.0),
            Window::Settings => (64.0, 44.0),
            Window::Connection => (52.0, 40.0),
            Window::Profile => (44.0, 30.0),
        };
        ((self.base * w).round(), (self.base * h).round())
    }
}

/// The windows this client opens, for [`Metrics::window`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Main,
    Settings,
    Connection,
    Profile,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every chrome height holds what goes in it, at every base.
    ///
    /// These were literals — `px(22.0)` for the status bar, `px(26.0)` for the
    /// tab strip, `44.0` for the row numbers — chosen against a base of 12 and
    /// then left behind when the text could be set to 24. The bar clipped its
    /// own descenders at exactly the setting somebody picks because they could
    /// not read it.
    #[test]
    fn the_chrome_holds_what_goes_in_it() {
        let mut b = MIN_BASE;
        while b <= MAX_BASE {
            let mut i = MIN_ICON;
            while i <= MAX_ICON {
                let m = Metrics::new(b).with_icon(i);
                let line = f32::from(m.line_h());
                assert!(
                    m.status_h() >= f32::from(m.small()) * 1.45,
                    "base {b}: status bar {} is shorter than its text",
                    m.status_h()
                );
                assert!(m.status_h() >= line, "base {b}: status bar is shorter than a line");
                assert!(
                    m.tab_h() > m.action_box(),
                    "base {b} icon {i}: the tab's close action does not fit in {}",
                    m.tab_h()
                );
                assert!(
                    m.control_h() >= f32::from(m.body()) * 1.3,
                    "base {b}: a control is too short for its label"
                );
                // Five digits of row number, at the small step's advance.
                assert!(
                    m.num_w() >= f32::from(m.small()) * 2.5,
                    "base {b}: the number column is too narrow"
                );
                assert!(m.marker_w() > 0.0 && m.indent() > 0.0 && m.gutter() > 0.0, "base {b}");
                assert!(m.hbar_h() >= 3.0, "base {b}: the scrollbar is too thin to grab");
                i += 2.0;
            }
            b += 1.0;
        }
    }

    /// A window opens big enough to hold its own form.
    ///
    /// They were absolute: 720×560 for the connection window, 620×420 for the
    /// tunnel one. At a base of 24 the connection form is taller than 560, so
    /// the window opened with its footer — and its Save — off the bottom.
    #[test]
    fn a_window_grows_with_the_text_in_it() {
        for kind in [Window::Main, Window::Settings, Window::Connection, Window::Profile] {
            let small = Metrics::new(MIN_BASE).window(kind);
            let large = Metrics::new(MAX_BASE).window(kind);
            assert!(large.0 > small.0 && large.1 > small.1, "{kind:?} does not follow the scale");
            assert!(small.0 > 300.0 && small.1 > 200.0, "{kind:?} opens too small to use");
        }
        // The main window is the biggest, and every window is wider than tall:
        // these hold rows of fields and lists beside detail.
        let m = Metrics::new(DEFAULT_BASE);
        assert!(m.window(Window::Main).0 > m.window(Window::Settings).0);
        assert!(m.window(Window::Settings).0 > m.window(Window::Connection).0);
        for kind in [Window::Main, Window::Settings, Window::Connection, Window::Profile] {
            let (w, h) = m.window(kind);
            assert!(w > h, "{kind:?} is taller than it is wide");
        }
    }

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
        // The icon is no longer a step off the text at all: it has its own
        // number, and the default is bigger than the 19 it used to compute to.
        assert_eq!(m.icon(), px(DEFAULT_ICON));
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
    /// The rail is navigation, not content: the two sizes move independently.
    /// Somebody enlarging the text to read a result is not asking for bigger
    /// buttons.
    #[test]
    fn the_icon_size_is_independent_of_the_text_size() {
        let m = Metrics::default();
        let bigger_text = m.with_base(22.0);
        assert_eq!(bigger_text.icon_size(), m.icon_size(), "text must not move the icons");
        assert_ne!(bigger_text.body(), m.body());
        let bigger_icons = m.with_icon(32.0);
        assert_eq!(bigger_icons.base(), m.base(), "icons must not move the text");
        assert!(bigger_icons.rail_w() > m.rail_w(), "a bigger icon widens the rail by itself");
    }

    /// The rail comes OFF the icon, so there is one number and not two to keep
    /// in step — and an icon always has air around it.
    #[test]
    fn the_rail_is_derived_from_the_icon_and_leaves_it_room() {
        let mut i = MIN_ICON;
        while i <= MAX_ICON {
            let m = Metrics::default().with_icon(i);
            assert!(m.icon_box() > i, "icon {i}: the box must be bigger than the glyph");
            assert!(m.rail_w() > m.icon_box(), "icon {i}: air on both sides");
            assert!(m.rail_h() > m.icon_box(), "icon {i}: a button is taller than its icon");
            // Touching on purpose: see `rail_gap`.
            assert_eq!(m.rail_gap(), 0.0, "icon {i}");
            i += 1.0;
        }
    }

    /// The reported complaint: the rail was too thin to read the selected
    /// marker. It is now comfortably wider than it was.
    #[test]
    fn the_rail_is_wider_than_it_was() {
        // It used to be `base * 3.1` — 43 pixels at the default text size.
        assert!(Metrics::default().rail_w() >= 52.0, "{}", Metrics::default().rail_w());
    }

    /// A form's labels line up because they come from ONE number, and the
    /// column is wide enough for a label in either language.
    #[test]
    fn a_form_label_column_fits_its_labels() {
        let mut b = MIN_BASE;
        while b <= MAX_BASE {
            let m = Metrics::new(b);
            // The longest label either catalogue carries, at the size labels
            // are drawn (`small`), at a typical advance for that size.
            let longest = "Удостоверяване".chars().count() as f32;
            let w = longest * f32::from(m.small()) * 0.55;
            assert!(m.label_w() >= w, "base {b}: {} vs {w}", m.label_w());
            assert!(m.field_w() > m.label_w(), "base {b}: a field wants more room than its label");
            // The SOURCE sits on a second line under the label, not appended to
            // it, and it is drawn at `tiny` — so it has its own fit to pass.
            // Appended, `Password · Environment variable` was cut mid-word on
            // the English labels and lost its whole second half on the
            // Bulgarian ones, which is the half that says what to type.
            let aside = "Keyring path".chars().count() as f32;
            let aw = aside * f32::from(m.tiny()) * 0.55;
            assert!(m.label_w() >= aw, "base {b}: the source line does not fit: {} vs {aw}", m.label_w());
            b += 1.0;
        }
    }

    /// A line box leaves room BELOW the baseline, at every size.
    ///
    /// Descenders were being clipped because the line box was the size of the
    /// text. This asserts the leading across the whole range rather than at the
    /// default, since the default was not where it was noticed.
    #[test]
    fn a_line_leaves_room_for_what_hangs_below_it() {
        let mut b = MIN_BASE;
        while b <= MAX_BASE {
            let m = Metrics::new(b);
            let text = f32::from(m.body());
            assert!(
                f32::from(m.line_h()) >= text * 1.4,
                "base {b}: a {}px line box for {text}px text clips the tails",
                f32::from(m.line_h())
            );
            // And the row is at least the line it holds.
            assert!(m.panel_row_h() >= f32::from(m.line_h()), "base {b}");
            b += 1.0;
        }
    }

    /// A panel row is tall enough for its text AND for the action in it, at
    /// every size either can be.
    ///
    /// Descenders were being clipped: the row height was a number, so raising
    /// the text did not raise the row. This asserts the derivation across the
    /// whole range of both numbers, which is the only way the question stops
    /// coming back.
    #[test]
    fn a_panel_row_fits_its_text_and_its_action() {
        let mut b = MIN_BASE;
        while b <= MAX_BASE {
            let mut i = MIN_ICON;
            while i <= MAX_ICON {
                let m = Metrics::new(b).with_icon(i);
                let text = f32::from(m.body());
                // Room for the line AND for what hangs below it.
                assert!(
                    m.panel_row_h() >= text * 1.5,
                    "base {b} icon {i}: {} is not enough for {text}px text",
                    m.panel_row_h()
                );
                // And for the action that sits at the end of the row.
                assert!(
                    m.panel_row_h() > m.action_box(),
                    "base {b} icon {i}: the action does not fit"
                );
                i += 2.0;
            }
            b += 1.0;
        }
    }

    /// A dragged panel width is a decision: kept as given, bounded so it can
    /// never become useless, and NOT re-derived when the text size moves.
    #[test]
    fn a_dragged_panel_width_is_kept_and_bounded() {
        let m = Metrics::default();
        let derived = m.panel_w();
        assert_eq!(m.panel_set(), None, "nothing dragged yet");

        // Dragged: kept exactly.
        let d = m.with_panel(Some(360.0));
        assert_eq!(d.panel_w(), 360.0);
        assert_eq!(d.panel_set(), Some(360.0));

        // And NOT re-derived when the text grows — the same rule the grid's
        // dragged columns follow.
        assert_eq!(d.with_base(22.0).panel_w(), 360.0);
        assert!(m.with_base(22.0).panel_w() > derived, "an underived one still follows");

        // Bounded at both ends: too thin to read a connection's name, or so
        // wide the result has nowhere left, are both refused.
        assert_eq!(m.with_panel(Some(1.0)).panel_w(), m.panel_min());
        assert_eq!(m.with_panel(Some(9_000.0)).panel_w(), m.panel_max());
        assert!(m.panel_min() < derived && derived < m.panel_max());

        // NaN goes to the derived width rather than through.
        assert!(m.with_panel(Some(f32::NAN)).panel_w() > 0.0);

        // Two clicks put it back to derived.
        assert_eq!(d.with_panel(None).panel_w(), derived);
        assert_eq!(d.with_panel(None).panel_set(), None);
    }

    #[test]
    fn a_base_from_outside_is_clamped() {
        assert_eq!(Metrics::new(0.0).base(), MIN_BASE);
        assert_eq!(Metrics::new(-40.0).base(), MIN_BASE);
        assert_eq!(Metrics::new(400.0).base(), MAX_BASE);
        assert_eq!(Metrics::new(f32::NAN).base(), DEFAULT_BASE, "NaN must not survive as a size");
        assert_eq!(Metrics::new(f32::INFINITY).base(), DEFAULT_BASE);
        assert_eq!(Metrics::new(15.0).base(), 15.0);
        // And the icon, which has its own range.
        assert_eq!(Metrics::default().with_icon(0.0).icon_size(), MIN_ICON);
        assert_eq!(Metrics::default().with_icon(900.0).icon_size(), MAX_ICON);
        assert_eq!(Metrics::default().with_icon(f32::NAN).icon_size(), DEFAULT_ICON);
        assert_eq!(Metrics::default().with_icon(26.0).icon_size(), 26.0);
    }
}
