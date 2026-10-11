//! The label an icon does not have.
//!
//! A rail of six glyphs and nothing else is readable only after somebody has
//! pressed all six and remembered what each did. A tip is how that is learned
//! without pressing anything.
//!
//! Three decisions are in here rather than at the call sites, so every tip
//! behaves the same:
//!
//!   * **The words come from the catalogue**, by the same key the panel's own
//!     heading uses. Not a second string written beside the first: two strings
//!     that mean one thing drift, and the one nobody looks at is the one that
//!     stays in English.
//!   * **After a pause.** GPUI waits 500 ms before building the view, which is
//!     what stops a tip appearing every time a pointer crosses the rail on its
//!     way somewhere else.
//!   * **The key is part of it.** A tip is where somebody finds out there is a
//!     shortcut at all, which is not true of any other place in the window.

use gpui::prelude::*;
use gpui::{div, px, AnyView, App, SharedString, Window};

use crate::metrics::Metrics;
use crate::theme::Theme;

/// One floating label.
pub struct Tip {
    text: SharedString,
    /// The keystroke that does the same thing, when there is one.
    key: Option<SharedString>,
    theme: Theme,
    scale: Metrics,
}

impl Tip {
    /// A builder for `.tooltip(…)`, which wants a function because the view is
    /// not made until the pointer has stayed put.
    pub fn build(
        text: impl Into<SharedString>,
        key: Option<&'static str>,
        theme: Theme,
        scale: Metrics,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
        let text = text.into();
        let key: Option<SharedString> = key.map(SharedString::from);
        move |_window, cx| {
            let (text, key, theme) = (text.clone(), key.clone(), theme.clone());
            cx.new(|_| Tip { text, key, theme, scale }).into()
        }
    }
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (t, ms) = (&self.theme, self.scale);
        let mut row = div()
            .flex()
            .items_start()
            .gap_2()
            .px_2()
            .py_1()
            // IT WRAPS, and it is held to a width that makes it wrap.
            //
            // A tip is one line by default, so a tip whose job is to show the
            // whole of a value that was too long to fit was itself too long
            // to fit: it ran off the window and the end — the part somebody
            // hovered to read — was the part cut off. A box forty characters
            // wide across as many lines as it needs shows all of it.
            //
            // In the type scale rather than pixels, so it is forty characters
            // at every size.
            .max_w(px(ms.base() * 40.0))
            // Raised, bordered and square: it has to read as sitting ON the
            // window on palettes whose surfaces are a hair apart, and the
            // corners are square everywhere else.
            .bg(t.raised())
            .border_1()
            .border_color(t.border())
            .text_size(ms.small())
            .text_color(t.fg())
            // The text takes the width and gives way, so the key beside it is
            // never pushed off the edge.
            .child(div().flex_1().min_w_0().child(self.text.clone()));
        if let Some(key) = &self.key {
            row = row.child(
                div()
                    .flex_none()
                    .px_1()
                    .bg(t.status())
                    .text_size(ms.tiny())
                    .text_color(t.muted())
                    .child(key.clone()),
            );
        }
        // A tip is never the thing being pointed at: it must not swallow the
        // click that was on its way to the button underneath.
        div().occlude().child(row).min_w(px(0.0))
    }
}
