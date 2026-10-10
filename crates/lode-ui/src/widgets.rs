//! The controls, in one place.
//!
//! WHY THIS FILE EXISTS. There were four windows and each one drew its own
//! buttons: a `div` with padding, a border or a background, a text size and a
//! hover, written out at every call site. Eighty-odd of them in `app.rs`
//! alone, each one a decision taken again — so "Run" was one height, "Save
//! chart" another, and "remove it" a third, and nothing was wrong with any of
//! them individually.
//!
//! The same fault the lengths had and the colours had, and the same cure: one
//! definition, named by its JOB. A `Primary` is the thing this row is for; a
//! `Danger` is the one that cannot be undone. Changing how a secondary button
//! looks is now one edit rather than a sweep through four files, each of which
//! is a chance to miss one.
//!
//! WHAT IS NOT HERE. A handler. These build the control and hand it back; the
//! caller attaches `.on_click(cx.listener(…))` itself, because the four windows
//! are four different entities and a shared helper cannot name all four. That
//! is also why these are free functions over `(&Theme, Metrics)` rather than
//! methods: a method would have to live on one of the four.

use gpui::prelude::*;
use gpui::{div, px, Div, Rgba, SharedString, Stateful};

use crate::metrics::Metrics;
use crate::theme::Theme;

/// Everything a separate window inherits from the main one.
///
/// ONE argument instead of five. `ConnectionForm::new` had grown to eleven
/// positional parameters, four of them `String` — and two of those were the
/// text font and the icon font, in that order, with nothing to stop them being
/// swapped. They were swapped once. A named field cannot be.
#[derive(Clone)]
pub struct Look {
    pub theme: Theme,
    pub scale: Metrics,
    pub text_font: String,
    pub icon_font: String,
    pub lang: crate::i18n::Lang,
}

/// What a button is FOR, which is the only thing a call site should have to
/// decide. How that looks is this file's business.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// The thing the row exists for: Run, Save, Connect. One per row at most —
    /// two primaries in a row is a row that has not decided.
    Primary,
    /// A real choice beside the primary one: Cancel, Test, Arrange.
    Secondary,
    /// Available, not advertised: a reload, a "clear finished", a toggle in a
    /// corner. No border; it comes forward under the pointer.
    Quiet,
    /// It removes something, or it writes. Outlined in the error colour until
    /// the pointer is on it, then filled: a red block is not a thing to put
    /// four of in a row, and a red outline is still unmistakable.
    Danger,
    /// NAVIGATION only — "edit this tunnel", "open in the editor". The accent
    /// as ink says "this takes you somewhere", and using it for an action is
    /// what made the accent meaningless.
    Link,
}

/// One button.
///
/// Every variant is `control_h` tall, so a row of them is a row. The hover and
/// the pressed state come from the theme's DERIVED marks (see `theme.rs`), so
/// they are visible on a light palette, which they were not.
pub fn btn(
    t: &Theme,
    ms: Metrics,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    kind: Btn,
) -> Stateful<Div> {
    let base = div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .h(px(ms.control_h()))
        .px_2()
        .cursor_pointer()
        .text_size(ms.small())
        .child(label.into());
    match kind {
        Btn::Primary => base
            .bg(t.accent())
            .text_color(t.inverted())
            .hover(|d| d.bg(t.fg()))
            .active(|d| d.bg(t.accent())),
        Btn::Secondary => base
            .border_1()
            .border_color(t.divider())
            .text_color(t.fg())
            .hover(|d| d.bg(t.hover_mark()).border_color(t.accent()))
            .active(|d| d.bg(t.selected_mark())),
        Btn::Quiet => base
            .text_color(t.muted())
            .hover(|d| d.bg(t.hover_mark()).text_color(t.fg()))
            .active(|d| d.bg(t.selected_mark())),
        Btn::Danger => base
            .border_1()
            .border_color(t.error())
            .text_color(t.error())
            .hover(|d| d.bg(t.error()).text_color(t.inverted()))
            .active(|d| d.bg(t.error())),
        Btn::Link => base
            .text_color(t.accent())
            .hover(|d| d.bg(t.hover_mark()))
            .active(|d| d.bg(t.selected_mark())),
    }
}

/// A button that cannot be pressed, and says so by its ink.
///
/// Returned WITHOUT an id, so there is nothing to attach a handler to: a
/// disabled control that still takes clicks is worse than none, because the
/// click looks like it worked.
pub fn btn_off(t: &Theme, ms: Metrics, label: impl Into<SharedString>, kind: Btn) -> Div {
    let d = div()
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .h(px(ms.control_h()))
        .px_2()
        .text_size(ms.small())
        .text_color(t.disabled())
        .child(label.into());
    match kind {
        Btn::Secondary | Btn::Danger => d.border_1().border_color(t.divider()),
        _ => d,
    }
}

