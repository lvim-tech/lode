//! Three rules about this client's style, enforced by reading its own source.
//!
//! Both of them have been broken before, in the same way: a number or a colour
//! written at the point of use, correct on the machine it was written on, and
//! then left behind when the thing it was derived from moved. Eighty-seven text
//! sizes were literals once; the status bar was `px(22.0)` and clipped its own
//! descenders at the large text size somebody picks *because* they could not
//! read it.
//!
//! A test rather than a note in a comment, because the note was there.
//!
//! Where the line is:
//!
//!   * A LENGTH is derived from the type scale (`metrics.rs`). The exceptions
//!     are hairlines — a border, a grip, a rule — which are 1, 2 or 3 physical
//!     pixels and do not grow with the text, because a 3 px line at a base of
//!     24 is not a line, it is a bar.
//!   * A COLOUR comes from the theme (`theme.rs`), which reads the editor's own
//!     palette. A literal `rgb(…)` is a colour that does not change when the
//!     theme does, which is the one thing this client must never have.
//!   * A SCROLLING COLUMN is built by `widgets::scroll`. A flex item shrinks
//!     before its container scrolls, so rows added straight to an
//!     `overflow_y_scroll` column get squeezed instead — which is how a
//!     hundred and four history rows came out as a nine-pixel smear with the
//!     lines drawn through each other.
//!
//! `metrics.rs` and `theme.rs` are where the derivation LIVES, so they are not
//! walked.

use std::path::{Path, PathBuf};

/// The files that draw. Everything here is a view, and a view may not invent a
/// length or a colour.
const DRAWING: [&str; 7] = [
    "app.rs",
    "form.rs",
    "settings.rs",
    "profile.rs",
    "tip.rs",
    "board.rs",
    "export.rs",
];

/// A length that is allowed to be written out: a hairline, or nothing at all.
///
/// `0` is "no padding", and the rest are physical lines. Anything else is a
/// measurement, and a measurement belongs in `metrics.rs`.
const HAIRLINES: [f32; 4] = [0.0, 1.0, 2.0, 3.0];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `px(<literal>)` in a file, with its line number — ignoring the ones
/// that are hairlines.
fn stray_lengths(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        // A comment explaining a number is not a number.
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find("px(") {
            let after = &rest[at + 3..];
            let arg: String = after.chars().take_while(|c| *c != ')').collect();
            let whole = format!("px({arg})");
            rest = &after[arg.len().min(after.len())..];
            let trimmed = arg.trim();
            // `px(something)` — a call, a field, a variable: fine, it came from
            // somewhere that can be changed in one place.
            if !trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                continue;
            }
            // A bare number, read as one. A compound expression that merely
            // STARTS with a digit (`16.0 + depth * 14.0`) is not a hairline
            // however small its first term is.
            if let Ok(v) = trimmed.parse::<f32>() {
                if HAIRLINES.contains(&v) {
                    continue;
                }
            }
            out.push((n + 1, whole));
        }
    }
    out
}

fn stray_colours(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for pat in ["rgb(", "rgba(", "hsla(", "hsl("] {
            if let Some(at) = line.find(pat) {
                // `gpui::rgb` in a theme fallback is the theme's business, and
                // the theme is not walked — so anything found here is a view.
                out.push((n + 1, line[at..].chars().take(32).collect()));
            }
        }
    }
    out
}

/// A length written at the point of use is a length that stops following the
/// type scale.
#[test]
fn no_view_invents_a_length() {
    let mut bad = Vec::new();
    for name in DRAWING {
        let path = src_dir().join(name);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for (line, what) in stray_lengths(&text) {
            bad.push(format!("{name}:{line}  {what}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} length(s) written out instead of derived from `metrics.rs`.\n\
         Hairlines (0, 1, 2, 3) are allowed — a border does not grow with the text.\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// A colour written at the point of use is a colour the theme cannot reach.
#[test]
fn no_view_invents_a_colour() {
    let mut bad = Vec::new();
    for name in DRAWING {
        let path = src_dir().join(name);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for (line, what) in stray_colours(&text) {
            bad.push(format!("{name}:{line}  {what}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} colour(s) written out instead of taken from `theme.rs`:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// The walker itself, against text whose answer is known — otherwise a test
/// that passes because it found nothing looks the same as one that works.
#[test]
fn the_walker_can_tell_a_measurement_from_a_hairline() {
    let sample = "\
        let a = px(22.0);\n\
        let b = px(1.0);\n\
        let c = px(ms.status_h());\n\
        // px(99.0) in a comment is prose\n\
        let d = px(0.);\n\
        let e = px(3.0);\n\
        let f = px(10.0);\n\
        let g = rgb(0x112233);\n";
    let lens = stray_lengths(sample);
    assert_eq!(
        lens.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>(),
        vec!["px(22.0)", "px(10.0)"],
        "{lens:?}"
    );
    assert_eq!(stray_colours(sample).len(), 1);
}

/// A separate window must not READ the workspace while it draws.
///
/// `App::open_window` draws the new window once before it returns, and the three
/// windows are opened from listeners — so that first draw happens while the
/// workspace is still inside its own update, and reading an entity that is being
/// updated is a panic in gpui. The symptom was not subtle and was invisible
/// here: pressing the gear closed the whole application.
///
/// Updating it is fine, and is how every one of these windows acts: a listener
/// runs after the draw, not during it. What is forbidden is a READ — `upgrade()`
/// followed by `read`.
#[test]
fn a_window_does_not_read_the_workspace() {
    const WINDOWS: [&str; 3] = ["settings.rs", "form.rs", "profile.rs"];
    let mut bad = Vec::new();
    for name in WINDOWS {
        let Ok(text) = std::fs::read_to_string(src_dir().join(name)) else { continue };
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            // `self.workspace.upgrade()` is only ever wanted so something can be
            // read off it; the update path does not need it.
            if line.contains("workspace.upgrade()") {
                bad.push(format!("{name}:{}  {}", n + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} place(s) reading the workspace from a window. `open_window` draws \
         before it returns, so a read during that draw is a panic — hand the \
         state over and push it again when it changes (see settings::Appearance):\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// A column that scrolls must be built by `widgets::scroll`.
///
/// **A flex item shrinks before its container scrolls.** Five columns in this
/// client were `flex_1 min_h_0 overflow_y_scroll` with their rows added
/// straight in, which is a flex container full of flex items: taffy did what
/// flexbox says and squeezed every row rather than letting the box scroll. It
/// showed first in the history panel, where a hundred and four rows came out
/// at a nine-pixel pitch with the lines drawn through each other; the
/// Keybindings page's thirty-one rows were next.
///
/// `widgets::scroll` puts the rows in an inner column that sizes to its
/// content, so nothing in it is ever asked to shrink. This test is what stops
/// the next scrolling column being written the old way — the comment that said
/// so was already there, on the container, about a different rule.
#[test]
fn a_scrolling_column_comes_from_the_helper() {
    let mut stray: Vec<String> = Vec::new();
    for f in DRAWING {
        let path = src_dir().join(f);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || !t.contains("overflow_y_scroll()") {
                continue;
            }
            // Is it a flex COLUMN? That is the shape with the fault: rows
            // stacked along the axis the container is constrained on.
            //
            // `overflow_scroll()` — both axes — is left alone deliberately.
            // In this client it is never a list of rows: it is the schema's
            // CREATE statement on one nowrap line and the diagram's canvas,
            // each a single self-sized child that has to be reachable in x as
            // well, which the helper (y only, x hidden) would take away.
            //
            // The chain is read backwards to the statement it belongs to:
            // `.flex_col()` and `.overflow_y_scroll()` are rarely on one line.
            let mut chain = String::new();
            for back in (0..=n).rev() {
                let l = lines[back].trim();
                chain.push_str(l);
                // The end of the PREVIOUS statement: stop before it.
                if back < n && (l.ends_with(';') || l.ends_with('{')) {
                    break;
                }
                if n - back > 20 {
                    break;
                }
            }
            if chain.contains(".flex_col()") {
                stray.push(format!("{f}:{}  {}", n + 1, t.chars().take(60).collect::<String>()));
            }
        }
    }
    assert!(
        stray.is_empty(),
        "{} scrolling column(s) built by hand instead of with `widgets::scroll`.\n\
         A flex item shrinks before its container scrolls, so the rows will be squeezed \
         rather than scrolled:\n  {}",
        stray.len(),
        stray.join("\n  ")
    );
}