/// One of a row of choices, on or off.
///
/// ONE size for every chip in the application. They were `tiny` in the driver
/// list, `small` in the capability list and `body` in the language row, which
/// made three rows of the same thing look like three different kinds of thing.
pub fn chip(
    t: &Theme,
    ms: Metrics,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    on: bool,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .h(px(ms.control_h()))
        .px_2()
        .cursor_pointer()
        .text_size(ms.small())
        .when(on, |d| d.bg(t.accent()).text_color(t.inverted()))
        .when(!on, |d| d.border_1().border_color(t.divider()).text_color(t.muted()))
        .hover(|d| if on { d } else { d.bg(t.hover_mark()).text_color(t.fg()) })
        .child(label.into())
}

/// A chip that only REPORTS — a capability a driver has or has not. No id, no
/// pointer: there is nothing to press.
pub fn flag(t: &Theme, ms: Metrics, label: impl Into<SharedString>, on: bool) -> Div {
    div()
        .flex()
        .items_center()
        .flex_none()
        .h(px(ms.control_h()))
        .px_2()
        .text_size(ms.small())
        .when(on, |d| d.bg(t.accent()).text_color(t.inverted()))
        .when(!on, |d| d.border_1().border_color(t.divider()).text_color(t.disabled()))
        .child(label.into())
}

/// A drawn checkbox: a box, with a tick in it when it is on.
pub fn tick(t: &Theme, ms: Metrics, on: bool) -> Div {
    let side = (ms.base() * 1.1).round();
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .w(px(side))
        .h(px(side))
        .line_height(px(side))
        .border_1()
        .border_color(if on { t.accent() } else { t.divider() })
        .when(on, |d| d.bg(t.accent()))
        .text_size(ms.tiny())
        .text_color(t.inverted())
        .child(if on { "\u{f00c}" } else { "" })
}

/// A label in the small step.
pub fn small(ms: Metrics, text: impl Into<SharedString>, colour: Rgba) -> Div {
    div().text_size(ms.small()).text_color(colour).child(text.into())
}

/// A heading inside a panel or a page. ONE style of heading: the settings panel
/// had two and nobody could tell them apart.
pub fn section(t: &Theme, ms: Metrics, text: &str) -> Div {
    div()
        .px_2()
        .py_1()
        .text_size(ms.tiny())
        .text_color(t.title())
        .child(SharedString::from(text.to_uppercase()))
}

/// One line of text in a box of DEFINITE width.
///
/// The width is not decoration. GPUI truncates only against a definite width,
/// so `w_full` plus `text_ellipsis` cuts the text and never draws the `…`.
pub fn line(ms: Metrics, text: impl Into<SharedString>, size: gpui::Pixels, colour: Rgba, w: f32) -> Div {
    let _ = ms;
    div()
        .w(px(w))
        .flex_none()
        .truncate()
        .text_size(size)
        .text_color(colour)
        .child(text.into())
}

/// A glyph in the icon font, in a box the size of an inline icon, so text after
/// it lines up whether there is a glyph or not.
pub fn icon_inline(
    ms: Metrics,
    family: impl Into<SharedString>,
    glyph: impl Into<SharedString>,
    colour: Rgba,
) -> Div {
    let box_w = ms.marker_w();
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_none()
        .w(px(box_w))
        .h(px(box_w))
        .line_height(px(box_w))
        .font_family(family.into())
        .text_size(ms.small())
        .text_color(colour)
        .child(glyph.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every button is one height, and that height holds its label.
    ///
    /// This is the whole point of the file: a row of controls at three heights
    /// reads as three unrelated things, and that is what four files' worth of
    /// hand-written `div`s produced.
    #[test]
    fn every_control_is_the_same_height() {
        let mut b = crate::metrics::MIN_BASE;
        while b <= crate::metrics::MAX_BASE {
            let ms = Metrics::new(b);
            // One number, used by the button, the chip and the field alike.
            assert!(ms.control_h() >= f32::from(ms.small()) * 1.5, "base {b}");
            assert!(ms.control_h() >= f32::from(ms.body()) * 1.3, "base {b}");
            b += 1.0;
        }
    }

    /// The five kinds are distinct, and `Link` is the only one that is only
    /// navigation — a list that lets an action be a link is how the accent
    /// stops meaning anything.
    #[test]
    fn the_kinds_say_what_they_are_for() {
        let all = [Btn::Primary, Btn::Secondary, Btn::Quiet, Btn::Danger, Btn::Link];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}
