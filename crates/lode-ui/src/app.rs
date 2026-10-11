//! The workspace view.
//!
//! Layout, left to right: an icon rail, a panel (connections and schema), and
//! the work area. Above the work area a row of result tabs, below it the
//! background-task list and a status strip. That arrangement is dbflux's and is
//! deliberate: the rail changes what the PANEL shows, not what is on the right.
//!
//! The thing this client exists to do, and the previous one did not: **clicking
//! a collection or a table opens a tab and shows its rows.** No statement is
//! typed, and none is invented here — the statement comes from the driver's own
//! `browse` template, which the engine sends in `rpc.hello`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use std::time::Instant;

use gpui::{
    div, prelude::*, px, size, uniform_list, Bounds, ClipboardItem, Context, Entity, FocusHandle,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Rgba, SharedString,
    TitlebarOptions, WeakEntity,
    Window, WindowBounds, WindowHandle, WindowOptions,
};
use lode_proto::{
    apply_time, cell_text, AuditRow, Dashboard, DriverMeta, Engine, Hello, HistoryRow, Index,
    Message, Node, QueryState, SavedChart, SavedConnection, SavedQuery, TableColumn, Tile, TimeFit,
    TileKind,
    TimeRange,
};
use serde_json::{json, Value};

use crate::export::{self, Format};
use crate::form::ConnectionForm;
use crate::board::{self, Board, DragMode, Pane};
use crate::i18n::Lang;
use crate::input::{InputColors, InputEvent, TextInput};
use crate::metrics::Metrics;
use crate::session::{self, Session, SessionTab};
use crate::theme::{self, Theme};
use crate::widgets::{btn, chip, Btn};
use crate::workspace::{
    bar_label, bar_label_of, clamped_scroll, column_editable, copy_as_insert, copy_as_select, fuzzy_score, row_key,
    value_range, ChartShape, Cond,
    PendingWrite, Toast,
    fmt_duration, needs_confirm_from_error, pick_target, preview, respawn_delay, type_badge, Live,
    ResultView, RunState, SubTab, Tab, Task,
};

gpui::actions!(
    lode,
    [
        /// Down a row, up a row — j and k, and the arrows, because a grid that
        /// only answers to vim keys is a grid half the people cannot drive.
        NextRow,
        PrevRow,
        /// Left and right move between TABS: h and l are the only pair spare,
        /// and moving between results is what this window is for.
        PrevTab,
        NextTab,
        /// The grid sideways. Shift-H and Shift-L, because plain h and l are
        /// already the tabs and taking them would be swapping one unreachable
        /// thing for another.
        ScrollLeft,
        ScrollRight,
        /// Page down and up, ctrl-f and ctrl-b.
        PageDown,
        PageUp,
        /// First and last loaded row.
        FirstRow,
        LastRow,
        /// Open the row under the cursor in the detail panel, or shut it.
        ToggleDetail,
        /// The palette, and the way out of anything.
        TogglePalette,
        Escape,
        /// Run the active tab.
        RunActive,
        /// Move the palette's highlight without leaving the text field.
        PaletteDown,
        PaletteUp,
        PaletteAccept,
        /// A tab of one's own, on the connection in front: ctrl-t.
        NewTab,
        /// Close the tab in front: ctrl-w.
        CloseTab,
        /// The tab you were last in — see `mru_next`. Ctrl-tab, and back again.
        LastTab,
        /// The panel, out of the way and back: ctrl-shift-b (plain ctrl-b is
        /// already a page of rows).
        TogglePanel,
        /// The settings window: ctrl-comma, as everywhere else.
        OpenSettings,
        /// A new connection: ctrl-shift-n.
        NewConnection,
        /// The export row: ctrl-e.
        ToggleExport,
        /// A page of rows forward and back, `[` and `]`.
        PrevPage,
        NextPage,
        /// Straight to a panel: ctrl-shift-a audit, ctrl-shift-t tasks.
        ShowAudit,
        ShowTasks,
        /// Run the active tab again: f5.
        Refresh,
        /// The console's own history, up and down. Its own actions, in its own
        /// key context: `up` in the console is not `up` in the grid.
        ConsolePrev,
        ConsoleNext,
        /// The cell cursor sideways, and what to do with the cell it is on.
        NextCol,
        PrevCol,
        CopyCell,
        CopyRowTsv,
        CopyPageTsv,
        /// Pick a tab by position: ctrl-1 … ctrl-9. Nine, because the tenth
        /// would be ctrl-0 and nobody counts the tenth tab.
        GoTab1,
        GoTab2,
        GoTab3,
        GoTab4,
        GoTab5,
        GoTab6,
        GoTab7,
        GoTab8,
        GoTab9,
    ]
);

/// Bind the window's keys.
///
/// Every one of these is barred inside a text field (`!TextInput`), because a
/// `j` typed into a filter box has to be a `j`. The field's own bindings — the
/// arrows, home, end — stay as they are; these are what is left over when
/// nothing is being typed into.
/// Every key somebody may CHANGE: what it does, its default, and where it
/// works.
///
/// One table, and two things come out of it — the bindings themselves and the
/// Keybindings page. Written by hand before, in two places, with a test to
/// stop them disagreeing; one table cannot disagree with itself.
///
/// The ACTION NAME is the handle an override is stored under, because it is the
/// only stable one: an index into this table would move the first time a row
/// was added in the middle, and a keystroke cannot be its own key when the
/// keystroke is the thing being changed.
macro_rules! keymap {
    ( $( $group:literal, $label:literal, $name:literal, $action:ident, $key:literal, $ctx:expr );* $(;)? ) => {
        /// The table, for the page that shows it.
        pub const KEYMAP: &[(&str, &str, &str, &str)] =
            &[ $( ($group, $label, $name, $key) ),* ];

        /// Bind them, letting an override replace a default.
        fn bind_changeable(
            cx: &mut gpui::App,
            over: &std::collections::BTreeMap<String, String>,
        ) {
            use gpui::KeyBinding;
            let mut out = Vec::new();
            $(
                let want = over.get($name).map(String::as_str).unwrap_or($key);
                // VALIDATED first. `KeyBinding::new` panics on a keystroke it
                // cannot parse, so a session file with one bad line would take
                // the window down on startup — and the window would never get
                // far enough to let anybody fix it.
                let key = if valid_keystroke(want) {
                    want
                } else {
                    eprintln!("lode: {:?} is not a keystroke; {} keeps {:?}", want, $name, $key);
                    $key
                };
                out.push(KeyBinding::new(key, $action, $ctx));
            )*
            cx.bind_keys(out);
        }
    };
}

/// Does gpui's parser accept this keystroke?
///
/// Asked BEFORE binding, because binding an invalid one is a panic. A sequence
/// (`g g`) is several keystrokes, so each part is checked.
pub fn valid_keystroke(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.split_whitespace().all(|part| gpui::Keystroke::parse(part).is_ok())
}

// Not in a field, and not in the EDITOR: `j`, `k`, `r` and `g` are letters
// somebody types into a statement, and a grid binding that reaches the editor
// makes it impossible to write one.
const OUTSIDE: Option<&str> = Some("Workspace && !TextInput && !Editor");
// These hold a modifier, so they are safe INSIDE a field as well — and have to
// be: ctrl-w while the cursor is in a statement still means close the tab.
const ANYWHERE: Option<&str> = None;

keymap! {
    "THE RESULT", "A row down", "lode::NextRow", NextRow, "j", OUTSIDE;
    "THE RESULT", "A row up", "lode::PrevRow", PrevRow, "k", OUTSIDE;
    "THE RESULT", "First row", "lode::FirstRow", FirstRow, "g", OUTSIDE;
    "THE RESULT", "Last row", "lode::LastRow", LastRow, "shift-g", OUTSIDE;
    "THE RESULT", "A page down", "lode::PageDown", PageDown, "ctrl-f", OUTSIDE;
    "THE RESULT", "A page up", "lode::PageUp", PageUp, "ctrl-b", OUTSIDE;
    "THE RESULT", "The grid left", "lode::ScrollLeft", ScrollLeft, "shift-h", OUTSIDE;
    "THE RESULT", "The grid right", "lode::ScrollRight", ScrollRight, "shift-l", OUTSIDE;
    "THE RESULT", "Show the row", "lode::ToggleDetail", ToggleDetail, "enter", OUTSIDE;
    "THE RESULT", "Run this tab", "lode::RunActive", RunActive, "r", OUTSIDE;
    "THE RESULT", "Run it again", "lode::Refresh", Refresh, "f5", ANYWHERE;
    "THE RESULT", "A cell left", "lode::PrevCol", PrevCol, ",", OUTSIDE;
    "THE RESULT", "A cell right", "lode::NextCol", NextCol, ".", OUTSIDE;
    "THE RESULT", "Copy the cell", "lode::CopyCell", CopyCell, "ctrl-c", OUTSIDE;
    "THE RESULT", "Copy the row", "lode::CopyRowTsv", CopyRowTsv, "ctrl-shift-c", OUTSIDE;
    "THE RESULT", "Copy the loaded rows", "lode::CopyPageTsv", CopyPageTsv, "y", OUTSIDE;
    "THE RESULT", "The next page of rows", "lode::NextPage", NextPage, "]", OUTSIDE;
    "THE RESULT", "The previous page", "lode::PrevPage", PrevPage, "[", OUTSIDE;
    "THE RESULT", "Export", "lode::ToggleExport", ToggleExport, "ctrl-e", ANYWHERE;
    "TABS", "The tab left", "lode::PrevTab", PrevTab, "h", OUTSIDE;
    "TABS", "The tab right", "lode::NextTab", NextTab, "l", OUTSIDE;
    "TABS", "A tab of its own", "lode::NewTab", NewTab, "ctrl-t", ANYWHERE;
    "TABS", "Close it", "lode::CloseTab", CloseTab, "ctrl-w", ANYWHERE;
    "TABS", "The tab before this", "lode::LastTab", LastTab, "ctrl-tab", ANYWHERE;
    "THE WINDOW", "Everything, by name", "lode::TogglePalette", TogglePalette, "ctrl-p", ANYWHERE;
    "THE WINDOW", "Leave what is in front", "lode::Escape", Escape, "escape", ANYWHERE;
    "THE WINDOW", "Settings", "lode::OpenSettings", OpenSettings, "ctrl-,", ANYWHERE;
    "THE WINDOW", "Hide the panel", "lode::TogglePanel", TogglePanel, "ctrl-shift-b", ANYWHERE;
    "THE WINDOW", "New connection", "lode::NewConnection", NewConnection, "ctrl-shift-n", ANYWHERE;
    "THE WINDOW", "Audit", "lode::ShowAudit", ShowAudit, "ctrl-shift-a", ANYWHERE;
    "THE WINDOW", "Tasks", "lode::ShowTasks", ShowTasks, "ctrl-shift-t", ANYWHERE;
}

/// Bind the window's keys.
///
/// Every one of the changeable ones is in `keymap!` above. What is left here is
/// what must NOT be changed: the aliases (the arrows beside `j` and `k`), the
/// keys that only exist inside the palette or the console, and the nine tab
/// positions — nine rows in a table of changeable keys would be nine rows
/// nobody reads to learn one thing.
pub fn bind_workspace_keys(cx: &mut gpui::App) {
    use gpui::KeyBinding;
    // What somebody has changed, read from the session. The same file the
    // window reads a moment later; this is cheap and happens once.
    let over = session::load().keys.unwrap_or_default();
    bind_changeable(cx, &over);
    cx.bind_keys([
        // ALIASES. The arrows do what the letters do, always: a window where
        // the arrow keys do nothing is a window half the people cannot drive,
        // and that is not something to leave to a setting.
        KeyBinding::new("down", NextRow, OUTSIDE),
        KeyBinding::new("up", PrevRow, OUTSIDE),
        // Inside the palette only, where they cannot clash with anything.
        KeyBinding::new("ctrl-n", PaletteDown, Some("Palette")),
        KeyBinding::new("ctrl-j", PaletteDown, Some("Palette")),
        KeyBinding::new("ctrl-k", PaletteUp, Some("Palette")),
        KeyBinding::new("ctrl-y", PaletteAccept, Some("Palette")),
        // And inside the console only.
        KeyBinding::new("up", ConsolePrev, Some("Console")),
        KeyBinding::new("down", ConsoleNext, Some("Console")),
        KeyBinding::new("ctrl-1", GoTab1, ANYWHERE),
        KeyBinding::new("ctrl-2", GoTab2, ANYWHERE),
        KeyBinding::new("ctrl-3", GoTab3, ANYWHERE),
        KeyBinding::new("ctrl-4", GoTab4, ANYWHERE),
        KeyBinding::new("ctrl-5", GoTab5, ANYWHERE),
        KeyBinding::new("ctrl-6", GoTab6, ANYWHERE),
        KeyBinding::new("ctrl-7", GoTab7, ANYWHERE),
        KeyBinding::new("ctrl-8", GoTab8, ANYWHERE),
        KeyBinding::new("ctrl-9", GoTab9, ANYWHERE),
    ]);
}

/// The font to draw in: the first of a short list the text system reports as
/// installed.
///
/// Asked rather than assumed. The literal this replaces was a font that exists
/// on one machine, and on any other the window fell back silently — which looks
/// like a window whose icons are empty boxes for no visible reason. `$LODE_FONT`
/// wins outright, because somebody who sets it has already decided.
fn pick_font(cx: &gpui::App) -> String {
    let installed = cx.text_system().all_font_names();
    let has = |n: &str| installed.iter().any(|f| f == n);
    if let Ok(want) = std::env::var("LODE_FONT") {
        if has(&want) {
            return want;
        }
    }
    // Nerd Fonts first: the interface uses glyphs from the Font Awesome block,
    // and a font without them draws a row of empty boxes.
    for name in [
        "CodeNerdFont",
        "CodeNerdFontMono",
        "Inconsolata LGC Nerd Font Mono",
        "Inconsolata LGC Nerd Font",
        "FiraCode Nerd Font",
        "JetBrainsMono Nerd Font",
        "Hack Nerd Font",
        "DejaVuSansM Nerd Font",
        "Symbols Nerd Font",
        "FiraCode Nerd Font Mono",
    ] {
        if has(name) {
            return name.to_string();
        }
    }
    // Anything with "Nerd" in it, then any monospace-looking name, then
    // whatever the system hands back for an unknown family.
    if let Some(any_nerd) = installed.iter().find(|f| f.contains("Nerd")) {
        return any_nerd.clone();
    }
    for name in ["DejaVu Sans Mono", "Liberation Mono", "monospace"] {
        if has(name) {
            return name.to_string();
        }
    }
    "monospace".to_string()
}

/// Every glyph the interface draws, so each can be checked against the font
/// that was actually chosen.
///
/// Listed rather than discovered, because the check has to happen once at
/// startup: asking the text system per frame would be a font lookup per icon
/// per paint.
/// How many log lines are kept. Enough to hold a whole session's worth of
/// warnings, bounded because a client that runs for a week and talks to a noisy
/// engine would otherwise keep every line of it.
const LOG_KEEP: usize = 500;

/// The size the font is PROBED at, which is not a layout length.
///
/// "Does this family have this glyph" and "how wide is it relative to its size"
/// are both answered at whatever size the question is asked; it is written once
/// here so it does not read as a measurement that forgot to follow the scale.
const GLYPH_PROBE: f32 = 20.0;

const GLYPHS: &[(&str, &str)] = &[
    // The rail.
    ("\u{f1c0}", "DB"),
    ("\u{f080}", "||"),
    ("\u{f1da}", "H"),
    ("\u{f132}", "A"),
    ("\u{f0ae}", "T"),
    ("\u{f013}", "S"),
    // Everything else.
    ("\u{f021}", "~"),
    ("\u{f00c}", "+"),
    ("\u{f00d}", "x"),
    ("\u{f067}", "+"),
    ("\u{f10c}", "o"),
    ("\u{f111}", "*"),
    // A collection in the schema tree.
    ("\u{f0c5}", "C"),
    ("\u{f044}", "e"),
    ("\u{f00a}", "#"),
    ("\u{f15c}", "Q"),
    ("\u{f07b}", "/"),
    ("\u{f0ce}", "T"),
    ("\u{f06e}", "V"),
    ("\u{f070}", "-"),
    ("\u{f0d7}", "v"),
    ("\u{f0da}", ">"),
    ("\u{f0de}", "^"),
    ("\u{f0dd}", "v"),
    // The padlock beside an open connection — see `link_label`.
    (crate::workspace::LINK_LOCKED, "L"),
    (crate::workspace::LINK_OPEN, "U"),
    // The mark a cancelled tab carries — see `workspace::tab_mark`.
    ("\u{f05e}", "/"),
];

/// The advance of one character per pixel of text size, in this font.
///
/// `0` rather than `m` or `x`: a digit is the same width as the rest in a
/// monospaced face and close to the average in a proportional one, and the
/// strings being measured here are full of them.
fn char_ratio(cx: &gpui::App, family: &str) -> f32 {
    let font = gpui::Font {
        family: family.to_string().into(),
        features: Default::default(),
        fallbacks: None,
        weight: Default::default(),
        style: Default::default(),
    };
    let id = cx.text_system().resolve_font(&font);
    const REF: f32 = 16.0;
    cx.text_system()
        .advance(id, px(REF), '0')
        .map(|a| f32::from(a.width) / REF)
        // A font that will not measure gets a ratio that errs WIDE, so the
        // comparison errs toward no tooltip rather than toward one on
        // every row.
        .unwrap_or(0.62)
}

/// `AnyElement` from a plain div, so a branch that adds an id and a branch that
/// does not can both be the value of one expression.
fn return_plain(d: gpui::Div) -> gpui::AnyElement {
    d.into_any_element()
}

/// How many documents the tree reads to say what a field holds.
///
/// Small on purpose. A document store declares no column types, so the only
/// source is the documents themselves — and twenty is enough to tell a date from
/// a string while staying a single cheap request. The number is SHOWN beside
/// the count, because a type derived from a sample and a type the server
/// declared are different claims and should not look alike.
const NODE_SAMPLE: usize = 20;

/// Does this family have every glyph the interface draws?
///
/// Measured, not assumed from the name. `typographic_bounds` errors when the
/// glyph is absent, which is exactly the question — and the names are no guide:
/// on this machine not ONE of the six families the picker used to ask for was
/// installed under that name, so it had been falling through to whatever was
/// left, and six of the rail's glyphs were drawing as empty rectangles.
fn covers_all_glyphs(cx: &gpui::App, family: &str) -> bool {
    missing_in(cx, family).is_empty()
}

/// The font the ICONS are drawn in, which is not the font the text is drawn in.
///
/// Two separate jobs. A text font is chosen for reading a result; an icon font
/// is chosen for having the glyphs and drawing them at one width. Nerd Fonts
/// ship a `Mono` variant whose symbols are all one cell wide, and `Symbols Nerd
/// Font Mono` is nothing but those symbols — which is precisely this job.
///
/// Measured against the installed fonts rather than hardcoded, because the
/// hardcoded list is what failed.
fn pick_icon_font(cx: &gpui::App, text_font: &str) -> String {
    if let Ok(want) = std::env::var("LODE_ICON_FONT") {
        if covers_all_glyphs(cx, &want) {
            return want;
        }
    }
    let installed = cx.text_system().all_font_names();
    // Purpose-built symbol fonts first, then any monospaced Nerd variant, then
    // any Nerd font at all, then the text font. First one that COVERS wins.
    let named = |n: &str| installed.iter().any(|f| f == n);
    for name in ["Symbols Nerd Font Mono", "Symbols Nerd Font"] {
        if named(name) && covers_all_glyphs(cx, name) {
            return name.to_string();
        }
    }
    for want_mono in [true, false] {
        for f in installed.iter() {
            if !f.contains("Nerd") {
                continue;
            }
            if want_mono && !f.contains("Mono") {
                continue;
            }
            if covers_all_glyphs(cx, f) {
                return f.clone();
            }
        }
    }
    text_font.to_string()
}

/// Which of those glyphs the given font does NOT have.
///
/// A font without a glyph draws an empty rectangle, and an empty rectangle is
/// read as "that icon is a different size" — which is what was reported. Asking
/// the font is cheap and is the difference between a letter somebody can read
/// and a box that tells them nothing.
///
/// `typographic_bounds` errors when the glyph is absent, which is exactly the
/// question.
fn missing_in(cx: &gpui::App, family: &str) -> std::collections::HashSet<char> {
    let font = gpui::Font {
        family: family.to_string().into(),
        features: Default::default(),
        fallbacks: None,
        weight: Default::default(),
        style: Default::default(),
    };
    let id = cx.text_system().resolve_font(&font);
    let mut missing = std::collections::HashSet::new();
    for (g, _) in GLYPHS {
        if let Some(ch) = g.chars().next() {
            if cx.text_system().typographic_bounds(id, px(GLYPH_PROBE), ch).is_err() {
                missing.insert(ch);
            }
        }
    }
    missing
}

/// Hand a refused save back to the open connection window.
///
/// A free function because the window handle gives back the view itself, and
/// the view's own context is what `save_failed` needs.
fn f_save_failed(form: &mut crate::form::ConnectionForm, why: String, cx: &mut Context<crate::form::ConnectionForm>) {
    form.save_failed(why, cx);
}

/// Print the numbers every panel is laid out from, and what each would hold.
///
/// Because "the panel is empty" is not a number, and three times today a
/// drawing that nobody could look at reached the user broken. The window opens
/// on a workspace tag that is not displayed, so neither end of this
/// conversation has eyes on it — this is the substitute.
pub fn report_panels(cx: &gpui::App) {
    // READ THIS AS "THE DATA IS RIGHT", NOT "THE PANEL IS VISIBLE".
    //
    // It measures what a panel is built FROM — widths, heights, group counts —
    // and it reported all of that correct while the settings panel was drawing
    // nothing at all. It cannot see the layout, so a green line here narrows the
    // search to the drawing; it never closes it.
    println!("NOTE: these are the INPUTS to the layout, not what reached the screen.");
    println!("      Everything here can be right while a panel still draws nothing.");
    let ms = Metrics::default();
    println!("base {} px, icon {} px", ms.base(), ms.icon_size());
    println!("rail      {:>6.0} px wide", ms.rail_w());
    println!("panel_w   {:>6.0} px   <- a panel with no width is a panel with no content", ms.panel_w());
    println!("line_w    {:>6.0} px", ms.line_w());
    println!("outcome_w {:>6.0} px", ms.outcome_w());
    println!("line_h    {:>6.0} px", f32::from(ms.line_h()));
    println!("row_h     {:>6.0} px (grid)", ms.row_h());
    println!("panel_row {:>6.0} px", ms.panel_row_h());
    println!("action    {:>6.0} px in a {:.0} px box", f32::from(ms.action()), ms.action_box());
    // The chrome, which used to be literals chosen against a base of 12.
    println!("status_h  {:>6.0} px   tab_h {:.0}   control_h {:.0}", ms.status_h(), ms.tab_h(), ms.control_h());
    println!("num_w     {:>6.0} px   indent {:.0}   gutter {:.0}   marker {:.0}", ms.num_w(), ms.indent(), ms.gutter(), ms.marker_w());
    println!("palette   {:>6.0} px tall, {:.0} px down", ms.palette_h(), ms.palette_top());
    let (ww, wh) = ms.window(crate::metrics::Window::Settings);
    println!("settings window {ww:.0} x {wh:.0} px");

    // Every number above has to be positive, or the thing it sizes is invisible
    // while still drawing its border — which is exactly what an "empty panel
    // with an edge" looks like.
    let bad: Vec<&str> = [
        ("rail_w", ms.rail_w()),
        ("panel_w", ms.panel_w()),
        ("line_w", ms.line_w()),
        ("outcome_w", ms.outcome_w()),
        ("line_h", f32::from(ms.line_h())),
        ("row_h", ms.row_h()),
        ("panel_row_h", ms.panel_row_h()),
    ]
    .into_iter()
    .filter(|(_, v)| !(*v > 0.0))
    .map(|(n, _)| n)
    .collect();
    if bad.is_empty() {
        println!("geometry: every width and height is positive");
    } else {
        println!("geometry: NOT POSITIVE: {}", bad.join(", "));
    }

    // The settings WINDOW's categories. There is no settings panel any more:
    // the gear opens a window, because a list beside the detail of what is
    // picked in it is not a shape a narrow column can hold.
    let cats = crate::settings::Category::all();
    println!(
        "settings categories: {} ({})",
        cats.len(),
        cats.iter().map(|c| c.title()).collect::<Vec<_>>().join(" · ")
    );

    // Which panels END by returning their own list. Settings once did not: it
    // fell through to the connections path and ended up with two scrolling
    // lists sharing one height.
    println!("panels that return their own list: connections, instance, history, audit, tasks");
    println!("it cannot see the layout.");
    // `LODE_THEMES` and its five layouts were listed here. They were ways to
    // arrange a theme list inside a NARROW COLUMN, and the column is gone: the
    // list lives in the settings window, where it is simply a list.
    let (dw, dh) = (ms.panel_w(), f32::from(ms.line_h()) * 7.0);
    println!("bottom drawer {dh:.0} px tall; panel {dw:.0} px wide");
    let _ = cx;
}

/// Print which fonts were chosen and whether they carry every glyph.
///
/// The honest form of "the icons look right": the family that was actually
/// resolved, and per glyph its presence and its measured ink box. Equal boxes
/// are the claim, so the boxes are what gets printed.
pub fn report_icons(cx: &gpui::App) {
    let text = pick_font(cx);
    let icons = pick_icon_font(cx, &text);
    println!("text font:  {text}");
    println!("icon font:  {icons}");
    let font = gpui::Font {
        family: icons.clone().into(),
        features: Default::default(),
        fallbacks: None,
        weight: Default::default(),
        style: Default::default(),
    };
    let id = cx.text_system().resolve_font(&font);
    let (mut present, mut widths) = (0usize, Vec::new());
    for (g, fallback) in GLYPHS {
        let ch = g.chars().next().unwrap_or(' ');
        match cx.text_system().typographic_bounds(id, px(GLYPH_PROBE), ch) {
            Ok(b) => {
                present += 1;
                widths.push(cx
                    .text_system()
                    .advance(id, px(GLYPH_PROBE), ch)
                    .map(|a| f32::from(a.width))
                    .unwrap_or(0.0));
                println!(
                    "  U+{:04x}  present  ink {:5.1} x {:5.1}",
                    ch as u32, f32::from(b.size.width), f32::from(b.size.height)
                );
            }
            Err(_) => println!("  U+{:04x}  MISSING  drawn as \"{fallback}\"", ch as u32),
        }
    }
    println!("{present}/{} glyphs present", GLYPHS.len());
    if let (Some(lo), Some(hi)) = (
        widths.iter().cloned().reduce(f32::min),
        widths.iter().cloned().reduce(f32::max),
    ) {
        println!("advance width: {lo:.1} .. {hi:.1} (equal means one cell per icon)");
    }
    let ms = Metrics::default();
    println!(
        "icon {} px, box {} px, rail {} px wide, button {} px tall, gap {} px",
        ms.icon_size(),
        ms.icon_box(),
        ms.rail_w(),
        ms.rail_h(),
        ms.rail_gap()
    );
}

/// One saved tunnel or proxy, as the engine lists it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProfileRow {
    pub name: String,
    #[serde(default)]
    pub spec: Value,
}

impl ProfileRow {
    /// What this profile IS, in one line, from its own spec.
    ///
    /// Read off the saved shape rather than stored beside it: a description
    /// written at save time is a second copy that goes stale the first time the
    /// profile is edited.
    pub fn summary(&self) -> String {
        let g = |k: &str| self.spec.get(k).and_then(Value::as_str).unwrap_or("");
        let host = g("host");
        if host.is_empty() {
            return "—".into();
        }
        let user = g("user");
        let port = self.spec.get("port").and_then(Value::as_u64).unwrap_or(22);
        let auth = self
            .spec
            .get("auth")
            .and_then(|a| a.get("kind"))
            .and_then(Value::as_str)
            .or_else(|| self.spec.get("kind").and_then(Value::as_str))
            .unwrap_or("");
        let who = if user.is_empty() { String::new() } else { format!("{user}@") };
        if auth.is_empty() {
            format!("{who}{host}:{port}")
        } else {
            format!("{who}{host}:{port} · {auth}")
        }
    }

    /// The same line, but knowing which KIND of profile it is.
    ///
    /// A proxy has no port 22 and no `user@`. `summary` assumes a tunnel —
    /// which is what it was written for — and a SOCKS proxy on no port at all
    /// came out as `socks-host:22`, a sentence that is simply false. What a
    /// proxy has is a kind (`socks5`, `http`) and a host and port, so that is
    /// what it says.
    pub fn summary_of(&self, kind: &str) -> String {
        if kind != "proxy" {
            return self.summary();
        }
        let g = |k: &str| self.spec.get(k).and_then(Value::as_str).unwrap_or("");
        let host = g("host");
        if host.is_empty() {
            return "—".into();
        }
        let how = match g("kind") {
            "" => "socks5",
            other => other,
        };
        match self.spec.get("port").and_then(Value::as_u64) {
            Some(port) => format!("{how} · {host}:{port}"),
            // No port rather than an invented one: a profile saved without one
            // is a profile that is not finished, and saying `:1080` would hide
            // that behind a plausible number.
            None => format!("{how} · {host} (no port)"),
        }
    }
}

/// What a name is being asked for.
///
/// A saved query used to take the TAB'S TITLE, which is "Draft" for anything
/// typed by hand and the first forty characters of the statement otherwise —
/// so two drafts saved in a row overwrote each other without a word, and
/// nothing in the window ever asked what to call them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameFor {
    Query,
    Chart,
    /// Which folder a saved connection is filed in. An empty answer takes it
    /// out of one, which is why this one accepts an empty name where the other
    /// two refuse it.
    Folder(String),
}

impl NameFor {
    fn what(&self) -> String {
        match self {
            NameFor::Query => "query".into(),
            NameFor::Chart => "chart".into(),
            NameFor::Folder(conn) => conn.clone(),
        }
    }
}

/// Every name in a schema tree, at every depth.
///
/// Flattened, because a statement names a table without naming the schema it
/// is in as often as not, and a completion that only offered the top level
/// would offer the one thing nobody types.
fn collect_names(nodes: &[Node], out: &mut Vec<String>) {
    for n in nodes {
        if !out.iter().any(|x| x == &n.name) {
            out.push(n.name.clone());
        }
        collect_names(&n.children, out);
    }
}

/// An export walking the whole result.
struct Exporting {
    tab: u64,
    call_id: u64,
    /// The task's row in the drawer, so progress is visible where every other
    /// long thing reports — and so it can be stopped from there.
    task: u64,
    at: usize,
    writer: crate::export::Streaming,
    total: Option<usize>,
}

/// A page of the bottom drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawer {
    Tasks,
    Log,
    Command,
}

impl Drawer {
    fn key(self) -> &'static str {
        match self {
            Drawer::Tasks => "tasks.title",
            Drawer::Log => "bottom.log",
            Drawer::Command => "console.title",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Drawer::Tasks => "tasks",
            Drawer::Log => "log",
            Drawer::Command => "command",
        }
    }
}

/// One line in the command palette.
pub struct PaletteItem {
    pub label: String,
    pub hint: String,
    pub act: PaletteAct,
}

#[derive(Clone)]
pub enum PaletteAct {
    Rail(Rail),
    View(ResultView),
    GoTab(u64),
    Connect(String),
    Open { conn: String, object: String, schema: Option<String>, kind: String },
    SavedQuery { conn: String, sql: String, name: String },
    HistoryItem { conn: String, sql: String },
    RunActive,
    ToggleExport,
    SaveQuery,
    ToggleConsole,
    NewConnection,
    /// Open a SAVED connection's settings, by name.
    EditConnection(String),
    NewProfile,
    /// Open a saved tunnel's own window, by name.
    EditProfile(String),
    /// A column's width sideways, signed.
    ScrollGrid(f32),
    /// The keystrokes, reachable by name as well — the palette is where a
    /// shortcut is learned.
    NewTab,
    CloseTab,
    LastTab,
    TogglePanel,
    OpenSettings,
    /// A page of rows, signed.
    Page(i64),
    CopyCell,
    CopyRow,
    CopyPage,
}

/// Read a typed cell back into a JSON value.
///
/// `null` is the empty word for "no value", and it has to be typed exactly —
/// otherwise a column holding the TEXT "null" could never be written. The rest
/// follows what the cell held before: a number stays a number, and everything
/// else is text, because guessing that "007" is seven is how a product code
/// becomes an integer.
fn parse_cell(typed: &str, was: Option<&Value>) -> Value {
    if typed == "null" {
        return Value::Null;
    }
    match was {
        Some(Value::Number(_)) => match typed.parse::<i64>() {
            Ok(i) => json!(i),
            Err(_) => match typed.parse::<f64>() {
                Ok(f) => json!(f),
                // It was a number and this is not one. Send the text and let
                // the database refuse it — which it will, clearly, in its own
                // words, rather than this silently storing a zero.
                Err(_) => json!(typed),
            },
        },
        Some(Value::Bool(_)) => match typed {
            "true" => json!(true),
            "false" => json!(false),
            other => json!(other),
        },
        _ => json!(typed),
    }
}

/// Rows fetched per page.
const PAGE: usize = 200;

/// Grid metrics. A data grid wants DENSITY — the complaint about the first
/// version was that there was more air than data in it. 20px a row with 6px of
/// side padding fits a third again as many rows on a screen and still leaves a
/// line of space between them; 150px is a column that shows a short value whole
/// without wasting half a screen on an id.
// Was `const ROW_H: f32 = 20.0; const COL_W: f32 = 150.0;`. Both now come from
// the type scale: a row that does not grow with the text in it pinches, and the
// pinch is invisible in a screenshot taken at the old size. See `metrics.rs`.
//
// The row-number column went the same way (`metrics::num_w`). It was 44 and it
// takes part in the grid's total width, so at a large text size the numbers
// overflowed a column the arithmetic still believed was 44 wide — and the last
// data column became unreachable by exactly the difference.
const BLOCKING: Duration = Duration::from_secs(15);

/// What the icon rail selects. It changes the PANEL, never the work area.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rail {
    Connections,
    /// What the server on the other end is doing. Reading this COSTS the server
    /// a round trip, which is why it is its own panel: it happens when it is
    /// being looked at and not otherwise.
    Instance,
    /// Every statement this engine has run, from either client — the history is
    /// the ENGINE's, so the editor's runs are in here too.
    History,
    /// What CHANGED, and what was stopped from changing. Different panel from
    /// History on purpose: history is every statement, most of them reads, and
    /// a refusal buried in a thousand SELECTs is a refusal nobody finds.
    Audit,
    Tasks,
    // There is no `Settings` here any more. The gear at the bottom of the rail
    // opens a WINDOW: almost everything that belongs in settings is a list
    // beside the detail of what is picked in it — tunnels, proxies, hooks,
    // drivers, MCP policies — and a narrow column cannot hold that shape.
}

impl Rail {
    fn glyph(self) -> &'static str {
        match self {
            // Font Awesome codepoints, carried by every Nerd Font patch.
            Rail::Connections => "\u{f1c0}",
            Rail::Instance => "\u{f080}",
            Rail::History => "\u{f1da}",
            Rail::Audit => "\u{f132}",
            Rail::Tasks => "\u{f0ae}",
        }
    }
    /// The key its name is under. Not the name itself: a `&'static str` here
    /// would be the English one, and this is read in whatever language the
    /// window is drawn in.
    fn key(self) -> &'static str {
        match self {
            Rail::Connections => "rail.connections",
            Rail::Instance => "rail.instance",
            Rail::History => "rail.history",
            Rail::Audit => "rail.audit",
            Rail::Tasks => "rail.tasks",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Rail::Connections => "Connections",
            Rail::Instance => "Instance",
            Rail::History => "History",
            Rail::Audit => "Audit",
            Rail::Tasks => "Tasks",
        }
    }
}

/// What an outstanding request was for.
enum Pending {
    /// `schema.ddl` asked for by the context menu: the answer goes on the
    /// clipboard rather than into a tab.
    DdlCopy,
    /// One page of an export walking the whole result.
    ExportPage,
    /// What the settings window's Access and MCP pages report.
    AccessList,
    Approvals,
    /// An answer given; the queue is asked for again.
    ApprovalDecided,
    /// A sign-in the engine was asked to resolve, and one it was asked to
    /// store as a profile.
    AccessTest { profile: String },
    AccessImport { profile: String },
    /// An older client's database, read in.
    StoreImport,
    /// The saved scripts, for the names a hook can point at.
    ScriptNames,
    StoreStatus,
    Connections,
    Connect { name: String, driver: String },
    Disconnect { name: String },
    Structure { name: String },
    Execute { tab: u64, task: u64 },
    Page { tab: u64 },
    Ignore(&'static str),
    /// A console line, waiting to hear whether it destroys something.
    Classify { conn: String, statement: String },
    /// One `conn.test` stage asked from the connection window.
    FormTest { stage: &'static str, form: WeakEntity<ConnectionForm> },
    /// A secret the connection window asked to be moved into the wallet. The
    /// field is only switched over once the engine says it LANDED: switching
    /// first and failing after would lose the password that was typed.
    VaultPut { field: String, name: String, form: WeakEntity<ConnectionForm> },
    /// The saved tunnel or proxy profiles, by kind.
    Profiles { kind: &'static str },
    /// The AWS profile names the engine found in `~/.aws/config`.
    AwsProfiles,
    /// A profile was stored; the list is asked for again.
    ProfileSaved { kind: &'static str },
    /// Which connections point at one profile, asked before touching it.
    ProfileUsers { name: String },
    /// A tree node's fields and indexes, asked when it was expanded.
    NodeColumns { key: String },
    NodeIndexes { key: String },
    /// The small sample a document store needs before it can say what a field
    /// holds. Two steps, because a result is run and then paged.
    NodeSample { key: String },
    NodeSamplePage { key: String, call_id: u64 },
    /// How many saved queries a connection has, asked BEFORE it is removed so
    /// the question can say what would be lost.
    ConnQueryCount { name: String },
    ConnRemoved { name: String },
    /// A profile was removed, and the engine says which connections used it.
    ProfileRemoved { kind: &'static str, name: String },
    /// A tab's statement was sent to be classified before it is run.
    ClassifyTab { tab: u64, statement: String },
    /// The engine was asked to rename a connection; the edited spec follows
    /// once it answers, so a refused rename never reaches the store as a save.
    Rename { to: String, driver: String, spec: Value },
    /// A connection saved from that window. `rename_from` is the name it used
    /// to have: the removal of the old one waits for this save to SUCCEED,
    /// because sending both at once loses the connection entirely when the
    /// save is refused (a name already taken, a store that would not write).
    FormSave { rename_from: Option<String> },
    History,
    Audit,
    Metrics { conn: String },
    SavedQueries { conn: String },
    SavedCharts { conn: String },
    /// The dashboards on one connection, by name.
    Dashboards { conn: String },
    /// One board's layout, on its way into a tab.
    Board { tab: u64 },
    /// The saved charts a board needs to find the statements behind its names.
    BoardCharts { tab: u64 },
    /// One tile's statement, launched. `tile` is the index into the board's
    /// tiles — a board is drawn from that list in that order, and the index is
    /// what connects a running call back to the square it belongs to.
    BoardRun { tab: u64, tile: usize },
    BoardPage { tab: u64, tile: usize },
    BoardSaved { tab: u64 },
    /// The columns of a tab's object — where the identifying key comes from.
    Columns { tab: u64 },
    Indexes { tab: u64 },
    Ddl { tab: u64 },
    /// A write the engine has been asked to DESCRIBE, not to do.
    RowPlan { tab: u64, row: usize, key: Vec<(String, Value)>, set: Vec<(String, Value)>, delete: bool },
    /// The same write, confirmed.
    RowApply { tab: u64 },
}

pub struct Workspace {
    theme: Theme,
    theme_name: String,

    engine: Option<Engine>,
    hello: Option<Hello>,
    fatal: Option<String>,
    /// How many times the engine has been started again after it exited, and
    /// when the next attempt is due. See [`respawn_delay`].
    /// Keys somebody has changed: action name → keystroke. Applied at startup
    /// by `bind_workspace_keys`; changing one here rebinds it at once.
    key_over: std::collections::BTreeMap<String, String>,
    /// Folders folded away, by key. Remembered, like the panel itself.
    shut_groups: std::collections::BTreeSet<String>,
    /// Something asked for the keyboard back, from a place that had no
    /// `Window` to give it. Acted on by `pump`, which has one.
    focus_back: bool,
    /// The context menu, while one is open — see `menu.rs`.
    menu: Option<crate::menu::Open>,
    /// Which tabs were visited, most recent first — see `mru_next`. Ctrl-tab
    /// goes along this rather than along the strip, so pressing it twice comes
    /// back to where it started.
    mru: Vec<u64>,
    /// The panel, folded away. Remembered, because somebody who wants the room
    /// wants it next time too.
    panel_hidden: bool,
    /// An export writing the WHOLE result, page by page.
    ///
    /// The tab it is reading, the file being written, and the next offset to
    /// ask for. Held here rather than in the tab because it outlives what the
    /// tab shows: the grid goes on holding one page while this walks the rest.
    exporting: Option<Exporting>,
    /// A connection somebody asked to close while it was still working, and
    /// what it is working on.
    confirm_disconnect: Option<(String, Vec<String>)>,
    /// How far back in the console's history the prompt is showing, if at all.
    console_hist: Option<usize>,
    /// A connection somebody has asked to remove, and what would go with it:
    /// `(name, saved queries)`. The count is `None` until the engine answers.
    confirm_conn_rm: Option<(String, Option<usize>)>,
    /// A save is waiting for a name — see `NameFor`.
    naming: Option<NameFor>,
    save_name: Entity<TextInput>,
    /// Something changed that belongs in the session file, and it has not been
    /// written yet. See `remember`.
    session_dirty: bool,
    /// When it was last written, so a burst of changes costs one write.
    session_written: Option<Instant>,
    /// Warnings and errors nobody has looked at yet. The log used to be
    /// write-only — twenty places pushed into it and no screen ever read it,
    /// so a plaintext warning or a refused classify went nowhere. This is the
    /// number beside the task count, and opening Tasks clears it.
    log_unread: usize,
    respawn_attempts: u32,
    respawn_at: Option<std::time::Instant>,
    store_ok: bool,
    /// Where the engine's own database is, as it answered — shown on the
    /// settings window's About page, because "where is my saved connection"
    /// has no answer anywhere else.
    store_path: String,
    /// What the engine found for `access.list`, cached for the settings window.
    /// Asked when that window opens: it costs a round trip and is not wanted
    /// until somebody looks.
    access: Vec<Value>,
    /// The saved scripts' names, for a hook that points at one by name.
    script_names: Vec<String>,
    /// The MCP approval queue — what an agent has asked a PERSON to allow.
    ///
    /// Kept in the window rather than only in the settings page, because it is
    /// not a setting: something is waiting for an answer, and a queue nobody
    /// is told about is a queue that never empties. The status line counts it.
    approvals: Vec<Value>,
    /// When the queue was last asked for, so the poll is a poll and not a race
    /// with the frame rate.
    approvals_at: Option<Instant>,

    saved: Vec<SavedConnection>,
    conns: Vec<Live>,
    /// Which schema branches are open, keyed by connection + path.
    open_nodes: HashSet<String>,
    /// A table's or collection's fields and indexes, by the node's key.
    ///
    /// Asked when the node is EXPANDED, not when the connection opens: he has
    /// thirty-nine collections, and asking for both up front would be
    /// seventy-eight requests before anything is looked at.
    node_fields: HashMap<String, Vec<TableColumn>>,
    node_indexes: HashMap<String, Vec<Index>>,
    /// Keys already asked for, so re-opening a node does not ask again.
    node_asked: HashSet<String>,
    /// A small sample of documents per node, for drivers that declare no column
    /// types — which is every document store. The derived type is SAID to be
    /// derived; see `NODE_SAMPLE`.
    node_sample: HashMap<String, Vec<Vec<Value>>>,
    /// The column order of that sample, so a value can be matched to a field.
    node_sample_cols: HashMap<String, Vec<String>>,
    /// A sample's call, by id, while it runs: `call_id -> node key`. A sample is
    /// the one call that belongs to NEITHER a tab nor a board tile, so without
    /// this its `query.state` has nowhere to land.
    sample_calls: HashMap<u64, String>,

    tabs: Vec<Tab>,
    active: usize,
    next_tab: u64,

    pending: HashMap<u64, Pending>,
    /// call_id → tab id.
    calls: HashMap<u64, u64>,
    /// A dashboard tile's running call: call_id to (tab, tile). Separate from
    /// `calls` because a board tile is not the tab's one result — a board has
    /// as many results at once as it has charts.
    board_calls: HashMap<u64, (u64, usize)>,
    /// Words to put in the tile box on the next pump.
    ///
    /// Set from `board_drag_start`, which is a mouse-down handler already
    /// borrowing the workspace and has no `Context` to update an entity with.
    pending_tile_text: Option<String>,
    /// Which TAB has a tile being dragged, tracked at the root like a column
    /// resize: the pointer leaves the tile's own square long before the drag is
    /// over. What is being dragged, and how, is on the board's own `Drag`.
    dragging: Option<u64>,
    /// How wide the canvas was last time it was drawn.
    ///
    /// DERIVED from the viewport rather than measured: GPUI lays out after the
    /// element tree is built, so the width a drag needs is not available while
    /// the drag handler is being written. Derived is enough because the drag
    /// only needs it to decide WHICH CELL the pointer is over, and the layout
    /// it is derived from is the one this file also writes.
    canvas_w: f32,
    /// call_id → task id, so a finished call closes its task.
    call_tasks: HashMap<u64, u64>,

    tasks: Vec<Task>,
    next_task: u64,
    /// Which page of the bottom drawer is open, if any.
    ///
    /// ONE strip instead of three. The console had its own bar with its own
    /// heading, the tasks had another, and the status line a third — so between
    /// a quarter and a third of the window's height was chrome saying what it
    /// was rather than showing anything, and all three were there whether they
    /// held anything or not.
    drawer: Option<Drawer>,

    rail: Rail,
    status: String,
    log: std::collections::VecDeque<(String, String)>,

    /// What was open last time, held until its connections come back: a tab can
    /// only be reopened once its connection has a schema again, which is several
    /// round trips after start.
    restore: Session,

    /// The sidebar's filter box.
    filter: Entity<TextInput>,
    /// Every field of every tab's query bar, keyed by (tab, field).
    ///
    /// The empty key USED to be the statement box. It is now a multi-line
    /// editor, which is a different entity — see `editors`.
    inputs: HashMap<(u64, String), Entity<TextInput>>,
    /// The statement editor of each tab that has one: a driver with a QUERY
    /// FORM gets fields instead, because what it wants is a filter and a limit
    /// rather than a language.
    editors: HashMap<u64, Entity<crate::editor_el::Editor>>,

    /// The console: a line typed straight at a connection.
    console: Entity<TextInput>,
    history: Vec<HistoryRow>,
    /// The history panel's own search box.
    history_filter: Entity<TextInput>,
    /// The engine's audit log, and how much of a statement it is keeping.
    ///
    /// `audit_mode` is shown to the reader: a blank statement column with no
    /// explanation reads as a broken client, and it is a setting on the engine.
    audit: Vec<AuditRow>,
    audit_mode: String,
    audit_filter: Entity<TextInput>,
    /// The three boxes of a board's shared time range. One set, not one per
    /// board: only one board is being looked at at a time, and three boxes per
    /// open tab would be three boxes nobody can see.
    /// The board's name, and the selected tile's words.
    board_name: Entity<TextInput>,
    tile_text: Entity<TextInput>,
    board_time_col: Entity<TextInput>,
    board_time_from: Entity<TextInput>,
    board_time_to: Entity<TextInput>,
    /// The type scale. One number; every size in the window comes off it.
    scale: Metrics,
    /// Which row's full reason is open. One at a time: a refusal's reason is a
    /// sentence, and a list where every row is three lines tall is a list
    /// nobody scans.
    audit_open: Option<i64>,

    /// What the server last said about itself, and which connection said it.
    metrics: Vec<lode_proto::Metric>,
    metrics_conn: Option<String>,
    metrics_error: Option<String>,
    /// How often to ask again, or None for "only when asked".
    ///
    /// None by DEFAULT. Repeating a statistics query against somebody's
    /// production database is a decision for the person looking at it, and a
    /// window that starts a timer on its own has made that decision for them.
    metrics_every: Option<Duration>,
    metrics_at: Option<Instant>,
    metrics_inflight: bool,

    /// The command palette, while it is open.
    palette_open: bool,
    palette_input: Entity<TextInput>,
    palette_sel: usize,

    /// Short-lived corner messages. Never the only place an error appears.
    toasts: Vec<Toast>,
    next_toast: u64,

    /// A column being dragged wider: (tab, column, where the drag began, the
    /// width it had then).
    resizing: Option<(u64, usize, f32, f32)>,
    /// Dragging the panel's edge: where the pointer started and how wide the
    /// panel was then.
    panel_drag: Option<(f32, f32)>,

    /// Where the result grid is scrolled to, sideways.
    grid_scroll: gpui::ScrollHandle,
    /// The row list's own handle, so moving the cursor with the keyboard can
    /// bring the row into view. Without it `j` thirty times moved a selection
    /// nobody could see: the highlight was real and thirty rows below the fold.
    grid_rows: gpui::UniformListScrollHandle,
    /// How much wider than the window the grid is, in pixels.
    ///
    /// DERIVED from the column widths rather than read from the scroll handle.
    /// `ScrollHandle::max_offset` is filled during PAINT, so the first render
    /// reads zero — and since nothing redraws afterwards, the scrollbar never
    /// appeared and the first keypress clamped the scroll to nothing. A `Cell`
    /// because `table_el` takes `&self`, and the alternative is threading a
    /// width back out of a render function.
    grid_overflow: std::cell::Cell<f32>,
    /// Whether the horizontal bar is being dragged right now.
    ///
    /// A drawn bar that cannot be grabbed is decoration, and decoration that
    /// looks like a control is worse than no control: somebody tries it, it
    /// does nothing, and they conclude the grid does not scroll.
    hbar_drag: std::cell::Cell<bool>,
    /// The language the chrome is drawn in. The ENGINE is not translated —
    /// its refusals and its audit log are the record, and a record whose
    /// meaning depends on who ran it cannot be read by anybody else.
    lang: Lang,
    /// How many spaces the JSON view indents by. Two, three or four: the
    /// argument about which is right is older than this program, so it is a
    /// setting rather than a decision.
    json_indent: u8,
    /// The AWS profile names out of `~/.aws/config`, for a parameter that a
    /// driver declares as `ParamType::AwsProfile`.
    aws_profiles: Vec<String>,
    /// The open tunnel window, if there is one.
    profile_window: Option<gpui::WindowHandle<crate::profile::ProfileForm>>,
    /// The settings window. A WINDOW, because almost everything in settings is
    /// a list beside the detail of what is picked in it, and a panel is a
    /// column.
    settings_window: Option<gpui::WindowHandle<crate::settings::SettingsWindow>>,
    /// A profile somebody has pressed delete on, waiting to be confirmed.
    /// Which connections point at each profile, by profile name. Asked before
    /// anything is removed, which is what makes it a warning.
    profile_users: std::collections::BTreeMap<String, Vec<String>>,
    /// The saved tunnel and proxy profiles, by kind.
    ///
    /// A bastion is one machine and twenty connections go through it. The engine
    /// has had profiles since protocol 9 and no client has ever shown them, so
    /// the feature existed and was unreachable.
    profiles: std::collections::BTreeMap<&'static str, Vec<ProfileRow>>,
    /// How wide one character is, per pixel of text size.
    ///
    /// Measured once at startup from the font actually resolved, so "does this
    /// line fit" is arithmetic rather than a guess. The text font here is
    /// monospaced, which makes it exact; for a proportional one it is an
    /// average, and the comparison is deliberately biased so that a tooltip is
    /// MISSED rather than shown on a line that was never cut — a tip on every
    /// row is noise, and noise is worse than a gap.
    char_ratio: f32,
    /// The glyphs the ICON font does not have, worked out once at startup.
    missing_glyphs: std::collections::HashSet<char>,
    /// The font the icons are drawn in — not the font the text is drawn in.
    icon_family: String,
    /// The font family the window is actually drawing in.
    ///
    /// Was the literal `"CodeNerdFont"`, which is a font on ONE machine. It is
    /// now the first of a short list that the text system reports as installed,
    /// and the Settings panel says which one won — a window that silently fell
    /// back to a default is a window whose icons are empty boxes for a reason
    /// nobody can see.
    font_family: String,
    /// Every theme file found, by name. Read at start and on demand, because a
    /// theme installed while the window is open should be findable without
    /// restarting it.
    themes: std::collections::BTreeMap<String, std::path::PathBuf>,
    /// The window's own focus, so the vim keys know whether a field has it.
    focus: FocusHandle,
    /// The export row, while it is open, and where it would write.
    export_open: bool,
    export_format: Format,
    export_path: Entity<TextInput>,
    /// The connection window, while one is open. A WINDOW, not a layer: the
    /// window manager owns it like any other.
    form_window: Option<WindowHandle<ConnectionForm>>,
    /// A statement the ENGINE called destructive, held with its reason until
    /// someone says yes. Nothing runs from here without that.
    confirm: Option<(String, String, String)>,
}

impl Workspace {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Read ONCE, and FIRST: the theme, the type scale and the window's
        // fields all come out of it.
        let restore = session::load();
        let font_family = pick_font(cx);
        let icon_family = pick_icon_font(cx, &font_family);
        // The session's theme before anything else. It was being saved and
        // never read, so the choice survived to the file and no further.
        let (theme_name, theme) = theme::pick_named(restore.theme.as_deref());
        let colors = input_colors(&theme);
        let filter = cx.new(|cx| TextInput::new(cx, "Filter connections and schema…", colors));
        cx.subscribe(&filter, |_this, _input, _ev: &InputEvent, cx| {
            // Typed into the filter: the panel is rebuilt from it each frame,
            // so there is nothing to do but ask for that frame.
            cx.notify();
        })
        .detach();
        let export_path = cx.new(|cx| TextInput::new(cx, "result.csv", colors));
        let history_filter = cx.new(|cx| TextInput::new(cx, "Search history…", colors));
        let audit_filter = cx.new(|cx| TextInput::new(cx, "Search the audit log…", colors));
        let board_name = cx.new(|cx| TextInput::new(cx, "name this dashboard", colors));
        let tile_text = cx.new(|cx| TextInput::new(cx, "heading for this tile", colors));
        let board_time_col = cx.new(|cx| TextInput::new(cx, "column", colors));
        let board_time_from = cx.new(|cx| TextInput::new(cx, "2026-01-01", colors));
        let board_time_to = cx.new(|cx| TextInput::new(cx, "2026-02-01", colors));
        let palette_input = cx.new(|cx| TextInput::new(cx, "Type to find a command…", colors));
        let save_name = cx.new(|cx| TextInput::new(cx, "name it, then press Enter", colors));
        cx.subscribe(&save_name, |this, input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Submit) {
                let name = input.read(cx).text().trim().to_string();
                this.named(name, cx);
            }
            cx.notify();
        })
        .detach();
        let console = cx.new(|cx| {
            TextInput::new(cx, "Type a command and press Enter — it runs against the open connection", colors)
        });
        cx.subscribe(&console, |this, input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Submit) {
                let line = input.read(cx).text().trim().to_string();
                if !line.is_empty() {
                    this.console_hist = None;
                    this.console_submit(line, cx);
                }
            }
            cx.notify();
        })
        .detach();
        let mut w = Workspace {
            theme,
            theme_name,
            engine: None,
            hello: None,
            fatal: None,
            exporting: None,
            confirm_disconnect: None,
            console_hist: None,
            key_over: restore.keys.clone().unwrap_or_default(),
            shut_groups: restore
                .shut_groups
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            focus_back: false,
            menu: None,
            mru: Vec::new(),
            panel_hidden: restore.panel_hidden.unwrap_or(false),
            confirm_conn_rm: None,
            naming: None,
            save_name,
            session_dirty: false,
            session_written: None,
            log_unread: 0,
            respawn_attempts: 0,
            respawn_at: None,
            store_ok: false,
            store_path: String::new(),
            access: Vec::new(),
            script_names: Vec::new(),
            approvals: Vec::new(),
            approvals_at: None,
            saved: Vec::new(),
            conns: Vec::new(),
            open_nodes: HashSet::new(),
            node_fields: HashMap::new(),
            node_indexes: HashMap::new(),
            node_asked: HashSet::new(),
            node_sample: HashMap::new(),
            node_sample_cols: HashMap::new(),
            sample_calls: HashMap::new(),
            tabs: Vec::new(),
            active: 0,
            next_tab: 1,
            pending: HashMap::new(),
            calls: HashMap::new(),
            board_calls: HashMap::new(),
            pending_tile_text: None,
            dragging: None,
            canvas_w: 900.0,
            call_tasks: HashMap::new(),
            tasks: Vec::new(),
            next_task: 1,
            drawer: None,
            rail: Rail::Connections,
            status: "starting the engine…".into(),
            log: std::collections::VecDeque::new(),
            restore: restore.clone(),
            filter,
            inputs: HashMap::new(),
            editors: HashMap::new(),
            console,
            history: Vec::new(),
            history_filter,
            audit: Vec::new(),
            audit_mode: "full".into(),
            audit_filter,
            audit_open: None,
            board_name,
            tile_text,
            board_time_col,
            board_time_from,
            board_time_to,
            scale: Metrics::new(restore.font_size.unwrap_or(crate::metrics::DEFAULT_BASE))
                .with_icon(match restore.icon_size {
                    // A value equal to a default this client has shipped is
                    // treated as NOT CHOSEN. The session has always written the
                    // size out, picked or not, so nobody is distinguishable as
                    // "never chose one" — and without this a raised default
                    // would reach nobody who had ever opened the window.
                    // Nobody types a size by hand to exactly the number it
                    // already was.
                    Some(v) if crate::metrics::SHIPPED_ICON_DEFAULTS.contains(&v) => {
                        crate::metrics::DEFAULT_ICON
                    }
                    Some(v) => v,
                    None => crate::metrics::DEFAULT_ICON,
                })
                .with_panel(restore.panel_w),
            metrics: Vec::new(),
            metrics_conn: None,
            metrics_error: None,
            metrics_every: None,
            metrics_at: None,
            metrics_inflight: false,
            palette_open: false,
            palette_input,
            palette_sel: 0,
            toasts: Vec::new(),
            next_toast: 1,
            resizing: None,
            panel_drag: None,
            grid_scroll: gpui::ScrollHandle::new(),
            grid_rows: gpui::UniformListScrollHandle::new(),
            grid_overflow: std::cell::Cell::new(0.0),
            hbar_drag: std::cell::Cell::new(false),
            lang: restore
                .lang
                .as_deref()
                .and_then(Lang::from_tag)
                .or_else(Lang::from_env)
                .unwrap_or_default(),
            json_indent: restore.json_indent.unwrap_or(4).clamp(2, 4),
            themes: theme::discover(),
            profiles: std::collections::BTreeMap::new(),
            profile_window: None,
            settings_window: None,
            profile_users: std::collections::BTreeMap::new(),
            aws_profiles: Vec::new(),
            char_ratio: char_ratio(cx, &font_family),
            missing_glyphs: missing_in(cx, &icon_family),
            icon_family,
            font_family,
            focus: cx.focus_handle(),
            export_open: false,
            export_format: Format::Csv,
            export_path,
            form_window: None,
            confirm: None,
        };

        // The placeholders too, at startup: the language comes out of the
        // session AFTER the fields were built.
        w.relabel_inputs(cx);

        w.rail = match w.restore.rail.as_str() {
            "history" => Rail::History,
            "audit" => Rail::Audit,
            "instance" => Rail::Instance,
            "tasks" => Rail::Tasks,
            // "settings" is no longer a panel — the gear opens a window. A
            // session saved while it WAS a panel would otherwise restore to a
            // view that nothing can leave, because the rail no longer has a
            // button that selects it.
            _ => Rail::Connections,
        };
        // The history panel is empty until it is asked for, and if that is the
        // panel being restored it should not open blank.
        let want_history = w.rail == Rail::History;
        let want_audit = w.rail == Rail::Audit;

        match Engine::spawn(None) {
            Ok(engine) => match engine.hello(BLOCKING) {
                Ok(hello) => {
                    w.status = format!(
                        "{} {} — protocol {}, {} drivers",
                        hello.engine.as_deref().unwrap_or("engine"),
                        hello.version.as_deref().unwrap_or("?"),
                        hello.proto,
                        hello.drivers.len()
                    );
                    w.hello = Some(hello);
                    w.engine = Some(engine);
                    w.ask(Pending::StoreStatus, "store.status", json!({}));
                    w.ask(Pending::Connections, "store.connections.list", json!({}));
                    if want_history {
                        w.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                    }
                    if want_audit {
                        w.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
                    }
                }
                Err(e) => w.fatal = Some(e.to_string()),
            },
            Err(e) => w.fatal = Some(e.to_string()),
        }

        // The session is written from `pump` on a 500 ms debounce, so a change
        // made in the last half second would be lost on the way out. Written
        // once more here, unconditionally — this is the one moment where the
        // cost of a file write does not matter.
        cx.on_app_quit(|w: &mut Workspace, _| {
            w.write_session();
            async {}
        })
        .detach();

        // The engine answers on its own schedule, so the view polls it. A timer
        // rather than a blocking read: the window stays live while a query runs,
        // which is the whole point of the engine being another process.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(40)).await;
            if this.update(cx, |w: &mut Workspace, cx| w.pump(cx)).is_err() {
                break;
            }
        })
        .detach();

        w
    }

    // ── engine ───────────────────────────────────────────────────────────────

    fn ask(&mut self, what: Pending, method: &str, params: Value) {
        let Some(engine) = &self.engine else { return };
        match engine.send(method, params) {
            Ok(id) => {
                self.pending.insert(id, what);
            }
            Err(e) => self.fatal = Some(format!("lost the engine: {e}")),
        }
    }

    fn meta(&self, driver: &str) -> Option<&DriverMeta> {
        self.hello.as_ref()?.drivers.iter().find(|d| d.kind == driver)
    }

    /// Start the engine again after it exited, and pick the session back up.
    ///
    /// A short `hello` rather than the startup one: this runs on the window's
    /// thread, and fifteen seconds of a frozen window while a broken engine
    /// fails to answer is worse than saying so quickly and trying again.
    fn try_respawn(&mut self, _cx: &mut Context<Self>) {
        const RESPAWN_HELLO: Duration = Duration::from_secs(3);
        match Engine::spawn(None).and_then(|e| e.hello(RESPAWN_HELLO).map(|h| (e, h))) {
            Ok((engine, hello)) => {
                self.status = format!(
                    "{} {} — protocol {}, {} drivers",
                    hello.engine.as_deref().unwrap_or("engine"),
                    hello.version.as_deref().unwrap_or("?"),
                    hello.proto,
                    hello.drivers.len()
                );
                self.hello = Some(hello);
                self.engine = Some(engine);
                self.respawn_attempts = 0;
                self.fatal = None;
                self.toast("the engine is back".to_string(), false);
                // The saved list comes first: reconnecting what was open is
                // done from `restore.connections` once it arrives (see
                // `Pending::Connections`), which is the same path a cold start
                // takes.
                self.ask(Pending::StoreStatus, "store.status", json!({}));
                self.ask(Pending::Connections, "store.connections.list", json!({}));
            }
            Err(e) => {
                self.respawn_attempts += 1;
                match respawn_delay(self.respawn_attempts) {
                    Some(d) => {
                        self.status = format!("the engine would not start ({e}) — trying again…");
                        self.respawn_at = Some(std::time::Instant::now() + d);
                    }
                    // Out of tries: this is the one case that is fatal, and it
                    // says what went wrong rather than "restart".
                    None => {
                        self.fatal = Some(format!(
                            "the engine exited and would not start again: {e}. Its drivers run in \
                             a separate process, so this window survived it."
                        ));
                    }
                }
            }
        }
    }

    /// Put a line in the log, and count it if it is one somebody should see.
    ///
    /// `info` is background noise — what connected, what moved. `warn` and
    /// `error` are the ones that mean something went differently than asked,
    /// and those raise the unread count until the Tasks panel is opened.
    fn log_push(&mut self, level: &str, text: String) {
        if crate::workspace::counts_as_unread(level) {
            self.log_unread += 1;
        }
        self.log.push_back((level.to_string(), text));
        if self.log.len() > LOG_KEEP {
            self.log.pop_front();
        }
    }

    fn live(&self, name: &str) -> Option<&Live> {
        self.conns.iter().find(|c| c.name == name)
    }

    /// The connection a command without a tab of its own belongs to: the active
    /// tab's, falling back to the only one open. See [`pick_target`].
    fn target_conn(&self) -> Option<String> {
        let open: Vec<String> = self.conns.iter().map(|c| c.name.clone()).collect();
        let active = self.tabs.get(self.active).map(|t| t.conn_name.as_str());
        pick_target(active, &open)
    }

    /// The driver metadata for a connection BY NAME — what a board needs to ask
    /// the driver how it spells a time window.
    fn meta_of(&self, conn: &str) -> Option<&DriverMeta> {
        let driver = self
            .conns
            .iter()
            .find(|c| c.name == conn)
            .map(|c| c.driver.as_str())
            .or_else(|| {
                self.saved
                    .iter()
                    .find(|c| c.name == conn)
                    .map(|c| c.driver.as_str())
            })?;
        self.meta(driver)
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        // The session, if something asked for it to be written — see `remember`.
        self.flush_session();
        if self.engine.is_none() {
            if let Some(due) = self.respawn_at {
                if std::time::Instant::now() >= due {
                    self.respawn_at = None;
                    self.try_respawn(cx);
                    changed = true;
                }
            }
        }
        if let Some(words) = self.pending_tile_text.take() {
            self.tile_text.update(cx, |i, cx| i.set_text(words, cx));
        }
        self.poll_metrics();
        self.poll_board();
        self.poll_approvals();
        loop {
            let Some(engine) = &self.engine else { break };
            let Some(msg) = engine.try_recv() else { break };
            changed = true;
            match msg {
                Message::Closed => {
                    // **The window outlives the engine, and now it says so by
                    // acting.** The drivers run in another process so that one
                    // of them dying takes nothing else with it; telling the
                    // person to restart threw that away. What was connected is
                    // remembered, the tabs stop pretending to run, and the
                    // engine is started again on a widening delay.
                    self.restore.connections =
                        self.conns.iter().map(|c| c.name.clone()).collect();
                    self.engine = None;
                    self.conns.clear();
                    for tab in self.tabs.iter_mut().filter(|t| t.state == RunState::Running) {
                        tab.state = RunState::Failed;
                        tab.error = Some("the engine exited".into());
                    }
                    for task in self.tasks.iter_mut().filter(|t| t.running()) {
                        task.finish(Duration::from_millis(0), Some("the engine exited".into()));
                    }
                    self.status = "the engine exited — starting it again…".into();
                    self.toast("the engine exited — starting it again".to_string(), true);
                    self.respawn_at =
                        respawn_delay(self.respawn_attempts).map(|d| std::time::Instant::now() + d);
                    break;
                }
                Message::Notification { method, params } => self.on_notification(&method, params),
                Message::Response { id, result } => {
                    let Some(what) = self.pending.remove(&id) else { continue };
                    match result {
                        Ok(v) => self.on_result(what, v, cx),
                        Err(e) => self.on_error(what, e, cx),
                    }
                }
            }
        }
        // A running task's clock moves even when nothing arrived, so the
        // duration beside it is not frozen at the moment it started.
        if changed || self.tasks.iter().any(|t| t.running()) {
            cx.notify();
        }
    }

    fn on_notification(&mut self, method: &str, params: Value) {
        match method {
            "query.state" => {
                let Ok(st) = serde_json::from_value::<QueryState>(params) else { return };
                // A board tile's call first: a board has as many results at once
                // as it has charts, and none of them is the tab's one result.
                if let Some(&(tab, tile)) = self.board_calls.get(&st.call_id) {
                    let want_page = match st.state.as_str() {
                        "done" => true,
                        "failed" | "cancelled" => false,
                        _ => return,
                    };
                    if let Some(p) = self.pane_mut(tab, tile) {
                        p.state = match st.state.as_str() {
                            "done" => RunState::Done,
                            "failed" => RunState::Failed,
                            _ => RunState::Cancelled,
                        };
                        p.error = st.error.clone();
                    }
                    if want_page {
                        let call_id = st.call_id;
                        self.ask(
                            Pending::BoardPage { tab, tile },
                            "query.page",
                            json!({ "call_id": call_id, "from": 0, "n": PAGE }),
                        );
                    }
                    return;
                }
                // A sample's call next: it belongs to a node in the tree, not
                // to a tab, so nothing below would find it.
                if let Some(key) = self.sample_calls.remove(&st.call_id) {
                    match st.state.as_str() {
                        "done" => {
                            let call_id = st.call_id;
                            self.ask(
                                Pending::NodeSamplePage { key, call_id },
                                "query.page",
                                json!({ "call_id": call_id, "from": 0, "n": NODE_SAMPLE }),
                            );
                        }
                        "failed" | "cancelled" => {
                            // Nothing is remembered for the key: an empty sample
                            // cached here is a node that never shows its types
                            // again, however many times it is expanded.
                            let call_id = st.call_id;
                            self.ask(
                                Pending::Ignore("release"),
                                "query.release",
                                json!({ "call_id": call_id }),
                            );
                            if let Some(e) = st.error.clone() {
                                self.log_push("warn", format!("sample failed: {e}"));
                            }
                        }
                        // Still running: put it back and wait for the next word.
                        _ => {
                            self.sample_calls.insert(st.call_id, key);
                        }
                    }
                    return;
                }
                // A call whose TAB is gone — closed, or re-run under a new
                // call — still has a task watching it, and that task has to be
                // told. Before this the state arrived, found no tab, returned,
                // and the task counted upwards until the window closed; and
                // `pump()` keeps repainting while any task runs.
                let Some(&tab_id) = self.calls.get(&st.call_id) else {
                    if let crate::workspace::Settle::Finished(err) =
                        crate::workspace::task_settled(&st.state, st.error.as_deref())
                    {
                        if let Some(task_id) = self.call_tasks.remove(&st.call_id) {
                            if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                                t.finish(Duration::from_millis(st.ms), err);
                            }
                        }
                        // And let the buffer go: nothing is left to page it.
                        let call_id = st.call_id;
                        self.ask(
                            Pending::Ignore("release"),
                            "query.release",
                            json!({ "call_id": call_id }),
                        );
                    }
                    return;
                };
                let want_page = {
                    let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
                    tab.ms = st.ms;
                    match st.state.as_str() {
                        "done" => {
                            tab.state = RunState::Done;
                            tab.affected = st.affected;
                            tab.error = None;
                            true
                        }
                        "failed" => {
                            tab.state = RunState::Failed;
                            // An engine that refuses an unconfirmed statement
                            // says why: that sentence becomes the tab's own
                            // confirmation, not a red line to read twice.
                            match st.error.as_deref().and_then(needs_confirm_from_error) {
                                Some(reason) => {
                                    tab.confirm = Some((tab.ran.clone(), reason));
                                    tab.error = None;
                                }
                                None => tab.error = st.error.clone(),
                            }
                            false
                        }
                        "cancelled" => {
                            tab.state = RunState::Cancelled;
                            false
                        }
                        _ => false,
                    }
                };
                if let Some(&task_id) = self.call_tasks.get(&st.call_id) {
                    if let crate::workspace::Settle::Finished(err) =
                        crate::workspace::task_settled(&st.state, st.error.as_deref())
                    {
                        self.call_tasks.remove(&st.call_id);
                        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task_id) {
                            t.finish(Duration::from_millis(st.ms), err);
                        }
                    }
                }
                if want_page {
                    let call_id = st.call_id;
                    self.ask(
                        Pending::Page { tab: tab_id },
                        "query.page",
                        json!({ "call_id": call_id, "from": 0, "n": PAGE }),
                    );
                }
                // **File it in the history, with a row count that is true.**
                //
                // A WRITE knows its count now: `affected` is what the engine
                // answers for one. A READ does not — `affected` is empty for a
                // select, and this used to file every read with no count at
                // all. A read is filed when its first page arrives, where the
                // total (or the rows actually handed over) is known. A failure
                // and a cancellation are filed here, because no page follows
                // one.
                match st.state.as_str() {
                    "failed" | "cancelled" => self.record_run(tab_id, &st.state, st.ms, None),
                    "done" if st.affected.is_some() => {
                        self.record_run(tab_id, "done", st.ms, st.affected)
                    }
                    _ => {}
                }
            }
            "daemon.log" => {
                let level = params.get("level").and_then(Value::as_str).unwrap_or("info").to_string();
                let message = params.get("message").and_then(Value::as_str).unwrap_or("").to_string();
                self.log_push(&level, message);
            }
            // What somebody's own code PRINTS, as it prints it.
            //
            // A hook's output carries the moment it ran at (`hook`), and it
            // goes in the log for the same reason the engine's own lines do: a
            // PreConnect hook that is about to refuse the connection should be
            // readable WHILE it runs, not only once the connection has failed
            // with a message about an exit code.
            //
            // `stderr` is a WARNING, not an error: a script writing to stderr
            // is a script talking, and only its exit status says whether it
            // went wrong. Colouring every line red would make a chatty hook
            // look like a broken one.
            "script.output" => {
                let text = params.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                if text.trim().is_empty() {
                    return;
                }
                let err = params.get("stream").and_then(Value::as_str) == Some("stderr");
                match params.get("hook").and_then(Value::as_str) {
                    Some(when) => {
                        self.log_push(if err { "warn" } else { "info" }, format!("{when}: {text}"))
                    }
                    None => self.log_push(if err { "warn" } else { "info" }, text),
                }
            }
            _ => {}
        }
    }

    fn on_result(&mut self, what: Pending, v: Value, cx: &mut Context<Self>) {
        match what {
            Pending::StoreStatus => {
                self.store_ok = v.get("available").and_then(Value::as_bool).unwrap_or(false);
                self.store_path = v
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // Warm the saved tunnels as soon as the store answers, so the
                // FIRST connection window already knows about them. Asking when
                // the window opens is too late: the answer arrives after it is
                // drawn, and it drew "no saved tunnels yet" at somebody who has
                // one.
                if self.store_ok {
                    self.ask_profiles();
                }
            }
            Pending::Connections => {
                self.saved =
                    serde_json::from_value(v.get("connections").cloned().unwrap_or(json!([]))).unwrap_or_default();
                // Nothing saved at all: the only useful thing a database client
                // can do on a first run is offer to describe a database. Shown
                // once, and only when the store is working — an unreachable
                // store is a different problem and a form would not fix it.
                if self.saved.is_empty() && self.store_ok && self.form_window.is_none() {
                    self.open_form(None, cx);
                }
                // Reopen what was open. Only now: before the saved list arrived
                // there was nothing to look a name up in.
                let want: Vec<String> = self
                    .restore
                    .connections
                    .iter()
                    .filter(|n| self.saved.iter().any(|s| &&s.name == n))
                    .cloned()
                    .collect();
                for name in want {
                    if !self.conns.iter().any(|c| c.name == name) {
                        self.connect(&name);
                    }
                }
            }
            Pending::Connect { name, driver } => {
                let conn_id = v.get("conn_id").and_then(Value::as_u64).unwrap_or(0);
                self.conns.retain(|c| c.name != name);
                self.conns.push(Live {
                    name: name.clone(),
                    driver,
                    conn_id,
                    encrypted: v.get("encrypted").and_then(Value::as_bool).unwrap_or(false),
                    tunneled: v.get("tunneled").and_then(Value::as_bool).unwrap_or(false),
                    nodes: Vec::new(),
                    queries: Vec::new(),
                    charts: Vec::new(),
                    boards: Vec::new(),
                });
                self.status = format!("connected: {name}");
                // Said once, out loud, at the moment it becomes true. The
                // padlock in the row says it from then on, but a person who
                // just typed a password deserves to be told before they type
                // the next statement.
                if !crate::workspace::link_label(
                    v.get("encrypted").and_then(Value::as_bool).unwrap_or(false),
                    v.get("tunneled").and_then(Value::as_bool).unwrap_or(false),
                )
                .safe
                {
                    let warn = format!("{name}: {}", self.lang.tr("toast.plaintext"));
                    self.toast(warn, true);
                }
                self.remember();
                self.ask(
                    Pending::Dashboards { conn: name.clone() },
                    "store.dashboards.list",
                    json!({ "conn": name }),
                );
                self.ask(
                    Pending::Structure { name },
                    "schema.structure",
                    json!({ "conn_id": conn_id }),
                );
            }
            Pending::Disconnect { name } => {
                self.conns.retain(|c| c.name != name);
                self.status = format!("disconnected: {name}");
                self.remember();
            }
            Pending::Structure { name } => {
                let nodes: Vec<Node> =
                    serde_json::from_value(v.get("nodes").cloned().unwrap_or(json!([]))).unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == name) {
                    c.nodes = nodes;
                }
                self.reopen_tabs_for(&name, cx);
            }
            Pending::Execute { tab, task } => {
                let call_id = v.get("call_id").and_then(Value::as_u64).unwrap_or(0);
                self.calls.insert(call_id, tab);
                self.call_tasks.insert(call_id, task);
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                    t.call_id = Some(call_id);
                }
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.call_id = Some(call_id);
                    t.state = RunState::Running;
                }
            }
            Pending::Page { tab } => {
                if !v.get("ready").and_then(Value::as_bool).unwrap_or(false) {
                    return;
                }
                let Ok(page) = serde_json::from_value::<lode_proto::Page>(v) else { return };
                let mut filing = None;
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.columns = page.columns;
                    t.rows = page.rows;
                    t.from = page.from;
                    t.has_more = page.has_more;
                    t.total = page.total;
                    t.affected = page.affected.or(t.affected);
                    // **What belonged to the OLD result goes.** A sort is over
                    // a column this result may not have; a selected row and an
                    // open detail are indexes into rows that are gone. Left
                    // alone, the grid drew a highlight on whatever row happened
                    // to be in that position, or showed a detail panel of the
                    // previous result's values beside the new ones.
                    t.sort = None;
                    let n = t.rows.len();
                    t.selected = t.selected.filter(|i| *i < n);
                    t.detail = t.detail.filter(|i| *i < n);
                    if !t.recorded {
                        filing = Some((t.ms, t.total.map(|v| v as u64).or(Some(n as u64))));
                    }
                }
                // A read's row count is known HERE and nowhere earlier.
                if let Some((ms, rows)) = filing {
                    self.record_run(tab, "done", ms, rows);
                }
            }
            Pending::Ignore(_) => {}
            Pending::FormTest { stage, form } => {
                let detail = v.get("detail").and_then(Value::as_str).unwrap_or("ok").to_string();
                let ms = v.get("ms").and_then(Value::as_u64).unwrap_or(0);
                let _ = form.update(cx, |f, cx| {
                    f.test_answer(stage, Ok(format!("{detail} ({ms} ms)")), cx)
                });
            }
            Pending::VaultPut { field, name, form } => {
                let _ = form.update(cx, |f, cx| f.stored_in_keyring(&field, &name, cx));
                self.toast(format!("stored as {name}"), false);
            }
            Pending::Metrics { conn } => {
                self.metrics =
                    serde_json::from_value(v.get("metrics").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                self.metrics_conn = Some(conn);
                self.metrics_error = None;
                self.metrics_at = Some(Instant::now());
                self.metrics_inflight = false;
            }
            Pending::History => {
                self.history =
                    serde_json::from_value(v.get("history").cloned().unwrap_or(json!([]))).unwrap_or_default();
            }
            Pending::Audit => {
                self.audit =
                    serde_json::from_value(v.get("audit").cloned().unwrap_or(json!([]))).unwrap_or_default();
                // An engine from before the audit log answers without a mode,
                // and the honest reading of that is that nothing is withheld.
                self.audit_mode = v
                    .get("mode")
                    .and_then(|m| m.as_str())
                    .unwrap_or("full")
                    .to_string();
            }
            Pending::SavedCharts { conn } => {
                let cs: Vec<SavedChart> =
                    serde_json::from_value(v.get("charts").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == conn) {
                    c.charts = cs;
                }
            }
            Pending::Dashboards { conn } => {
                let names: Vec<String> = v
                    .get("dashboards")
                    .and_then(|d| d.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| d.get("name").and_then(Value::as_str).map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == conn) {
                    c.boards = names;
                }
            }
            Pending::Board { tab } => {
                let raw = v.get("dashboard").cloned().unwrap_or(Value::Null);
                let stale = v.get("stale").and_then(Value::as_str).map(str::to_string);
                let columns = v.get("columns").and_then(Value::as_u64).unwrap_or(12) as u32;
                let dash: Dashboard = serde_json::from_value(raw).unwrap_or_default();
                let conn = self.tabs.iter().find(|t| t.id == tab).map(|t| t.conn_name.clone());
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    if let Some(b) = t.board.as_mut() {
                        b.dash = dash;
                        b.stale = stale;
                        b.columns = columns;
                        b.panes.clear();
                        b.dirty = false;
                    }
                }
                // The charts come next, and only then can anything be run: a
                // tile holds a NAME, and the statement behind it lives with the
                // saved chart.
                if let Some(conn) = conn {
                    self.ask(
                        Pending::BoardCharts { tab },
                        "store.charts.list",
                        json!({ "conn": conn }),
                    );
                }
            }
            Pending::BoardCharts { tab } => {
                let cs: Vec<SavedChart> =
                    serde_json::from_value(v.get("charts").cloned().unwrap_or(json!([]))).unwrap_or_default();
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    if let Some(b) = t.board.as_mut() {
                        b.charts = cs;
                    }
                }
                self.run_board(tab);
            }
            Pending::BoardRun { tab, tile } => {
                let call_id = v.get("call_id").and_then(Value::as_u64).unwrap_or(0);
                self.board_calls.insert(call_id, (tab, tile));
                if let Some(p) = self.pane_mut(tab, tile) {
                    p.call_id = Some(call_id);
                    p.state = RunState::Running;
                }
            }
            Pending::BoardPage { tab, tile } => {
                if !v.get("ready").and_then(Value::as_bool).unwrap_or(false) {
                    return;
                }
                let Ok(page) = serde_json::from_value::<lode_proto::Page>(v) else { return };
                if let Some(p) = self.pane_mut(tab, tile) {
                    p.columns = page.columns;
                    p.rows = page.rows;
                }
            }
            Pending::BoardSaved { tab } => {
                // The engine hands back the board it ACTUALLY stored, with the
                // time range canonicalised. Taking that back rather than keeping
                // what was sent is what stops a range reading one way here and
                // another after a reload.
                if let Ok(d) = serde_json::from_value::<Dashboard>(v.get("dashboard").cloned().unwrap_or(Value::Null))
                {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                        if let Some(b) = t.board.as_mut() {
                            b.dash = d;
                            b.dirty = false;
                            b.stale = None;
                        }
                    }
                }
                let conn = self.tabs.iter().find(|t| t.id == tab).map(|t| t.conn_name.clone());
                if let Some(conn) = conn {
                    self.ask(
                        Pending::Dashboards { conn: conn.clone() },
                        "store.dashboards.list",
                        json!({ "conn": conn }),
                    );
                }
                self.toast("dashboard saved", false);
            }
            Pending::SavedQueries { conn } => {
                let qs: Vec<SavedQuery> =
                    serde_json::from_value(v.get("queries").cloned().unwrap_or(json!([]))).unwrap_or_default();
                if let Some(c) = self.conns.iter_mut().find(|c| c.name == conn) {
                    c.queries = qs;
                }
            }
            Pending::Columns { tab } => {
                let cols: Vec<TableColumn> =
                    serde_json::from_value(v.get("columns").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.meta_columns = cols;
                }
            }
            Pending::Indexes { tab } => {
                let idx: Vec<Index> =
                    serde_json::from_value(v.get("indexes").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.indexes = idx;
                }
            }
            Pending::ExportPage => {
                if !v.get("ready").and_then(Value::as_bool).unwrap_or(false) {
                    return;
                }
                if let Ok(page) = serde_json::from_value::<lode_proto::Page>(v) {
                    self.export_page(page, cx);
                }
            }
            Pending::AccessList => {
                self.access =
                    v.get("providers").and_then(|x| x.as_array()).cloned().unwrap_or_default();
                self.poke_settings(cx);
            }
            Pending::Approvals => {
                let was = self.approvals.len();
                self.approvals =
                    v.get("approvals").and_then(|x| x.as_array()).cloned().unwrap_or_default();
                // SAID once, when one arrives. The count in the status line is
                // for afterwards; a thing waiting for a person needs telling
                // the person, not a number they have to notice.
                if self.approvals.len() > was {
                    self.toast(
                        format!("{} statement(s) waiting for your answer", self.approvals.len()),
                        true,
                    );
                }
                self.poke_settings(cx);
            }
            Pending::AccessTest { profile } => {
                // The ENGINE's own words about what it got, not a tick: "it
                // worked" tells nobody which identity they are now using.
                let detail = v
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("resolved")
                    .to_string();
                self.status = format!("{profile}: {detail}");
                self.toast(format!("{profile}: {detail}"), false);
            }
            Pending::AccessImport { profile } => {
                self.toast(format!("imported {profile}"), false);
                // It became a profile; the lists that show profiles are asked
                // again rather than left to be refreshed by hand.
                self.ask_profiles();
            }
            Pending::ScriptNames => {
                self.script_names = v
                    .get("scripts")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.get("name").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
            }
            Pending::StoreImport => {
                let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
                let (c, q, h) = (n("connections"), n("queries"), n("history"));
                self.toast(
                    format!("imported {c} connection(s), {q} quer(ies), {h} history row(s)"),
                    false,
                );
                self.ask(Pending::Connections, "store.connections.list", json!({}));
            }
            Pending::ApprovalDecided => {
                self.ask(Pending::Approvals, "approvals.list", json!({ "state": "pending" }));
            }
            Pending::DdlCopy => {
                // Straight to the clipboard: the menu said "copy the DDL", and
                // opening a tab to show it would be a different answer.
                match v.get("ddl").and_then(Value::as_str) {
                    Some(ddl) if !ddl.trim().is_empty() => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(ddl.to_string()));
                        self.toast("DDL copied".to_string(), false);
                    }
                    _ => self.toast("this driver answered no DDL".to_string(), true),
                }
            }
            Pending::Ddl { tab } => {
                let ddl = v.get("ddl").and_then(|d| d.as_str()).map(str::to_string);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.ddl = ddl;
                }
            }
            Pending::RowPlan { tab, row, key, set, delete } => {
                // The statement held here is the ENGINE's. A client that showed
                // its own guess and then sent something else would be asking
                // about one thing and doing another.
                let statement = v
                    .get("statement")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                let reason = v.get("reason").and_then(|r| r.as_str()).map(str::to_string);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.pending_write = Some(PendingWrite { statement, reason, key, set, delete, row });
                    // The boxes close once the engine has described the write:
                    // what is on screen from here is the PLAN and the question
                    // about it, not the typing that produced it.
                    t.editing = None;
                    t.editing_doc = None;
                }
            }
            Pending::RowApply { tab } => {
                let n = v.get("affected").and_then(|a| a.as_u64()).unwrap_or(0);
                self.toast(format!("{n} row(s) written"), false);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.pending_write = None;
                }
                // Read it back rather than patching the grid from what we sent:
                // a column with a default, a trigger or a type coercion means
                // the stored row is not always the row that was asked for.
                self.run_tab(tab, cx);
            }
            Pending::AwsProfiles => {
                // Names only. Which of them is usable, and why, is the engine's
                // to answer at connect time; a client repeating it here would be
                // a second opinion about somebody else's file.
                self.aws_profiles = v
                    .get("profiles")
                    .and_then(Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|r| r.get("name").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
            }
            Pending::Profiles { kind } => {
                let rows: Vec<ProfileRow> =
                    serde_json::from_value(v.get("profiles").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                // Who depends on each, asked now rather than when somebody
                // presses delete: a warning that arrives after the action is a
                // report of damage.
                for r in &rows {
                    self.ask(
                        Pending::ProfileUsers { name: r.name.clone() },
                        "store.profiles.users",
                        json!({ "kind": kind, "name": r.name }),
                    );
                }
                self.profiles.insert(kind, rows);
                // A connection window already open was built from whatever was
                // cached when it opened — which on a first run is nothing. Hand
                // it the real list now rather than leaving it wrong until it is
                // closed and opened again.
                if kind == "tunnel" {
                    let names = self.profile_names("tunnel");
                    let specs: std::collections::BTreeMap<String, Value> = self
                        .profiles
                        .get("tunnel")
                        .map(|v| v.iter().map(|p| (p.name.clone(), p.spec.clone())).collect())
                        .unwrap_or_default();
                    if let Some(h) = self.form_window.clone() {
                        let _ = h.update(cx, |form, _, cx| {
                            form.profiles_arrived(names, specs, cx);
                        });
                    }
                }
                // And the settings window, which LISTS them: it opens asking
                // for both kinds, so without this it shows "none yet" over a
                // list that arrived a moment later.
                if let Some(h) = self.settings_window.clone() {
                    let (tun, prx) = (self.profile_rows("tunnel"), self.profile_rows("proxy"));
                    let _ = h.update(cx, |w, _, cx| w.profiles_arrived(tun, prx, cx));
                }
            }
            Pending::NodeColumns { key } => {
                let cols: Vec<TableColumn> =
                    serde_json::from_value(v.get("columns").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                self.node_fields.insert(key, cols);
            }
            Pending::NodeIndexes { key } => {
                let idx: Vec<Index> =
                    serde_json::from_value(v.get("indexes").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                self.node_indexes.insert(key, idx);
            }
            Pending::NodeSample { key } => {
                // **The page is NOT asked for here.** `query.execute` answers
                // with a call id the moment the statement is away, and a page
                // asked at that moment gets `{ready: false}` — which this code
                // read as "no rows" and remembered as an empty sample, so the
                // types never appeared and expanding the node again found the
                // empty answer cached. The call is remembered instead, and the
                // page is asked for when `query.state` says `done`.
                if let Some(call_id) = v.get("call_id").and_then(Value::as_u64) {
                    self.sample_calls.insert(call_id, key);
                }
            }
            Pending::NodeSamplePage { key, call_id } => {
                // The engine holds a call's buffer until it is released, and a
                // sample is read once and never paged again.
                self.ask(Pending::Ignore("release"), "query.release", json!({ "call_id": call_id }));
                let cols: Vec<String> = v
                    .get("columns")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.get("name").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let rows: Vec<Vec<Value>> =
                    serde_json::from_value(v.get("rows").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                self.node_sample_cols.insert(key.clone(), cols);
                self.node_sample.insert(key, rows);
            }
            Pending::ConnQueryCount { name } => {
                let n = v.get("queries").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                // Only fills in the question that is already up: an answer that
                // arrives after it was dismissed must not re-open it.
                if let Some((who, count)) = self.confirm_conn_rm.as_mut() {
                    if *who == name {
                        *count = Some(n);
                    }
                }
            }
            Pending::ConnRemoved { name } => {
                self.saved.retain(|c| c.name != name);
                self.ask(Pending::Connections, "store.connections.list", json!({}));
                self.toast(format!("removed connection: {name}"), false);
            }
            Pending::ProfileUsers { name } => {
                let users: Vec<String> =
                    serde_json::from_value(v.get("users").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                self.profile_users.insert(name, users);
                // The settings window reads these off this window as it paints,
                // so it needs telling they are here, nothing more.
                self.poke_settings(cx);
            }
            Pending::ProfileSaved { kind } => {
                // Stored. The window goes and the list is asked for again, so a
                // saved profile is visible at once rather than after a refresh
                // somebody has to know about.
                self.close_profile(cx);
                self.ask(Pending::Profiles { kind }, "store.profiles.list", json!({ "kind": kind }));
                self.toast(format!("{kind} profile saved"), false);
            }
            Pending::ProfileRemoved { kind, name } => {
                // The engine says WHICH connections referred to it. Saying
                // nothing would leave somebody to find out by a failing connect.
                let users: Vec<String> =
                    serde_json::from_value(v.get("was_used_by").cloned().unwrap_or(json!([])))
                        .unwrap_or_default();
                if users.is_empty() {
                    self.toast(format!("removed the {kind} profile {name}"), false);
                } else {
                    let msg = format!(
                        "removed {name}, which {} still refer{} to: {}",
                        if users.len() == 1 { "1 connection" } else { "connections" },
                        if users.len() == 1 { "s" } else { "" },
                        users.join(", ")
                    );
                    self.toast(msg.clone(), true);
                    self.log_push("warn", msg);
                }
                self.ask(Pending::Profiles { kind }, "store.profiles.list", json!({ "kind": kind }));
            }
            Pending::ClassifyTab { tab, statement } => {
                match v.get("reason").and_then(Value::as_str) {
                    // Named a reason: the tab holds it, with the statement, and
                    // waits. Nothing is sent to the database yet.
                    Some(reason) => {
                        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                            t.confirm = Some((statement, reason.to_string()));
                        }
                    }
                    None => self.run_tab_confirmed(tab, &statement, false),
                }
            }
            Pending::Rename { to, driver, spec } => {
                // Renamed, and the engine says what moved with it. Now the spec
                // the window holds goes onto the row under its new name.
                let moved = v.get("moved").cloned().unwrap_or_else(|| json!({}));
                let total: u64 = moved
                    .as_object()
                    .map(|m| m.values().filter_map(Value::as_u64).sum())
                    .unwrap_or(0);
                if total > 0 {
                    self.log_push("info", format!("renamed to {to}; {total} saved items moved with it"),
                    );
                }
                self.ask(
                    Pending::FormSave { rename_from: None },
                    "store.connections.save",
                    json!({ "name": to, "driver": driver, "spec": spec }),
                );
            }
            Pending::FormSave { rename_from } => {
                // Stored under the new name: only now is the old one taken off.
                if let Some(old) = rename_from {
                    self.ask(
                        Pending::Ignore("rename"),
                        "store.connections.remove",
                        json!({ "name": old }),
                    );
                }
                // Now the window goes, and the panel is asked for the list again
                // so the new name and driver are there immediately — a save
                // somebody has to go and refresh to see is a save they are not
                // sure happened.
                self.close_form(cx);
                self.ask(Pending::Connections, "store.connections.list", json!({}));
                self.toast("connection saved".to_string(), false);
            }
            Pending::Classify { conn, statement } => {
                match v.get("reason").and_then(Value::as_str) {
                    // The engine named a reason, so it is held and shown. The
                    // reason is what makes someone stop; a yes/no box would not.
                    Some(reason) => self.confirm = Some((conn, statement, reason.to_string())),
                    None => {
                        self.run_statement(&conn, &statement, cx);
                        self.console.update(cx, |i, cx| i.set_text("", cx));
                    }
                }
            }
        }
    }

    fn on_error(&mut self, what: Pending, e: String, cx: &mut Context<Self>) {
        match what {
            Pending::Execute { tab, task } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.state = RunState::Failed;
                    t.error = Some(e.clone());
                }
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
                    let d = t.duration();
                    t.finish(d, Some(e.clone()));
                }
                // The grid shows the error in full; the toast is so it is seen
                // when the eye is somewhere else.
                self.toast(e.lines().next().unwrap_or("failed").to_string(), true);
            }
            Pending::Page { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.state = RunState::Failed;
                    t.error = Some(e);
                }
            }
            Pending::BoardRun { tab, tile } | Pending::BoardPage { tab, tile } => {
                if let Some(p) = self.pane_mut(tab, tile) {
                    p.state = RunState::Failed;
                    p.error = Some(e);
                }
            }
            Pending::Board { tab } | Pending::BoardCharts { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    if let Some(b) = t.board.as_mut() {
                        b.stale = Some(e.clone());
                    }
                }
                self.log_push("error", e);
            }
            Pending::BoardSaved { .. } => {
                // The engine refuses a board that is wrong and says WHICH tile.
                // It goes to the toast AND to the log, because the toast
                // expires and this is the sentence that explains the refusal.
                self.toast(e.clone(), true);
                self.log_push("error", e);
            }
            Pending::Connect { name, .. } => {
                self.status = format!("could not connect to {name}");
                self.toast(format!("{name}: {e}"), true);
            }
            Pending::Metrics { conn } => {
                // The panel shows the refusal where the numbers would be. A
                // managed service withholding its statistics views is a fact
                // about that server, not a malfunction here.
                self.metrics.clear();
                self.metrics_conn = Some(conn);
                self.metrics_error = Some(e);
                self.metrics_at = Some(Instant::now());
                self.metrics_inflight = false;
            }
            Pending::RowPlan { tab, .. } | Pending::RowApply { tab } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.pending_write = None;
                    t.editing = None;
                    t.editing_doc = None;
                }
                // A refused write is the most important thing on the screen: it
                // is both the reason nothing happened and, usually, the reason
                // it should not have.
                self.toast(e, true);
            }
            // A failed stage is the engine naming the layer that is wrong, so it
            // goes back to the window beside the button that asked.
            Pending::ClassifyTab { tab, .. } => {
                // Could not check: that is not permission to run. The tab says
                // so where the person is looking, rather than in a log nobody
                // reads.
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    t.error = Some(format!("could not check what this statement does: {e}"));
                    t.state = RunState::Failed;
                }
                self.toast(e, true);
            }
            Pending::Rename { .. } => {
                // A refused rename stops here: nothing was saved, nothing was
                // removed, and the window still holds the typing.
                if let Some(h) = self.form_window.clone() {
                    let _ = h.update(cx, |form, _, cx| f_save_failed(form, e.clone(), cx));
                }
                self.toast(e, true);
            }
            Pending::FormSave { .. } => {
                // The window is still open, which is the point: the refusal
                // goes into it, beside the fields that still hold the typing.
                if let Some(h) = self.form_window.clone() {
                    let _ = h.update(cx, |form, _, cx| f_save_failed(form, e.clone(), cx));
                }
                self.toast(e, true);
            }
            Pending::FormTest { stage, form } => {
                let _ = form.update(cx, |f, cx| f.test_answer(stage, Err(e), cx));
            }
            Pending::VaultPut { field, form, .. } => {
                // The field is LEFT as it was, holding the value: a form that
                // had switched to the wallet while the wallet refused it would
                // be a form pointing at a secret that is not there.
                let _ = form.update(cx, |f, cx| f.keyring_refused(&field, &e, cx));
            }
            // A classification that failed is NOT permission to run: the answer
            // was "I do not know", and the whole point is to ask first.
            Pending::Classify { statement, .. } => self.log_push("error", format!("could not check whether this is destructive, so it was not run: {statement} — {e}"),
            ),
            _ => self.log_push("error", e),
        }
    }

    // ── actions ──────────────────────────────────────────────────────────────

    fn connect(&mut self, name: &str) {
        let Some(c) = self.saved.iter().find(|c| c.name == name).cloned() else { return };
        let mut spec = match c.spec {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        spec.insert("driver".into(), json!(c.driver.clone()));
        // The SAVED name goes with the spec so the engine's audit log can say
        // which connection a statement ran on. The engine does not use it to
        // connect; without it the log names the driver and leaves the name
        // empty, which reads as a gap rather than as a wrong answer.
        spec.insert("name".into(), json!(c.name.clone()));
        self.status = format!("connecting to {name}…");
        self.ask(
            Pending::Connect { name: c.name.clone(), driver: c.driver.clone() },
            "conn.connect",
            Value::Object(spec),
        );
    }

    /// May the window close?
    ///
    /// NO while something is still running, and the question goes up instead —
    /// closing the window ends every statement under it, which is a way to
    /// abandon a write by reaching for the corner of a title bar. Answering
    /// "disconnect anyway" and pressing the corner again closes it.
    ///
    /// This is the only moment gpui lets a client say no: `on_app_quit` runs
    /// when the decision has already been taken.
    pub fn may_close(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        let busy: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| t.state == RunState::Running)
            .map(|t| t.title.clone())
            .collect();
        if busy.is_empty() {
            return true;
        }
        // The card names a CONNECTION, so it is the one the running tabs belong
        // to — whichever that is, stopping its statements is the thing being
        // offered.
        let conn = self
            .tabs
            .iter()
            .find(|t| t.state == RunState::Running)
            .map(|t| t.conn_name.clone())
            .unwrap_or_default();
        self.confirm_disconnect = Some((conn, busy));
        cx.notify();
        false
    }

    /// Close a connection — ASKING first if something of its own is still
    /// running.
    ///
    /// A disconnect used to go straight out. The engine drops the connection,
    /// which kills the statement under it, so a click meant to tidy up could
    /// end a write halfway: one of those three things is what somebody wanted
    /// and the window had no way to find out which.
    fn disconnect(&mut self, name: &str) {
        let busy: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| t.conn_name == name && t.state == RunState::Running)
            .map(|t| t.title.clone())
            .collect();
        if !busy.is_empty() {
            self.confirm_disconnect = Some((name.to_string(), busy));
            return;
        }
        self.disconnect_now(name);
    }

    fn disconnect_now(&mut self, name: &str) {
        let Some(live) = self.live(name) else { return };
        let (conn_id, name) = (live.conn_id, live.name.clone());
        self.ask(
            Pending::Disconnect { name },
            "conn.disconnect",
            json!({ "conn_id": conn_id }),
        );
    }

    /// Stop whatever this connection is running, and stay connected.
    fn cancel_running_on(&mut self, name: &str) {
        let calls: Vec<u64> = self
            .tabs
            .iter()
            .filter(|t| t.conn_name == name && t.state == RunState::Running)
            .filter_map(|t| t.call_id)
            .collect();
        for call_id in calls {
            self.ask(Pending::Ignore("cancel"), "query.cancel", json!({ "call_id": call_id }));
        }
    }

    /// Open a tab on an object and RUN it. This is the whole point of the
    /// client: the rows appear on a click, with no statement typed and none
    /// invented here — `browse_for` is the driver's own template.
    fn open_object(
        &mut self,
        conn_name: &str,
        name: &str,
        schema: Option<&str>,
        kind: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(live) = self.live(conn_name) else { return };
        // ALREADY OPEN? Then go to it. Clicking a table four times made four
        // tabs of it, and three of them held a result nobody had re-run.
        if let Some(at) = crate::workspace::find_tab_for(&self.tabs, conn_name, name, schema) {
            self.go_tab_at(at, cx);
            return;
        }
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let template = meta
            .as_ref()
            .map(|m| m.browse_for(kind).to_string())
            .unwrap_or_default();
        if template.is_empty() {
            self.log_push("warn", format!("the {driver} driver declares no browse statement (engine older than protocol 3)"),
            );
            return;
        }
        let title = match schema.filter(|s| !s.is_empty()) {
            Some(s) => format!("{s}.{name}"),
            None => name.to_string(),
        };
        let id = self.next_tab;
        self.next_tab += 1;
        let mut tab = Tab::new(id, conn_name.to_string(), driver, title, meta.as_ref());
        tab.object = Some((name.to_string(), schema.map(str::to_string)));
        tab.kind = kind.to_string();
        // The browse template wins over the structured form for the first run:
        // a click asks to SEE the object, and the form is for narrowing it.
        tab.sql = lode_proto::render_template(&template, name, schema, PAGE, &[]);
        let statement = tab.sql.clone();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.make_inputs(id, meta.as_ref(), &statement, cx);
        // What the object IS, asked once per tab. The identifying key comes
        // from here and nowhere else — without it a row cannot be addressed,
        // and a client that guessed a key from the columns it happened to get
        // back is a client that writes the wrong row.
        let objref = json!({ "name": name, "schema": schema });
        if let Some(conn_id) = self.live(conn_name).map(|l| l.conn_id) {
            self.ask(
                Pending::Columns { tab: id },
                "schema.columns",
                json!({ "conn_id": conn_id, "object": objref }),
            );
            if meta.as_ref().map_or(false, |m| m.caps.indexes) {
                self.ask(
                    Pending::Indexes { tab: id },
                    "schema.indexes",
                    json!({ "conn_id": conn_id, "object": objref }),
                );
            }
            if meta.as_ref().map_or(false, |m| m.caps.ddl) {
                self.ask(
                    Pending::Ddl { tab: id },
                    "schema.ddl",
                    json!({ "conn_id": conn_id, "object": objref }),
                );
            }
        }
        self.run_tab(id, cx);
        self.remember();
    }

    /// Build the query bar for a tab: the driver's declared fields where it has
    /// them, a statement box where it does not. Created once per tab, because an
    /// entity made during a render would be made again every frame.
    fn make_inputs(
        &mut self,
        tab_id: u64,
        meta: Option<&DriverMeta>,
        statement: &str,
        cx: &mut Context<Self>,
    ) {
        let colors = input_colors(&self.theme);
        let structured = meta.map(|m| m.is_structured()).unwrap_or(false);
        for f in meta.map(|m| m.browse_form.clone()).unwrap_or_default() {
            let (ph, def) = (f.placeholder.clone(), f.default.clone());
            let input = cx.new(|cx| {
                let mut i = TextInput::new(cx, &ph, colors);
                if !def.is_empty() {
                    i.set_text(def.clone(), cx);
                }
                i
            });
            self.inputs.insert((tab_id, format!("browse:{}", f.key)), input);
        }
        if structured {
            for f in &meta.unwrap().query_form {
                let (ph, default) = (f.placeholder.clone(), f.default.clone());
                let input = cx.new(|cx| {
                    let mut i = TextInput::new(cx, ph, colors);
                    i.set_text(default, cx);
                    i
                });
                self.subscribe_run(&input, tab_id, cx);
                self.inputs.insert((tab_id, f.key.clone()), input);
            }
        } else {
            self.make_editor(tab_id, meta.map(|m| m.hint.as_str()), statement, cx);
        }
    }

    /// Give a tab a MULTI-LINE statement editor.
    ///
    /// One place, because there are two ways a tab gets one — the tree opening
    /// an object and a statement run by name — and when they were two pieces of
    /// code one of them got the editor and the other kept a one-line field, so
    /// half the tabs had no statement box drawn at all.
    fn make_editor(
        &mut self,
        tab_id: u64,
        hint: Option<&str>,
        statement: &str,
        cx: &mut Context<Self>,
    ) {
        // The language, from the driver's own HINT. Where it names none this
        // draws in one ink rather than guessing: a SQL palette over something
        // that is not SQL is a lie that reads as a bug.
        let hint_lower = hint.unwrap_or_default().to_ascii_lowercase();
        let flavour = if hint_lower.contains("sql") {
            crate::editor_el::Flavour::Sql
        } else if hint_lower.contains("json") || hint_lower.contains("mongo") {
            crate::editor_el::Flavour::Json
        } else {
            crate::editor_el::Flavour::Plain
        };
        let (theme, scale, font) = (self.theme.clone(), self.scale, self.font_family.clone());
        let colors = input_colors(&self.theme);
        let text = statement.to_string();
        let ed = cx.new(|cx| {
            crate::editor_el::Editor::new(&text, flavour, colors, theme, scale, font, cx)
        });
        cx.subscribe(&ed, move |this, ed, ev: &crate::editor_el::EditorEvent, cx| {
            match ev {
                // The draft is kept in the TAB, because the session keeps the
                // tab: typing and then closing the window must not lose it.
                crate::editor_el::EditorEvent::Changed => {
                    let text = ed.read(cx).text();
                    if let Some(tb) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tb.sql = text;
                    }
                    this.remember();
                }
                crate::editor_el::EditorEvent::Run => {
                    this.sync_tab_from_inputs(tab_id, cx);
                    this.run_tab(tab_id, cx);
                }
                crate::editor_el::EditorEvent::RunNewTab => {
                    this.run_in_new_tab(tab_id, cx);
                }
                // Escape in the editor with nothing of its own in front: the
                // keyboard comes back to the window, where the grid's keys are.
                crate::editor_el::EditorEvent::Escaped => {
                    this.focus_back = true;
                }
                // The editor asks what the word under its cursor could be; the
                // schema is here, so the answer is.
                crate::editor_el::EditorEvent::WordAt(word) => {
                    let list = this.complete_word(tab_id, word, cx);
                    let word = word.clone();
                    ed.update(cx, |e, cx| e.set_candidates(&word, list, cx));
                }
            }
            cx.notify();
        })
        .detach();
        self.editors.insert(tab_id, ed);
    }

    /// Open or shut a table's insides, asking for them the first time only.
    ///
    /// On EXPAND, not on connect. He has thirty-nine collections, and asking
    /// for the fields and the indexes of every one when a connection opens
    /// would be seventy-eight requests before anything has been looked at.
    fn toggle_inside(
        &mut self,
        key: &str,
        conn: &str,
        name: &str,
        schema: Option<&str>,
        kind: &str,
    ) {
        if self.open_nodes.contains(key) {
            self.open_nodes.remove(key);
            return;
        }
        self.open_nodes.insert(key.to_string());
        if !self.node_asked.insert(key.to_string()) {
            return;
        }
        let Some(conn_id) = self.live(conn).map(|l| l.conn_id) else { return };
        let objref = json!({ "name": name, "schema": schema });
        self.ask(
            Pending::NodeColumns { key: key.to_string() },
            "schema.columns",
            json!({ "conn_id": conn_id, "object": objref }),
        );
        // Only when the driver says it can. Asking a driver that cannot is a
        // refusal in the log for something nobody requested.
        let has = self
            .live(conn)
            .and_then(|l| self.meta(&l.driver))
            .map_or(false, |m| m.caps.indexes);
        if has {
            self.ask(
                Pending::NodeIndexes { key: key.to_string() },
                "schema.indexes",
                json!({ "conn_id": conn_id, "object": objref }),
            );
        } else {
            self.node_indexes.insert(key.to_string(), Vec::new());
        }
        // A driver that declares no column types has only the documents to go
        // on, so a small sample is read — once, on expand, and remembered.
        let declares = self
            .live(conn)
            .and_then(|l| self.meta(&l.driver))
            .map_or(true, |m| m.is_structured() || m.caps.sql);
        if !declares {
            let Some(meta) = self.live(conn).and_then(|l| self.meta(&l.driver)) else { return };
            let sql = lode_proto::render_full(
                meta.browse_for(kind),
                name,
                schema,
                NODE_SAMPLE,
                &[],
                &[],
                "",
            );
            self.ask(
                Pending::NodeSample { key: key.to_string() },
                "query.execute",
                json!({ "conn_id": conn_id, "statement": sql }),
            );
        }
    }

    /// Put a name where text is being written.
    ///
    /// Clicking a field in the schema tree inserts its NAME into the active
    /// tab's statement, rather than opening a panel about it. The details — the
    /// type, whether it is a key — belong on the row where they can be read
    /// without a click; the name is the thing somebody is about to type, and it
    /// is the one thing a click can save them. It is also what clicking a saved
    /// query or a history line already does: it puts text where text goes.
    fn insert_into_editor(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.toast(format!("{name} — no tab to put it in"), true);
            return;
        };
        let tab_id = tab.id;
        // The free-text statement field. A structured driver has named boxes
        // instead, and guessing which of them a column name belongs in would be
        // a guess about somebody's query.
        let Some(ed) = self.editors.get(&tab_id).cloned() else {
            self.toast(format!("{name} — this tab has no statement to add to"), true);
            return;
        };
        let add = name.to_string();
        ed.update(cx, |e, cx| {
            // AT THE CURSOR, not at the end. The field could only be appended
            // to, so a column clicked while the cursor sat in the middle of a
            // SELECT list arrived after the FROM.
            let before = e.doc.text_before_cursor();
            let sep = match before.chars().last() {
                None | Some(' ') | Some('(') | Some(',') | Some('\n') => "",
                _ => " ",
            };
            e.doc.insert(&format!("{sep}{add}"));
            cx.notify();
        });
        let text = ed.read(cx).text();
        if let Some(tb) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            tb.sql = text;
        }
        cx.notify();
    }

    /// Enter in any field of a tab's bar runs that tab.
    fn subscribe_run(&self, input: &Entity<TextInput>, tab_id: u64, cx: &mut Context<Self>) {
        cx.subscribe(input, move |this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Submit) {
                this.sync_tab_from_inputs(tab_id, cx);
                this.run_tab(tab_id, cx);
            }
            cx.notify();
        })
        .detach();
    }

    /// Copy what was typed into the tab, then rebuild its statement. For a
    /// structured driver the statement is assembled from the engine's own
    /// template; for a typed one it IS what was typed.
    fn sync_tab_from_inputs(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let driver = self.tabs[idx].driver.clone();
        let meta = self.meta(&driver).cloned();
        let structured = meta.as_ref().map(|m| m.is_structured()).unwrap_or(false);
        if structured {
            let keys: Vec<String> = meta
                .as_ref()
                .unwrap()
                .query_form
                .iter()
                .map(|f| f.key.clone())
                .collect();
            for k in keys {
                if let Some(i) = self.inputs.get(&(tab_id, k.clone())) {
                    let v = i.read(cx).text().to_string();
                    self.tabs[idx].fields.insert(k, v);
                }
            }
            self.tabs[idx].sql = self.tabs[idx].statement(meta.as_ref(), PAGE);
        } else if let Some(ed) = self.editors.get(&tab_id).cloned() {
            let typed = ed.read(cx).text();
            // A browse with narrowing fields: read them, and rewrite the box
            // ONLY if nobody has typed in it since we last filled it.
            let browse_keys: Vec<String> = meta
                .as_ref()
                .map(|m| m.browse_form.iter().map(|f| f.key.clone()).collect())
                .unwrap_or_default();
            let object = self.tabs[idx].object.clone();
            if let (Some(m), Some((name, schema)), false) =
                (meta.as_ref(), object, browse_keys.is_empty())
            {
                for k in &browse_keys {
                    if let Some(f) = self.inputs.get(&(tab_id, format!("browse:{k}"))) {
                        let v = f.read(cx).text().to_string();
                        self.tabs[idx].fields.insert(k.clone(), v);
                    }
                }
                let kind = self.tabs[idx].kind.clone();
                let fresh = lode_proto::render_browse(
                    m,
                    &kind,
                    &self.tabs[idx].fields,
                    &name,
                    schema.as_deref(),
                    PAGE,
                );
                if typed == self.tabs[idx].rendered || typed.trim().is_empty() {
                    self.tabs[idx].sql = fresh.clone();
                    self.tabs[idx].rendered = fresh.clone();
                    ed.update(cx, |e, cx| e.set_text(&fresh, cx));
                    return;
                }
            }
            self.tabs[idx].sql = typed;
        }
    }

    /// Ctrl-Shift-Enter: the same statement, in a tab of its own.
    ///
    /// Keeping the result you have while running a variant is most of what two
    /// tabs are FOR, and doing it by hand means opening a tab and retyping.
    fn run_in_new_tab(&mut self, from: u64, cx: &mut Context<Self>) {
        self.sync_tab_from_inputs(from, cx);
        let Some(src) = self.tabs.iter().find(|t| t.id == from) else { return };
        let (conn, driver) = (src.conn_name.clone(), src.driver.clone());
        let sql = self
            .editors
            .get(&from)
            .map(|e| e.read(cx).to_run())
            .unwrap_or_else(|| src.sql.clone());
        if sql.trim().is_empty() {
            return;
        }
        let title = crate::workspace::draft_title(&sql);
        let _ = driver;
        self.run_statement_titled(&conn, &title, &sql, false, cx);
    }

    /// Run a tab — **after asking the engine what the statement means.**
    ///
    /// The console has always classified first; a tab ran straight away, so the
    /// same `DROP` typed into the tab's box went through without a word. Only
    /// the engine knows what a statement does in that driver's language, so it
    /// is asked, and a named reason stops the tab the way it stops the console.
    fn run_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        self.tabs[idx].confirm = None;
        // **The SELECTION, if there is one.** A file of several statements is
        // only usable this way, and it is the one thing about a query editor
        // that cannot be worked around by retyping. `to_run` falls back to the
        // whole text, so a tab with nothing selected behaves as before.
        let statement = match self.editors.get(&tab_id) {
            Some(ed) => ed.read(cx).to_run(),
            None => self.tabs[idx].sql.clone(),
        };
        if statement.trim().is_empty() {
            return;
        }
        let driver = self.tabs[idx].driver.clone();
        self.ask(
            Pending::ClassifyTab { tab: tab_id, statement: statement.clone() },
            "query.classify",
            json!({ "driver": driver, "statement": statement }),
        );
    }

    /// Run a tab, saying whether the person has already been asked.
    ///
    /// The `confirm` goes to the ENGINE, which is where the policy lives. This
    /// client asks first anyway; the flag is how it reports that it did, and the
    /// engine refuses when it did not.
    fn run_tab_confirmed(&mut self, tab_id: u64, statement: &str, confirm: bool) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let (conn_name, title) = (self.tabs[idx].conn_name.clone(), self.tabs[idx].title.clone());
        let Some(live) = self.live(&conn_name) else {
            self.log_push("warn", format!("{conn_name} is not connected"));
            return;
        };
        let conn_id = live.conn_id;
        // The statement is PASSED, not re-read from the tab: with a selection
        // the tab holds the whole text and only part of it is being run, and
        // classifying one statement and executing another is exactly the gap
        // the confirmation exists to close.
        let statement = statement.to_string();
        if statement.trim().is_empty() {
            return;
        }
        // A tab whose result is being exported does not start another: the new
        // run would take the tab's `call_id` and the old call would be let go
        // from under the export.
        if self.exporting.as_ref().map(|e| e.tab) == Some(tab_id) {
            self.toast(
                "an export of this result is running — it has to finish first".to_string(),
                true,
            );
            return;
        }
        self.tabs[idx].ran = statement.clone();
        let old = self.tabs[idx].call_id.take();
        let old_running = self.tabs[idx].state == RunState::Running;
        self.tabs[idx].reset_result();

        if let Some(old) = old {
            self.let_go_of_call(old, old_running);
        }
        let task_id = self.next_task;
        self.next_task += 1;
        self.tasks.push(Task::new(task_id, format!("{conn_name} · {title}")));
        if self.tasks.len() > 100 {
            self.tasks.remove(0);
        }
        self.ask(
            Pending::Execute { tab: tab_id, task: task_id },
            "query.execute",
            json!({ "conn_id": conn_id, "statement": statement, "confirm": confirm }),
        );
    }

    /// Reopen the remembered tabs that belong to a connection that has just
    /// come back. Taken from `restore` as they are opened, so a reconnect later
    /// in the session does not open them a second time.
    fn reopen_tabs_for(&mut self, conn: &str, cx: &mut Context<Self>) {
        let mine: Vec<SessionTab> = self
            .restore
            .tabs
            .iter()
            .filter(|t| t.conn == conn)
            .cloned()
            .collect();
        self.restore.tabs.retain(|t| t.conn != conn);
        for t in mine {
            // A dashboard tab. It comes back by NAME and reloads its layout
            // from the engine rather than from the session file, so a board
            // rearranged in the editor is the board that reopens here.
            if t.kind == "dashboard" {
                self.open_board(&t.conn, &t.name, cx);
                continue;
            }
            if t.name.is_empty() {
                // A typed tab. It comes back with its words in the box and
                // NOTHING run: a statement someone was halfway through writing
                // is not a statement they asked for, and re-running it on a
                // restart is how a client deletes a table nobody asked it to.
                let title = if t.title.is_empty() { "Draft".into() } else { t.title.clone() };
                self.open_draft(&t.conn, &title, &t.draft, cx);
                continue;
            }
            self.open_object(&t.conn, &t.name, t.schema.as_deref(), &t.kind, cx);
            if let Some(tab) = self.tabs.last_mut() {
                if !t.view.is_empty() {
                    tab.view = match t.view.as_str() {
                        "tree" => ResultView::Tree,
                        "json" => ResultView::Json,
                        "chart" => ResultView::Chart,
                        _ => ResultView::Table,
                    };
                }
                tab.sub = match t.sub.as_str() {
                    "schema" => SubTab::Schema,
                    "aggregate" => SubTab::Aggregate,
                    "builder" => SubTab::Builder,
                    _ => SubTab::Documents,
                };
                tab.detail = t.detail;
            }
        }
        if self.restore.active < self.tabs.len() {
            self.active = self.restore.active;
        }
    }

    /// Open a tab holding a statement, without running it.
    fn open_draft(&mut self, conn: &str, title: &str, sql: &str, cx: &mut Context<Self>) {
        let Some(live) = self.live(conn) else { return };
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let id = self.next_tab;
        self.next_tab += 1;
        let mut tab = Tab::new(id, conn.to_string(), driver, title.to_string(), meta.as_ref());
        tab.sql = sql.to_string();
        self.make_inputs(id, meta.as_ref(), sql, cx);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    /// Remember what is open. Called after anything that changes it; writing a
    /// small file on each change is cheaper than deciding when to.
    /// Say that something worth remembering changed.
    ///
    /// It does NOT write. Fourteen places call this, several of them on a
    /// keystroke or a click — paging a result, dragging the panel's edge,
    /// switching tabs — and each call serialized the whole session and wrote it
    /// to disk. The write happens from `pump`, at most once every 500 ms, so a
    /// burst of changes costs one file write instead of one per change.
    fn remember(&mut self) {
        self.session_dirty = true;
    }

    /// Write the session if it is dirty and enough time has passed.
    fn flush_session(&mut self) {
        if !self.session_dirty {
            return;
        }
        if let Some(last) = self.session_written {
            if last.elapsed() < Duration::from_millis(500) {
                return;
            }
        }
        self.session_dirty = false;
        self.session_written = Some(Instant::now());
        self.write_session();
    }

    fn write_session(&self) {
        session::save(&Session {
            connections: self.conns.iter().map(|c| c.name.clone()).collect(),
            tabs: self
                .tabs
                .iter()
                .map(|t| {
                    let (name, schema) = t.object.clone().unzip();
                    // A dashboard tab is remembered by NAME and nothing else.
                    // The layout is the engine's, and writing a copy of it here
                    // would be a second place for it to live and a second place
                    // for it to be out of date.
                    if let Some(b) = t.board.as_ref() {
                        return SessionTab {
                            conn: t.conn_name.clone(),
                            name: b.name.clone(),
                            schema: None,
                            kind: "dashboard".into(),
                            view: String::new(),
                            title: t.title.clone(),
                            draft: String::new(),
                            sub: String::new(),
                            detail: None,
                        };
                    }
                    SessionTab {
                        conn: t.conn_name.clone(),
                        name: name.unwrap_or_default(),
                        schema: schema.flatten(),
                        kind: t.kind.clone(),
                        view: t.view.label().to_lowercase(),
                        title: t.title.clone(),
                        draft: t.sql.clone(),
                        sub: t.sub.label().to_lowercase(),
                        detail: t.detail,
                    }
                })
                .collect(),
            active: self.active,
            theme: Some(self.theme_name.clone()),
            json_indent: Some(self.json_indent),
            panel_hidden: Some(self.panel_hidden).filter(|x| *x),
            shut_groups: Some(self.shut_groups.iter().cloned().collect())
                .filter(|v: &Vec<String>| !v.is_empty()),
            keys: Some(self.key_over.clone()).filter(|m| !m.is_empty()),
            // Nothing opens or closes a settings group any more — the panel
            // became a window with categories. The field stays in the file's
            // shape so a session written by an older build still parses.
            settings_open: None,
            lang: Some(self.lang.tag().into()),
            // `None` when it is the default, so a later change to the default
            // reaches anybody who has not picked a size of their own. Writing
            // the number out unconditionally is what froze everyone at the old
            // one.
            font_size: Some(self.scale.base()).filter(|v| *v != crate::metrics::DEFAULT_BASE),
            icon_size: Some(self.scale.icon_size())
                .filter(|v| *v != crate::metrics::DEFAULT_ICON),
            panel_w: self.scale.panel_set(),
            rail: match self.rail {
                Rail::Connections => "connections",
                Rail::Instance => "instance",
                Rail::History => "history",
                Rail::Audit => "audit",
                Rail::Tasks => "tasks",
            }
            .into(),
        });
    }

    fn close_tab(&mut self, id: u64) {
        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
            if let Some(call) = self.tabs[pos].call_id {
                // A statement whose tab is gone has nobody waiting for it, and
                // it is still holding a connection on the server. Closing the
                // tab stops it.
                let running = self.tabs[pos].state == RunState::Running;
                self.let_go_of_call(call, running);
            }
            self.tabs.remove(pos);
            self.inputs.retain(|(t, _), _| *t != id);
            self.editors.remove(&id);
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
            self.remember();
        }
    }

    /// Move the active tab's window by `pages`. The engine buffers the result,
    /// so paging backwards costs nothing and re-runs nothing.
    fn page_by(&mut self, pages: i64) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(call_id) = tab.call_id else { return };
        if tab.state != RunState::Done {
            return;
        }
        let from = if pages >= 0 {
            if !tab.has_more {
                return;
            }
            tab.from + PAGE * pages as usize
        } else {
            tab.from.saturating_sub(PAGE * (-pages) as usize)
        };
        let tab_id = tab.id;
        self.ask(
            Pending::Page { tab: tab_id },
            "query.page",
            json!({ "call_id": call_id, "from": from, "n": PAGE }),
        );
    }

    /// A console line. It is NOT run yet: first the engine is asked whether it
    /// destroys anything, because only the engine knows what the statement means
    /// in that driver's language.
    fn console_submit(&mut self, line: String, _cx: &mut Context<Self>) {
        let Some(conn) = self.target_conn() else {
            self.log_push("warn", "nothing is connected".into());
            return;
        };
        let Some(live) = self.live(&conn) else { return };
        let driver = live.driver.clone();
        self.ask(
            Pending::Classify { conn, statement: line.clone() },
            "query.classify",
            json!({ "driver": driver, "statement": line }),
        );
    }

    fn run_statement(&mut self, conn: &str, statement: &str, cx: &mut Context<Self>) {
        let title: String = statement.chars().take(40).collect();
        self.run_statement_titled(conn, &title, statement, false, cx);
    }

    /// The same, for a statement the person has just been asked about and
    /// approved. The flag travels to the engine, which is what actually decides.
    fn run_statement_approved(&mut self, conn: &str, statement: &str, cx: &mut Context<Self>) {
        let title: String = statement.chars().take(40).collect();
        self.run_statement_titled(conn, &title, statement, true, cx);
    }

    /// Run a statement at a connection, as its own tab — so it gets the same
    /// grid, views, paging and task entry everything else gets.
    fn run_statement_titled(
        &mut self,
        conn: &str,
        title: &str,
        statement: &str,
        confirm: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(live) = self.live(conn) else { return };
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let id = self.next_tab;
        self.next_tab += 1;
        let mut tab = Tab::new(id, conn.to_string(), driver, title.to_string(), meta.as_ref());
        tab.sql = statement.to_string();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        // A console statement is free text whatever the driver is: it was typed,
        // not assembled, so the tab gets an editor rather than a form.
        let hint = meta.as_ref().map(|m| m.hint.clone()).unwrap_or_default();
        self.make_editor(id, Some(&hint), statement, cx);
        // The approval travels with the run. Without it the engine refuses a
        // destructive statement that the person has just been asked about and
        // said yes to — the console would look broken, correctly but uselessly.
        let stmt = statement.to_string();
        self.run_tab_confirmed(id, &stmt, confirm);
    }

    /// Open the connection window. Editing passes what is stored; adding passes
    /// nothing and the driver's own defaults fill the fields.
    fn open_form(&mut self, existing: Option<(String, String, Value)>, cx: &mut Context<Self>) {
        // Already open: nothing is gained by a second one, and two would fight
        // over the same saved connection.
        if self.form_window.is_some() {
            return;
        }
        let drivers = self.hello.as_ref().map(|h| h.drivers.clone()).unwrap_or_default();
        let tunnel_profiles = self.profile_names("tunnel");
        let tunnel_profile_specs: std::collections::BTreeMap<String, Value> = self
            .profiles
            .get("tunnel")
            .map(|v| v.iter().map(|p| (p.name.clone(), p.spec.clone())).collect())
            .unwrap_or_default();
        let proxy_profiles = self.profile_names("proxy");
        // The saved scripts a hook can name. Asked for on the way in, so a
        // script saved since the last window is offered by this one.
        let scripts = self.script_names.clone();
        if let Some((n, _, _)) = &existing {
            self.ask(
                Pending::ScriptNames,
                "store.scripts.list",
                json!({ "conn": n }),
            );
        }
        let aws_profiles = self.aws_profiles.clone();
        // Asked on the way in, so a profile added to the file since the last
        // window is offered by this one.
        self.ask(Pending::AwsProfiles, "access.list", json!({}));
        // Asked again on the way in, so a window opened right after one was
        // saved elsewhere still lists it.
        self.ask_profiles();
        let me = cx.entity().downgrade();
        let title = match &existing {
            Some((_, d, _)) => format!("lode — Edit {d} connection"),
            None => "lode — New connection".to_string(),
        };
        let look = self.look();
        // PUSHED, like everything else this window is given: it cannot read the
        // workspace while the workspace is drawing it. See the guardrail test.
        let proto = self.hello.as_ref().map_or(0, |h| h.proto);
        let (w, h) = self.scale.window(crate::metrics::Window::Connection);
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                cx.new(|cx| {
                    ConnectionForm::new(
                        me,
                        look,
                        proto,
                        drivers,
                        tunnel_profiles,
                        tunnel_profile_specs,
                        proxy_profiles,
                        scripts,
                        aws_profiles,
                        existing,
                        cx,
                    )
                })
            },
        );
        match opened {
            Ok(h) => self.form_window = Some(h),
            Err(e) => self.log_push("error", format!("could not open the window: {e}")),
        }
    }

    /// Open the window for ONE saved tunnel, new or existing.
    ///
    /// Its own window: a profile is a host, a port, a user, a method and a key,
    /// and the connection window would make somebody fill in a driver and a
    /// database the profile has no use for. That is why "save over the same
    /// name" was never editing — it was re-creating.
    pub fn open_profile(
        &mut self,
        kind: &'static str,
        existing: Option<(String, Value)>,
        cx: &mut Context<Self>,
    ) {
        if self.profile_window.is_some() {
            return;
        }
        // Who depends on it, asked BEFORE the window opens, so the window can
        // say so before anything is changed rather than after.
        if let Some((name, _)) = &existing {
            self.ask(
                Pending::ProfileUsers { name: name.clone() },
                "store.profiles.users",
                json!({ "kind": kind, "name": name }),
            );
        }
        let users = existing
            .as_ref()
            .map(|(n, _)| self.profile_users.get(n).cloned().unwrap_or_default())
            .unwrap_or_default();
        let me = cx.entity().downgrade();
        let title = match &existing {
            Some((n, _)) => format!("lode — {kind} {n}"),
            None => format!("lode — New {kind}"),
        };
        let look = self.look();
        let (w, h) = self.scale.window(crate::metrics::Window::Profile);
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some(title.into()), ..Default::default() }),
                ..Default::default()
            },
            |_window, cx| {
                cx.new(|cx| {
                    crate::profile::ProfileForm::new(me, look, kind, existing, users, cx)
                })
            },
        );
        match opened {
            Ok(h) => self.profile_window = Some(h),
            Err(e) => self.log_push("error", format!("could not open the window: {e}")),
        }
    }

    /// Open the settings WINDOW.
    ///
    /// Everything it needs is handed over, so the window has no engine of its
    /// own — the same arrangement as the connection and tunnel windows.
    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_window.is_some() {
            return;
        }
        self.ask_profiles();
        let drivers = self.hello.as_ref().map(|h| h.drivers.clone()).unwrap_or_default();
        let prof = |kind: &str| -> Vec<ProfileRow> {
            self.profiles.get(kind).cloned().unwrap_or_default()
        };
        let about: Vec<(String, String)> = self
            .hello
            .as_ref()
            .map(|h| {
                vec![
                    ("Protocol".into(), h.proto.to_string()),
                    ("Drivers".into(), h.drivers.len().to_string()),
                    ("Theme".into(), self.theme_name.clone()),
                    ("Themes found".into(), self.themes.len().to_string()),
                    ("Text font".into(), self.font_family.clone()),
                    ("Icon font".into(), self.icon_family.clone()),
                ]
            })
            .unwrap_or_default();
        let me = cx.entity().downgrade();
        let (tun, prx) = (prof("tunnel"), prof("proxy"));
        let look = self.look();
        // Taken HERE rather than read by the window as it draws: `open_window`
        // draws once before it returns, and that draw happens inside this
        // listener's update of the workspace — where reading the workspace is a
        // panic. See `settings::Appearance`.
        let look_now = self.appearance();
        let facts = self.facts();
        // Both cost a round trip and neither is wanted until somebody looks at
        // the page that reports it — so they are asked for HERE, when the
        // window opens, rather than at startup.
        if self.hello.as_ref().map_or(false, |h| h.proto >= 11) {
            self.ask(Pending::AccessList, "access.list", json!({}));
        }
        if self.hello.as_ref().map_or(false, |h| h.proto >= 12) {
            self.ask(Pending::Approvals, "approvals.list", json!({}));
        }
        let users: std::collections::BTreeMap<String, Vec<String>> = self
            .profile_users
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // Derived: at a large text size a window of 900×620 held half a page
        // and put its own Close button off the bottom edge.
        let (w, h) = self.scale.window(crate::metrics::Window::Settings);
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("lode — Settings".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| {
                cx.new(|cx| {
                    crate::settings::SettingsWindow::new(
                        me, look, drivers, tun, prx, about, look_now, users, facts, cx,
                    )
                })
            },
        );
        match opened {
            Ok(h) => self.settings_window = Some(h),
            Err(e) => self.log_push("error", format!("could not open the window: {e}")),
        }
    }

    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.settings_window.take() {
            let _ = h.update(cx, |_, window, _| window.remove_window());
        }
        cx.notify();
    }

    pub fn close_profile(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.profile_window.take() {
            let _ = h.update(cx, |_, window, _| window.remove_window());
        }
        cx.notify();
    }

    pub fn close_form(&mut self, cx: &mut Context<Self>) {
        if let Some(h) = self.form_window.take() {
            let _ = h.update(cx, |_, window, _| window.remove_window());
        }
        cx.notify();
    }

    /// Run one `conn.test` stage for the connection window and send the answer
    /// back to it. The engine's message goes across untouched.
    pub fn form_test(
        &mut self,
        stage: &'static str,
        spec: Value,
        form: WeakEntity<ConnectionForm>,
        _cx: &mut Context<Self>,
    ) {
        self.ask(
            Pending::FormTest { stage, form },
            "conn.test",
            json!({ "stage": stage, "spec": spec }),
        );
    }

    /// Move a secret the connection window is holding into the wallet.
    ///
    /// The value goes out on the engine's stdin, which is the same path a
    /// literal credential already takes on `conn.connect` — it does not touch
    /// the store on the way, and the engine does not keep it.
    pub fn vault_put(
        &mut self,
        field: String,
        name: String,
        value: String,
        form: WeakEntity<ConnectionForm>,
    ) {
        self.ask(
            Pending::VaultPut { field, name: name.clone(), form },
            "vault.put",
            json!({ "name": name, "value": value }),
        );
    }

    /// Ask for the saved tunnel and proxy profiles.
    pub fn ask_profiles(&mut self) {
        for kind in ["tunnel", "proxy"] {
            self.ask(
                Pending::Profiles { kind },
                "store.profiles.list",
                json!({ "kind": kind }),
            );
        }
    }

    /// Store one, under a name. Saving over a name replaces it, which is how a
    /// profile is edited: one bastion, one entry, changed in one place.
    pub fn save_profile(&mut self, kind: &'static str, name: String, spec: Value) {
        self.ask(
            Pending::ProfileSaved { kind },
            "store.profiles.save",
            json!({ "kind": kind, "name": name, "spec": spec }),
        );
    }

    pub fn remove_profile(&mut self, kind: &'static str, name: &str) {
        self.ask(
            Pending::ProfileRemoved { kind, name: name.to_string() },
            "store.profiles.remove",
            json!({ "kind": kind, "name": name }),
        );
    }

    /// The names a connection window can choose from.
    pub fn profile_names(&self, kind: &str) -> Vec<String> {
        self.profiles
            .get(kind)
            .map(|v| v.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default()
    }

    pub fn form_save(
        &mut self,
        name: String,
        driver: String,
        spec: Value,
        renamed_from: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let rename_from = renamed_from.filter(|o| *o != name);
        // **An engine that can rename, renames.** Saved queries, charts,
        // dashboards and scripts are scoped to the connection's NAME: a save
        // under the new name followed by a delete of the old one leaves all of
        // them pointing at a name nothing answers to. `store.connections.rename`
        // (protocol 15) moves them in one transaction; the save that follows
        // writes the edited spec onto the renamed row.
        let engine_renames = self.hello.as_ref().map_or(false, |h| h.proto >= 15);
        if let (true, Some(old)) = (engine_renames, rename_from.clone()) {
            self.ask(
                Pending::Rename { to: name.clone(), driver: driver.clone(), spec: spec.clone() },
                "store.connections.rename",
                json!({ "from": old, "to": name }),
            );
            return;
        }
        // Older engine: a save under the new name and a removal of the old, but
        // SEQUENTIAL, not parallel. Fired together, a refused save left the old
        // name removed and the new one never written — a connection deleted by
        // pressing Save.
        self.ask(
            Pending::FormSave { rename_from },
            "store.connections.save",
            json!({ "name": name, "driver": driver, "spec": spec }),
        );
        // NOT closed here. The window closes when the ENGINE says the
        // connection is stored — see `Pending::FormSave`. Closing on the way out
        // of this function closes it whether the save worked or not, and a
        // window that vanishes on a failed save takes the typing with it.
        //
        // The repaint IS needed: the status line already says the save is in
        // flight, and nothing else this frame asks for one.
        cx.notify();
    }

    /// Save the active tab's statement under its title, for this connection.
    /// The engine owns saved queries, so one saved here appears in the editor.
    /// Ask what to call it. The save itself is `save_query_as`.
    fn save_query(&mut self, cx: &mut Context<Self>) {
        self.ask_for_name(NameFor::Query, cx);
    }

    /// Put up the naming row, with what the tab is called as the suggestion.
    fn ask_for_name(&mut self, what: NameFor, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.sql.trim().is_empty() && tab.fields.is_empty() {
            self.toast("there is nothing to save yet".to_string(), true);
            return;
        }
        let suggestion = tab.title.clone();
        self.naming = Some(what);
        self.save_name.update(cx, |i, cx| i.set_text(suggestion, cx));
        cx.notify();
    }

    /// The name came back from the row.
    fn named(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(what) = self.naming.take() else { return };
        // A FOLDER may be empty — that is how a connection comes out of one.
        // A query or a chart may not: the name is how it is found again.
        if name.is_empty() && !matches!(what, NameFor::Folder(_)) {
            let whose = what.what();
            self.naming = Some(what);
            self.toast(format!("a {whose} needs a name"), true);
            return;
        }
        match what {
            NameFor::Query => self.save_query_as(name, cx),
            NameFor::Chart => self.save_chart_as(name, cx),
            NameFor::Folder(conn) => self.file_connection(&conn, &name),
        }
    }

    /// File a saved connection in a folder, or take it out of one.
    ///
    /// The spec goes back UNCHANGED, read from what is stored: this is a save
    /// of the folder, and re-deriving the spec from a window that is not open
    /// would be a save of something nobody edited.
    fn file_connection(&mut self, conn: &str, folder: &str) {
        let Some(c) = self.saved.iter().find(|c| c.name == conn) else { return };
        let (name, driver, spec) = (c.name.clone(), c.driver.clone(), c.spec.clone());
        let group = if folder.trim().is_empty() {
            // `null`, which the engine reads as "take it out" — and which it
            // tells apart from a save that says nothing. See `present_or_absent`
            // in the engine.
            Value::Null
        } else {
            Value::String(folder.trim().to_string())
        };
        self.ask(
            Pending::Connections,
            "store.connections.save",
            json!({ "name": name, "driver": driver, "spec": spec, "group": group }),
        );
        self.ask(Pending::Connections, "store.connections.list", json!({}));
    }

    fn save_query_as(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let conn = tab.conn_name.clone();
        let id = tab.id;
        self.sync_tab_from_inputs(id, cx);
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else { return };
        let sql = tab.sql.clone();
        if sql.trim().is_empty() {
            return;
        }
        self.ask(
            Pending::SavedQueries { conn: conn.clone() },
            "store.queries.save",
            json!({ "conn": conn, "name": name, "sql": sql }),
        );
        // The save answers `{saved:true}`, not a list, so ask for the list.
        self.ask(
            Pending::SavedQueries { conn: conn.clone() },
            "store.queries.list",
            json!({ "conn": conn }),
        );
        self.status = format!("saved query: {name}");
        self.toast(format!("saved query: {name}"), false);
    }

    /// Open a statement in its own tab on a connection — used by the history
    /// list and by a saved query.
    fn open_sql(&mut self, conn: &str, title: &str, sql: &str, cx: &mut Context<Self>) {
        if self.live(conn).is_none() {
            // A click that does nothing at all reads as a broken list, so this
            // one says why in the corner as well as in the log.
            self.toast(format!("{conn} is not connected"), true);
            return;
        }
        // **It LOADS, it does not run.** A click in the history ran the
        // statement straight away, which for a history of writes means a click
        // in a list repeats a write — and for a slow read it means waiting for
        // something somebody only wanted to look at. The statement goes into
        // the editor, where it can be read, changed, and run with ctrl-enter.
        //
        // Into the tab IN FRONT when it belongs to this connection and holds
        // nothing that would be lost; a tab of its own otherwise.
        let here = self
            .tabs
            .get(self.active)
            .filter(|t| t.conn_name == conn && t.board.is_none() && t.object.is_none())
            .map(|t| t.id);
        match here.and_then(|id| self.editors.get(&id).cloned().map(|e| (id, e))) {
            Some((id, ed)) => {
                let text = sql.to_string();
                ed.update(cx, |e, cx| e.set_text(&text, cx));
                if let Some(tb) = self.tabs.iter_mut().find(|t| t.id == id) {
                    tb.sql = sql.to_string();
                    tb.title = title.to_string();
                }
                self.status = format!("loaded: {title}");
                self.remember();
                cx.notify();
            }
            None => self.open_draft_with(conn, title, sql, cx),
        }
    }

    /// A tab of its own, holding a statement and NOT running it.
    fn open_draft_with(&mut self, conn: &str, title: &str, sql: &str, cx: &mut Context<Self>) {
        let Some(live) = self.live(conn) else { return };
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let id = self.next_tab;
        self.next_tab += 1;
        let mut tab = Tab::new(id, conn.to_string(), driver, title.to_string(), meta.as_ref());
        tab.sql = sql.to_string();
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        let hint = meta.as_ref().map(|m| m.hint.clone()).unwrap_or_default();
        self.make_editor(id, Some(&hint), sql, cx);
        crate::workspace::mru_touch(&mut self.mru, id);
        self.remember();
        cx.notify();
    }

    /// Write what the grid holds. What is written is what is LOADED, and the row
    /// says so — an export that quietly paged the whole table in the background
    /// is a different feature with a different cost.
    fn write_export(&mut self, cx: &mut Context<Self>) {
        let path = self.export_path.read(cx).text().trim().to_string();
        if path.is_empty() {
            return;
        }
        let fmt = self.export_format;
        let msg = match self.tabs.get(self.active) {
            Some(t) if !t.rows.is_empty() => {
                match export::write(std::path::Path::new(&path), &t.columns, &t.rows, fmt) {
                    Ok(n) => format!("wrote {n} row(s) to {path}"),
                    Err(e) => format!("could not write {path}: {e}"),
                }
            }
            _ => "nothing loaded to export".to_string(),
        };
        self.status = msg;
    }

    /// What the word under a tab's cursor could become.
    ///
    /// The names come from the OPEN connection's tree and from the fields the
    /// engine has already answered for — never from a guess about what a
    /// database might contain. Where a collection's fields have not been asked
    /// for yet, it offers nothing for that name rather than offering the
    /// keywords as if they were its columns.
    fn complete_word(&mut self, tab_id: u64, word: &str, cx: &mut Context<Self>) -> Vec<String> {
        let _ = cx;
        if word.is_empty() {
            return Vec::new();
        }
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else { return Vec::new() };
        let conn = tab.conn_name.clone();
        let Some(live) = self.live(&conn) else { return Vec::new() };
        // Every name in the tree, at every depth: a schema's tables are two
        // levels down and are exactly what somebody is typing towards.
        let mut objects: Vec<String> = Vec::new();
        collect_names(&live.nodes, &mut objects);
        // And the columns of whatever has been looked inside. The key the
        // fields are stored under is `conn/kind/name`, so the NAME is its last
        // part — which is the word a statement uses.
        let columns: Vec<(String, Vec<String>)> = self
            .node_fields
            .iter()
            .filter(|(k, _)| k.starts_with(&format!("{conn}/")))
            .filter_map(|(k, cols)| {
                let name = k.rsplit('/').next()?.to_string();
                Some((name, cols.iter().map(|c| c.name.clone()).collect()))
            })
            .collect();
        let schema = crate::workspace::Schema { objects: &objects, columns: &columns };
        // The language's own words, from the same list the highlighter uses —
        // one dictionary, so a word that is coloured as a keyword is a word
        // that can be completed to.
        let keywords = crate::editor::KEYWORDS;
        crate::workspace::completions(word, &schema, keywords)
    }

    /// Write the WHOLE result to the chosen file, a page at a time.
    ///
    /// Not one call: a table of two million rows would want itself in memory
    /// twice — once as rows, once as text — to produce a file written in one go
    /// at the end. The file is opened now, each page is written as it arrives,
    /// and the task row in the drawer says how far it has got.
    ///
    /// It reads the SAME call the grid is showing, so nothing is run twice and
    /// the file matches what is on screen. The engine buffers the call's rows,
    /// which is what makes that possible.
    fn start_export_all(&mut self, cx: &mut Context<Self>) {
        let path = self.export_path.read(cx).text().trim().to_string();
        if path.is_empty() {
            self.toast("name a file first".to_string(), true);
            return;
        }
        if self.exporting.is_some() {
            self.toast("an export is already running".to_string(), true);
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(call_id) = tab.call_id else {
            self.toast("run something first".to_string(), true);
            return;
        };
        let (tab_id, columns, total) = (tab.id, tab.columns.clone(), tab.total);
        let writer = match crate::export::Streaming::new(
            std::path::Path::new(&path),
            &columns,
            self.export_format,
        ) {
            Ok(w) => w,
            Err(e) => {
                self.toast(format!("could not open {path}: {e}"), true);
                return;
            }
        };
        let task = self.next_task;
        self.next_task += 1;
        self.tasks.push(Task::new(task, format!("export \u{2192} {path}")));
        self.exporting = Some(Exporting { tab: tab_id, call_id, task, at: 0, writer, total });
        // The first page. Each answer asks for the next, which is the loop.
        self.ask(
            Pending::ExportPage,
            "query.page",
            json!({ "call_id": call_id, "from": 0, "n": PAGE }),
        );
        self.drawer = Some(Drawer::Tasks);
    }

    /// One page has arrived: write it and ask for the next, or finish.
    fn export_page(&mut self, page: lode_proto::Page, cx: &mut Context<Self>) {
        let Some(mut job) = self.exporting.take() else { return };
        let got = page.rows.len();
        if let Err(e) = job.writer.push(&page.rows) {
            let n = job.writer.written();
            self.finish_export_task(job.task, Some(format!("after {n} row(s): {e}")));
            self.toast(format!("the export failed: {e}"), true);
            return;
        }
        job.at += got;
        // The engine says `has_more`; the row count is the belt and braces. A
        // page of nothing would otherwise loop for ever against an engine that
        // answered `has_more` wrongly.
        let more = page.has_more && got > 0;
        if !more {
            let path = job.writer.path().display().to_string();
            match job.writer.finish() {
                Ok(n) => {
                    self.finish_export_task(job.task, None);
                    self.status = format!("wrote {n} row(s) to {path}");
                    self.toast(format!("wrote {n} row(s)"), false);
                }
                Err(e) => {
                    self.finish_export_task(job.task, Some(e.to_string()));
                    self.toast(format!("the export could not be closed: {e}"), true);
                }
            }
            cx.notify();
            return;
        }
        let (call_id, from) = (job.call_id, job.at);
        // The task row counts up, which is the only sign a long export is
        // alive at all.
        let label = match job.total {
            Some(total) => format!("export {} of {total}", job.at),
            None => format!("export {} row(s)", job.at),
        };
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == job.task) {
            t.label = label;
        }
        self.exporting = Some(job);
        self.ask(
            Pending::ExportPage,
            "query.page",
            json!({ "call_id": call_id, "from": from, "n": PAGE }),
        );
        cx.notify();
    }

    fn finish_export_task(&mut self, task: u64, error: Option<String>) {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == task) {
            let d = t.duration();
            t.finish(d, error);
        }
    }

    /// Order the loaded page by a column; clicking the same one again reverses
    /// it. The page, not the result — see `sort_rows`.
    fn sort_by(&mut self, tab_id: u64, col: usize) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
        let ascending = match tab.sort {
            Some((c, asc)) if c == col => !asc,
            _ => true,
        };
        crate::workspace::sort_rows(&mut tab.rows, col, ascending);
        tab.sort = Some((col, ascending));
        tab.selected = None;
    }

    // ── keyboard ────────────────────────────────────────────────────────────

    /// Move the highlighted row, and keep it inside the rows that are loaded.
    /// `by` of 0 means "go to the end".
    fn move_row(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if tab.rows.is_empty() {
            return;
        }
        let last = tab.rows.len() - 1;
        let now = tab.selected.unwrap_or(0) as isize;
        tab.selected = Some(match by {
            isize::MAX => last,
            isize::MIN => 0,
            d => (now + d).clamp(0, last as isize) as usize,
        });
        // A detail panel that is open follows the cursor — it is showing "the
        // row", not "that row".
        if tab.detail.is_some() {
            tab.detail = tab.selected;
        }
        let want = tab.selected;
        // And the list follows it. CENTRED rather than merely "in view": a row
        // scrolled to the bottom edge gives no sight of what comes next, and
        // the next thing is what somebody moving the cursor is looking for.
        if let Some(i) = want {
            self.grid_rows.scroll_to_item(i, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }

    /// A DRAFT tab: the connection in front, nothing typed yet.
    ///
    /// The only way to get an empty tab before this was to run something and
    /// then clear the box, which is a strange way to start.
    fn new_draft_tab(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.target_conn() else {
            self.toast("nothing connected".to_string(), true);
            return;
        };
        let Some(live) = self.live(&conn) else { return };
        let driver = live.driver.clone();
        let meta = self.meta(&driver).cloned();
        let id = self.next_tab;
        self.next_tab += 1;
        let tab = Tab::new(id, conn, driver, "Draft".to_string(), meta.as_ref());
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        let hint = meta.as_ref().map(|m| m.hint.clone()).unwrap_or_default();
        self.make_editor(id, Some(&hint), "", cx);
        crate::workspace::mru_touch(&mut self.mru, id);
        self.remember();
        cx.notify();
    }

    fn go_tab_at(&mut self, at: usize, cx: &mut Context<Self>) {
        if at < self.tabs.len() {
            self.active = at;
            let id = self.tabs[at].id;
            crate::workspace::mru_touch(&mut self.mru, id);
            self.remember();
            cx.notify();
        }
    }

    fn go_tab_id(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(at) = self.tabs.iter().position(|t| t.id == id) {
            self.go_tab_at(at, cx);
        }
    }

    fn go_rail(&mut self, rail: Rail, cx: &mut Context<Self>) {
        self.rail = rail;
        self.panel_hidden = false;
        if rail == Rail::Audit && self.audit.is_empty() {
            self.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
        }
        if rail == Rail::Tasks {
            self.log_unread = 0;
        }
        self.remember();
        cx.notify();
    }

    fn step_tab(&mut self, by: isize, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let n = self.tabs.len() as isize;
        self.active = (((self.active as isize + by) % n + n) % n) as usize;
        if let Some(t) = self.tabs.get(self.active) {
            let id = t.id;
            crate::workspace::mru_touch(&mut self.mru, id);
        }
        self.remember();
        cx.notify();
    }

    // ── toasts ──────────────────────────────────────────────────────────────

    /// Say something briefly in the corner. It goes to the console log too —
    /// a toast expires, and an error that only ever appeared in one is an error
    /// nobody can go back and read.
    fn toast(&mut self, text: impl Into<String>, bad: bool) {
        let text = text.into();
        self.log_push(if bad { "error" } else { "info" }, text.clone());
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, text, bad, born: Instant::now() });
    }

    // ── the palette ─────────────────────────────────────────────────────────

    /// Everything the palette can reach, before filtering.
    ///
    /// It is built fresh each time it is drawn, from the state as it is: there
    /// is no registry to forget to update, so a connection opened a second ago
    /// is in here, and one that was deleted is not.
    fn palette_items(&self) -> Vec<PaletteItem> {
        let mut out = Vec::new();
        for s in &self.saved {
            let live = self.live(&s.name).is_some();
            out.push(PaletteItem {
                label: format!("{} {}", if live { "Go to" } else { "Connect" }, s.name),
                hint: s.driver.clone(),
                act: PaletteAct::Connect(s.name.clone()),
            });
        }
        for c in &self.conns {
            for q in &c.queries {
                out.push(PaletteItem {
                    label: format!("Query: {}", q.name),
                    hint: c.name.clone(),
                    act: PaletteAct::SavedQuery {
                        conn: c.name.clone(),
                        sql: q.sql.clone(),
                        name: q.name.clone(),
                    },
                });
            }
            for n in &c.nodes {
                Self::palette_nodes(&c.name, n, None, &mut out);
            }
        }
        for h in self.history.iter().take(40) {
            let one: String = h.statement.replace('\n', " ").chars().take(60).collect();
            out.push(PaletteItem {
                label: format!("History: {one}"),
                hint: h.conn.clone().unwrap_or_default(),
                act: PaletteAct::HistoryItem {
                    conn: h.conn.clone().unwrap_or_default(),
                    sql: h.statement.clone(),
                },
            });
        }
        for (i, tb) in self.tabs.iter().enumerate() {
            out.push(PaletteItem {
                label: format!("Tab: {}", tb.title),
                hint: format!("{} · {}", tb.conn_name, i + 1),
                act: PaletteAct::GoTab(tb.id),
            });
        }
        for v in [ResultView::Tree, ResultView::Table, ResultView::Json, ResultView::Chart] {
            out.push(PaletteItem {
                label: format!("View as {}", v.label()),
                hint: "result".into(),
                act: PaletteAct::View(v),
            });
        }
        for r in [Rail::Connections, Rail::Instance, Rail::History, Rail::Audit, Rail::Tasks] {
            // The KEY where there is one. The palette is where a keystroke is
            // learned: a row that says "panel" teaches nothing, and the whole
            // point of showing a shortcut here is that next time nobody opens
            // the palette at all.
            let hint = match r {
                Rail::Audit => "ctrl-shift-a",
                Rail::Tasks => "ctrl-shift-t",
                _ => "panel",
            };
            out.push(PaletteItem {
                label: format!("{} {}", self.lang.tr("panel.look_again").split(' ').next().unwrap_or("Show"), self.lang.tr(r.key())),
                hint: hint.into(),
                act: PaletteAct::Rail(r),
            });
        }
        out.push(PaletteItem { label: "Scroll the grid left".into(), hint: "shift-h".into(), act: PaletteAct::ScrollGrid(1.0) });
        out.push(PaletteItem { label: "Scroll the grid right".into(), hint: "shift-l".into(), act: PaletteAct::ScrollGrid(-1.0) });
        out.push(PaletteItem { label: "Run this tab".into(), hint: "r / f5".into(), act: PaletteAct::RunActive });
        out.push(PaletteItem { label: "Copy this cell".into(), hint: "ctrl-c".into(), act: PaletteAct::CopyCell });
        out.push(PaletteItem { label: "Copy this row".into(), hint: "ctrl-shift-c".into(), act: PaletteAct::CopyRow });
        out.push(PaletteItem { label: "Copy the loaded rows".into(), hint: "y".into(), act: PaletteAct::CopyPage });
        out.push(PaletteItem { label: "New tab".into(), hint: "ctrl-t".into(), act: PaletteAct::NewTab });
        out.push(PaletteItem { label: "Close this tab".into(), hint: "ctrl-w".into(), act: PaletteAct::CloseTab });
        out.push(PaletteItem { label: "The tab before this".into(), hint: "ctrl-tab".into(), act: PaletteAct::LastTab });
        out.push(PaletteItem { label: "Hide the panel".into(), hint: "ctrl-shift-b".into(), act: PaletteAct::TogglePanel });
        out.push(PaletteItem { label: "Settings".into(), hint: "ctrl-,".into(), act: PaletteAct::OpenSettings });
        out.push(PaletteItem { label: "Next page of rows".into(), hint: "]".into(), act: PaletteAct::Page(1) });
        out.push(PaletteItem { label: "Previous page of rows".into(), hint: "[".into(), act: PaletteAct::Page(-1) });
        out.push(PaletteItem { label: "Export loaded rows".into(), hint: "ctrl-e".into(), act: PaletteAct::ToggleExport });
        out.push(PaletteItem { label: "Save this query".into(), hint: "result".into(), act: PaletteAct::SaveQuery });
        out.push(PaletteItem { label: "Toggle console".into(), hint: "window".into(), act: PaletteAct::ToggleConsole });
        out.push(PaletteItem { label: "New connection".into(), hint: "ctrl-shift-n".into(), act: PaletteAct::NewConnection });
        out.push(PaletteItem { label: "New tunnel profile".into(), hint: "window".into(), act: PaletteAct::NewProfile });
        for p in self.profiles.get("tunnel").into_iter().flatten() {
            out.push(PaletteItem {
                label: format!("Edit tunnel: {}", p.name),
                hint: "window".into(),
                act: PaletteAct::EditProfile(p.name.clone()),
            });
        }
        // One per saved connection, so the settings can be reached by typing
        // the connection's name — the pencil in the list is easy to miss.
        for c in &self.saved {
            out.push(PaletteItem {
                label: format!("Edit connection: {}", c.name),
                hint: "window".into(),
                act: PaletteAct::EditConnection(c.name.clone()),
            });
        }
        out
    }

    fn palette_nodes(conn: &str, node: &Node, schema: Option<&str>, out: &mut Vec<PaletteItem>) {
        let here = if node.kind == "schema" || node.kind == "database" {
            Some(node.name.as_str())
        } else {
            schema
        };
        if !matches!(node.kind.as_str(), "schema" | "database" | "folder" | "group") {
            out.push(PaletteItem {
                label: format!("Open {}", node.name),
                hint: format!("{conn} · {}", node.kind),
                act: PaletteAct::Open {
                    conn: conn.to_string(),
                    object: node.name.clone(),
                    schema: here.map(|s| s.to_string()),
                    kind: node.kind.clone(),
                },
            });
        }
        for c in &node.children {
            Self::palette_nodes(conn, c, here, out);
        }
    }

    /// The items that match what has been typed, best first.
    fn palette_matches(&self, cx: &gpui::App) -> Vec<(usize, PaletteItem)> {
        let needle = self.palette_input.read(cx).text().to_string();
        let mut scored: Vec<(i32, usize, PaletteItem)> = self
            .palette_items()
            .into_iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let s = fuzzy_score(&it.label, &needle)
                    .or_else(|| fuzzy_score(&it.hint, &needle).map(|s| s + 50))?;
                Some((s, i, it))
            })
            .collect();
        scored.sort_by_key(|(s, i, _)| (*s, *i));
        scored.into_iter().take(60).map(|(_, i, it)| (i, it)).collect()
    }

    fn palette_run(&mut self, act: PaletteAct, cx: &mut Context<Self>) {
        self.palette_open = false;
        match act {
            PaletteAct::Rail(r) => {
                self.rail = r;
                // Asking for a panel by name is asking to SEE it: folded away,
                // the row would have done nothing at all.
                self.panel_hidden = false;
                if r == Rail::Tasks {
                    self.log_unread = 0;
                }
                if r == Rail::History && self.history.is_empty() {
                    self.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                }
                if r == Rail::Audit && self.audit.is_empty() {
                    self.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
                }
                self.remember();
            }
            PaletteAct::ScrollGrid(dir) => {
                self.scroll_grid(dir * self.scale.col_w());
            }
            PaletteAct::View(v) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    tab.view = v;
                }
            }
            PaletteAct::GoTab(id) => {
                if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
                    self.active = i;
                    self.remember();
                }
            }
            PaletteAct::Connect(name) => {
                if let Some(i) = self.tabs.iter().position(|t| t.conn_name == name) {
                    self.active = i;
                } else {
                    self.connect(&name);
                }
            }
            PaletteAct::Open { conn, object, schema, kind } => {
                self.open_object(&conn, &object, schema.as_deref(), &kind, cx)
            }
            PaletteAct::SavedQuery { conn, sql, name } => self.open_sql(&conn, &name, &sql, cx),
            PaletteAct::HistoryItem { conn, sql } => {
                let title: String = sql.chars().take(40).collect();
                self.open_sql(&conn, &title, &sql, cx);
            }
            PaletteAct::RunActive => {
                if let Some(id) = self.tabs.get(self.active).map(|t| t.id) {
                    self.run_tab(id, cx);
                }
            }
            PaletteAct::ToggleExport => self.export_open = !self.export_open,
            PaletteAct::SaveQuery => self.save_query(cx),
            PaletteAct::NewTab => self.new_draft_tab(cx),
            PaletteAct::CloseTab => {
                if let Some(t) = self.tabs.get(self.active) {
                    let id = t.id;
                    self.close_tab(id);
                }
            }
            PaletteAct::LastTab => {
                let alive: Vec<u64> = self.tabs.iter().map(|t| t.id).collect();
                let now = self.tabs.get(self.active).map(|t| t.id).unwrap_or(0);
                if let Some(to) = crate::workspace::mru_next(&self.mru, now, &alive) {
                    self.go_tab_id(to, cx);
                }
            }
            PaletteAct::TogglePanel => {
                self.panel_hidden = !self.panel_hidden;
                self.remember();
            }
            PaletteAct::OpenSettings => self.open_settings(cx),
            PaletteAct::Page(by) => self.page_by(by),
            PaletteAct::CopyCell => self.copy_cell(cx),
            PaletteAct::CopyRow => {
                let what = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.selected.or(t.detail).map(|r| (t.id, r)));
                match what {
                    Some((id, row)) => self.copy_row_tsv(id, row, cx),
                    None => self.toast("pick a row first".to_string(), true),
                }
            }
            PaletteAct::CopyPage => self.copy_page_tsv(cx),
            PaletteAct::ToggleConsole => {
                self.drawer = match self.drawer {
                    Some(Drawer::Command) => None,
                    _ => Some(Drawer::Command),
                }
            }
            PaletteAct::NewConnection => self.open_form(None, cx),
            PaletteAct::NewProfile => self.open_profile("tunnel", None, cx),
            PaletteAct::EditProfile(name) => {
                let found = self
                    .profiles
                    .get("tunnel")
                    .and_then(|v| v.iter().find(|p| p.name == name))
                    .map(|p| (p.name.clone(), p.spec.clone()));
                if let Some(found) = found {
                    self.open_profile("tunnel", Some(found), cx);
                }
            }
            PaletteAct::EditConnection(name) => {
                let found = self
                    .saved
                    .iter()
                    .find(|c| c.name == name)
                    .map(|c| (c.name.clone(), c.driver.clone(), c.spec.clone()));
                if let Some(found) = found {
                    self.open_form(Some(found), cx);
                }
            }
        }
    }

    // ── the clipboard ───────────────────────────────────────────────────────

    /// Put the selected row on the clipboard as a statement. See
    /// `copy_as_select` for why generating SQL is allowed HERE and nowhere else.
    fn copy_row(&mut self, as_insert: bool, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(i) = tab.selected.or(tab.detail) else {
            self.toast("pick a row first", true);
            return;
        };
        let Some(row) = tab.rows.get(i) else { return };
        let table = tab
            .object
            .as_ref()
            .map(|(name, schema)| match schema {
                Some(s) => format!("{s}.{name}"),
                None => name.clone(),
            })
            .unwrap_or_else(|| tab.title.clone());
        let sql = if as_insert {
            copy_as_insert(&table, &tab.columns, row)
        } else {
            copy_as_select(&table, &tab.columns, row)
        };
        cx.write_to_clipboard(ClipboardItem::new_string(sql));
        self.toast(
            if as_insert { "row copied as INSERT" } else { "row copied as SELECT" },
            false,
        );
    }

    /// Up and down through the console's history.
    ///
    /// The history is the ENGINE's, so what was typed into the editor is in it
    /// too — which is right: this is "what has been run here", not "what I
    /// typed in this box". Narrowed to the connection the console is pointed
    /// at, because a statement from another database is not a suggestion.
    fn step_console_history(&mut self, back: bool, cx: &mut Context<Self>) {
        let target = self.target_conn();
        let lines: Vec<String> = self
            .history
            .iter()
            .filter(|h| match (&target, &h.conn) {
                (Some(t), Some(c)) => c == t,
                // An engine that did not say which connection it was: shown
                // rather than hidden, because the alternative is a history that
                // looks empty.
                (_, None) => true,
                _ => false,
            })
            .map(|h| h.statement.clone())
            .collect();
        self.console_hist = crate::workspace::history_step(self.console_hist, back, lines.len());
        let text = match self.console_hist {
            Some(i) => lines.get(i).cloned().unwrap_or_default(),
            None => String::new(),
        };
        self.console.update(cx, |i, cx| i.set_text(text, cx));
        cx.notify();
    }

    /// The cell cursor sideways.
    fn step_col(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let count = tab.grid_columns().len();
        tab.cursor_col = crate::workspace::step_col(tab.cursor_col, by, count);
        // A cell cursor with no row under it is a cursor on nothing.
        if tab.selected.is_none() && !tab.rows.is_empty() {
            tab.selected = Some(0);
        }
        cx.notify();
    }

    /// The cell the cursor is on, on the clipboard.
    ///
    /// Straight out of the page, and that is not a shortcut. The engine buffers
    /// rows WHOLE — `query.cell` exists for a client that did not keep the page
    /// it was given, and the README says so: "a cell's full value is always
    /// retrievable even when the grid truncated it". The truncation is in the
    /// DRAWING, at the column's edge; the value in hand is complete, so asking
    /// again would be a round trip for something already here.
    fn copy_cell(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(row) = tab.selected.or(tab.detail) else {
            self.toast("pick a row first".to_string(), true);
            return;
        };
        let cols = tab.grid_columns();
        let Some(ci) = cols.get(tab.cursor_col).map(|(i, _)| *i) else { return };
        let Some(value) = tab.rows.get(row).and_then(|r| r.get(ci)).cloned() else { return };
        let text = cell_text(&value).unwrap_or_default();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.toast("cell copied".to_string(), false);
    }

    /// Every loaded row as tab-separated values, with the column names above.
    fn copy_page_tsv(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        if tab.rows.is_empty() {
            self.toast("nothing to copy".to_string(), true);
            return;
        }
        let cols: Vec<String> = tab.columns.iter().map(|c| c.name.clone()).collect();
        let text = crate::menu::page_tsv(&cols, &tab.rows);
        let n = tab.rows.len();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        // How MANY, because what is copied is what is LOADED — the same honesty
        // the export row has about itself.
        self.toast(format!("{n} loaded row(s) copied"), false);
    }

    /// One row as tab-separated values.
    ///
    /// TSV because a spreadsheet and a terminal both understand it, and because
    /// CSV would have to decide about quoting — a decision that is wrong for
    /// one of the two. See `menu::row_tsv`.
    fn copy_row_tsv(&mut self, tab_id: u64, row: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else { return };
        let Some(values) = tab.rows.get(row) else { return };
        let cols: Vec<String> = tab.columns.iter().map(|c| c.name.clone()).collect();
        let text = crate::menu::row_tsv(&cols, values);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.toast("row copied".to_string(), false);
    }

    // ── saved charts ────────────────────────────────────────────────────────

    /// Open a saved chart: its statement in a tab, shown as a chart, measuring
    /// the column it was saved measuring.
    ///
    /// The measure is matched BY NAME once the result arrives, in `on_result` —
    /// if the statement has changed and that column is gone, the chart falls
    /// back to the first measurement rather than drawing a different column
    /// under the old name.
    fn open_chart(&mut self, conn: &str, name: &str, sql: &str, measure: &str, cx: &mut Context<Self>) {
        if self.live(conn).is_none() {
            self.toast(format!("{conn} is not connected"), true);
            return;
        }
        self.run_statement_titled(conn, name, sql, false, cx);
        if let Some(tab) = self.tabs.last_mut() {
            tab.view = ResultView::Chart;
            tab.chart_measure = Some(measure.to_string());
        }
    }

    // ── dashboards ───────────────────────────────────────────────────────────

    fn pane_mut(&mut self, tab: u64, tile: usize) -> Option<&mut Pane> {
        self.tabs
            .iter_mut()
            .find(|t| t.id == tab)?
            .board
            .as_mut()
            .map(|b| b.panes.entry(tile).or_default())
    }

    fn board_mut(&mut self, tab: u64) -> Option<&mut Board> {
        self.tabs.iter_mut().find(|t| t.id == tab)?.board.as_mut()
    }

    /// Put a new tile at the bottom of the board.
    ///
    /// A chart tile takes the FIRST saved chart that is not already on the
    /// board — a tile that opens naming nothing is a tile that fails the
    /// engine's check the moment anybody presses Save, and the point of the
    /// check is to be told early, not to be told at the end.
    fn board_add(&mut self, tab: u64, kind: TileKind) {
        let Some(b) = self.board_mut(tab) else { return };
        match kind {
            TileKind::Chart => {
                let used: Vec<String> = b.dash.tiles.iter().map(|t| t.chart.clone()).collect();
                let pick = b
                    .charts
                    .iter()
                    .find(|c| !used.contains(&c.name))
                    .or_else(|| b.charts.first())
                    .map(|c| c.name.clone());
                match pick {
                    Some(name) => b.push(TileKind::Chart, &name, "", 6, 4),
                    None => {
                        // Nothing to put on it, said plainly rather than by
                        // adding an empty square.
                        self.toast(
                            "there are no saved charts on this connection yet — save a chart first",
                            true,
                        );
                        return;
                    }
                }
            }
            TileKind::Divider => b.push(TileKind::Divider, "", "", 12, 1),
            TileKind::Text => b.push(TileKind::Text, "", "", 12, 1),
        }
        self.run_board(tab);
    }

    fn board_drag_start(&mut self, tab: u64, tile: usize, mode: DragMode, pos: (f32, f32)) {
        let Some(b) = self.board_mut(tab) else { return };
        let Some(t) = b.dash.tiles.get(tile) else { return };
        let start = (t.x, t.y, t.w, t.h);
        b.selected = Some(tile);
        b.drag = Some(board::Drag { tile, mode, from: pos, start });
        self.dragging = Some(tab);
    }

    /// Where the drag has got to. Called from the root on every mouse move,
    /// because the pointer leaves the tile's own square long before the drag is
    /// over.
    fn board_drag_to(&mut self, pos: (f32, f32)) {
        let Some(tab) = self.dragging else { return };
        let (cw, rh, gap, cols) = {
            let Some(b) = self.tabs.iter().find(|t| t.id == tab).and_then(|t| t.board.as_ref()) else {
                return;
            };
            let cols = b.columns.max(1);
            (
                board::cell_w(self.canvas_w, cols, 0.0),
                self.scale.base() * 4.0,
                0.0f32,
                cols,
            )
        };
        let Some(b) = self.board_mut(tab) else { return };
        let Some(d) = b.drag.as_ref() else { return };
        let (tile, start, from, mode) = (d.tile, d.start, d.from, d.mode);
        let delta = (pos.0 - from.0, pos.1 - from.1);
        let candidate = match mode {
            DragMode::Move => {
                let (x, y) = board::moved(start, delta, cw, rh, gap, cols);
                (x, y, start.2, start.3)
            }
            DragMode::Resize => {
                let (w, h) = board::resized(start, delta, cw, rh, gap, cols);
                (start.0, start.1, w, h)
            }
        };
        // A drop that would overlap is simply NOT TAKEN: the engine refuses an
        // overlapping board, and a client that asks it to store one is a client
        // that loses somebody's arrangement at the last step.
        if board::collides(&b.dash.tiles, tile, candidate) {
            return;
        }
        if let Some(t) = b.dash.tiles.get_mut(tile) {
            if (t.x, t.y, t.w, t.h) != candidate {
                t.x = candidate.0;
                t.y = candidate.1;
                t.w = candidate.2;
                t.h = candidate.3;
                b.dirty = true;
            }
        }
    }

    /// Read the three time boxes into the board and run it again.
    fn board_apply_time(&mut self, tab: u64, cx: &mut Context<Self>) {
        let col = self.board_time_col.read(cx).text().trim().to_string();
        let from = self.board_time_from.read(cx).text().trim().to_string();
        let to = self.board_time_to.read(cx).text().trim().to_string();
        if col.is_empty() || from.is_empty() || to.is_empty() {
            self.toast("a time range needs a column, a start and an end", true);
            return;
        }
        if let Some(b) = self.board_mut(tab) {
            b.dash.time = Some(TimeRange { column: col, from, to });
            b.dirty = true;
        }
        // The ENGINE decides whether those are timestamps. Saving is how it is
        // asked — the answer comes back canonicalised, or refused with a reason.
        self.save_board(tab, cx);
        self.run_board(tab);
        cx.notify();
    }

    /// Ask the engine what is waiting for a person's answer.
    ///
    /// Every five seconds, and ONLY while the panel that shows them is open —
    /// the queue is filled by an agent working on its own, so it has to be
    /// polled rather than waited for, and polling a database every five
    /// seconds for a panel nobody is looking at is a cost with no reader.
    ///
    /// Once at startup too, whatever panel is open: something may have been
    /// waiting since before this window started, and a queue that only appears
    /// after somebody thinks to look is a queue that holds things for days.
    fn poll_approvals(&mut self) {
        if self.engine.is_none() || self.hello.as_ref().map_or(true, |h| h.proto < 12) {
            return;
        }
        let first = self.approvals_at.is_none();
        let watching = self.rail == Rail::Audit && !self.panel_hidden;
        let due = self
            .approvals_at
            .map_or(true, |at| watching && at.elapsed() >= Duration::from_secs(5));
        if !(first || due) {
            return;
        }
        self.approvals_at = Some(Instant::now());
        self.ask(Pending::Approvals, "approvals.list", json!({ "state": "pending" }));
    }

    /// A person's answer to one. It goes to the ENGINE, which writes it in the
    /// audit log — "who let this through" is the question asked straight after
    /// "what ran".
    fn decide_approval(&mut self, id: i64, approve: bool) {
        self.ask(
            Pending::ApprovalDecided,
            "approvals.decide",
            json!({ "id": id, "approve": approve }),
        );
    }

    /// Run the board in FRONT again, if its interval has come round.
    ///
    /// Only the front one. A board left open in a background tab refreshing
    /// every ten seconds is a query every ten seconds against a database
    /// nobody is looking at — and the person paying for it is whoever runs the
    /// server.
    fn poll_board(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let tab_id = tab.id;
        let Some(b) = tab.board.as_ref() else { return };
        let Some(every) = b.refresh_every else { return };
        let due = b.refreshed_at.map_or(true, |at| at.elapsed() >= every);
        if !due {
            return;
        }
        // Not while it is still working: a board whose charts take longer than
        // the interval would queue runs on top of each other for ever.
        if b.panes.values().any(|p| p.state == RunState::Running) {
            return;
        }
        if let Some(bd) = self.board_mut(tab_id) {
            bd.refreshed_at = Some(Instant::now());
        }
        self.run_board(tab_id);
    }

    /// A time window ending NOW, from a preset.
    ///
    /// The column is still the board's own — only the driver knows which of its
    /// columns is a time, and a preset that guessed one would be guessing about
    /// somebody's schema. What the preset saves is the two timestamps, which
    /// are the part nobody should be typing.
    fn board_preset_time(&mut self, tab: u64, seconds: u64, cx: &mut Context<Self>) {
        let col = self.board_time_col.read(cx).text().trim().to_string();
        if col.is_empty() {
            self.toast("name the time column first".to_string(), true);
            return;
        }
        let (from, to) =
            crate::workspace::window_ending_now(seconds, std::time::SystemTime::now());
        self.board_time_from.update(cx, |i, cx| i.set_text(from.clone(), cx));
        self.board_time_to.update(cx, |i, cx| i.set_text(to.clone(), cx));
        if let Some(b) = self.board_mut(tab) {
            b.dash.time = Some(TimeRange { column: col, from, to });
            b.dirty = true;
        }
        self.save_board(tab, cx);
        self.run_board(tab);
        cx.notify();
    }

    /// Open a dashboard in its own tab. An empty `name` makes a new one.
    fn open_board(&mut self, conn: &str, name: &str, cx: &mut Context<Self>) {
        let driver = self
            .conns
            .iter()
            .find(|c| c.name == conn)
            .map(|c| c.driver.clone())
            .unwrap_or_default();
        let id = self.next_tab;
        self.next_tab += 1;
        let title = if name.is_empty() { "New dashboard".to_string() } else { name.to_string() };
        let mut tab = Tab::new(id, conn.to_string(), driver, title, self.meta_of(conn));
        let mut b = Board::new(name.to_string(), Dashboard::default(), None, board::COLUMNS);
        // A board with no name has never been saved, so there is nothing to
        // read and everything to arrange: it opens ready to be edited.
        b.editing = name.is_empty();
        tab.board = Some(b);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        if name.is_empty() {
            // Still needs the charts, or there is nothing to put on it.
            let conn = conn.to_string();
            self.ask(
                Pending::BoardCharts { tab: id },
                "store.charts.list",
                json!({ "conn": conn }),
            );
        } else {
            self.ask(
                Pending::Board { tab: id },
                "store.dashboards.get",
                json!({ "conn": conn, "name": name }),
            );
        }
        self.remember();
        cx.notify();
    }

    /// Run every chart tile on a board.
    ///
    /// One call per tile, all at once: they are independent statements and the
    /// engine runs each in its own task, so waiting for one before starting the
    /// next would make a board of six charts six times slower for no reason.
    fn run_board(&mut self, tab_id: u64) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else { return };
        let Some(b) = tab.board.as_ref() else { return };
        let Some(conn_id) = self.live(&tab.conn_name).map(|l| l.conn_id) else { return };
        let meta = self.meta_of(&tab.conn_name).cloned();
        let time = b.dash.time.clone();
        let driver = tab.driver.clone();
        let mut jobs: Vec<(usize, String, TimeFit, Option<String>)> = Vec::new();
        for (i, tile) in b.dash.tiles.iter().enumerate() {
            if tile.kind != TileKind::Chart {
                continue;
            }
            match b.chart_of(tile) {
                Some(c) => {
                    let (sql, fit) = match &meta {
                        Some(m) => apply_time(&c.sql, m, time.as_ref()),
                        None => (c.sql.clone(), TimeFit::NotMarked),
                    };
                    jobs.push((i, sql, fit, None));
                }
                // The tile stays, and says what is missing. The board does not
                // silently become a board with one fewer chart.
                None => jobs.push((
                    i,
                    String::new(),
                    TimeFit::NotMarked,
                    Some(format!("the saved chart '{}' is not on this connection", tile.chart)),
                )),
            }
        }
        for (tile, sql, fit, missing) in jobs {
            // A chart that asked for a range it cannot have is NOT SENT to a
            // database. The first version stripped the marker and sent
            // `… WHERE  ORDER BY …`, and the engine answered with a syntax
            // error — a terrible way to say "you have not picked a date".
            let say = missing.clone().or_else(|| {
                if fit.runnable() {
                    None
                } else {
                    fit.note(&driver)
                }
            });
            if let Some(p) = self.pane_mut(tab_id, tile) {
                p.statement = sql.clone();
                p.fit = fit;
                p.error = say.clone();
                p.state = if say.is_some() { RunState::Failed } else { RunState::Running };
                p.rows.clear();
            }
            if say.is_some() {
                continue;
            }
            self.ask(
                Pending::BoardRun { tab: tab_id, tile },
                "query.execute",
                json!({ "conn_id": conn_id, "statement": sql }),
            );
        }
    }

    /// Hand the board to the engine, which decides whether it is a board.
    fn save_board(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else { return };
        let Some(b) = tab.board.as_ref() else { return };
        let (conn, name, dash) = (tab.conn_name.clone(), b.name.clone(), b.dash.clone());
        if name.trim().is_empty() {
            self.toast("the dashboard needs a name before it can be saved", true);
            return;
        }
        self.ask(
            Pending::BoardSaved { tab: tab_id },
            "store.dashboards.save",
            json!({ "conn": conn, "name": name, "dashboard": dash }),
        );
        cx.notify();
    }

    /// Save what the chart is showing. The statement and the measured column,
    /// under a name — kept by the ENGINE, so it is there in the editor too.
    fn save_chart(&mut self, cx: &mut Context<Self>) {
        self.ask_for_name(NameFor::Chart, cx);
    }

    fn save_chart_as(&mut self, name: String, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let (conn, id) = (tab.conn_name.clone(), tab.id);
        self.sync_tab_from_inputs(id, cx);
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else { return };
        if tab.sql.trim().is_empty() {
            return;
        }
        let measure = tab
            .chart_col
            .and_then(|i| tab.columns.get(i))
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let (sql, conn2) = (tab.sql.clone(), conn.clone());
        self.ask(
            Pending::SavedCharts { conn: conn.clone() },
            "store.charts.save",
            json!({ "conn": conn, "name": name, "sql": sql, "measure": measure }),
        );
        self.ask(
            Pending::SavedCharts { conn: conn2.clone() },
            "store.charts.list",
            json!({ "conn": conn2 }),
        );
        self.toast(format!("saved chart: {name}"), false);
    }

    // ── instance metrics ────────────────────────────────────────────────────

    /// Ask the server what it is doing. One round trip, when asked.
    fn ask_metrics(&mut self) {
        // The instance being looked at, not the first one that connected.
        let Some(target) = self.target_conn() else { return };
        let Some(live) = self.live(&target) else { return };
        let (name, conn_id, driver) = (live.name.clone(), live.conn_id, live.driver.clone());
        if !self.meta(&driver).map_or(false, |m| m.caps.metrics) {
            self.metrics.clear();
            self.metrics_conn = Some(name);
            self.metrics_error =
                Some(format!("the {driver} driver has nothing to report about its server"));
            return;
        }
        self.metrics_inflight = true;
        self.ask(
            Pending::Metrics { conn: name },
            "instance.metrics",
            json!({ "conn_id": conn_id }),
        );
    }

    /// Repeat it, but ONLY while the panel is open and only at a rate somebody
    /// chose. Leaving the panel stops it, because this is the only place it is
    /// called from and it checks.
    fn poll_metrics(&mut self) {
        if self.rail != Rail::Instance || self.metrics_inflight || self.conns.is_empty() {
            return;
        }
        // The panel being open is the request. A session restored WITH it open
        // is somebody who left it open, so it reads once — and then only again
        // at a rate they chose.
        if self.metrics_at.is_none() && self.metrics_error.is_none() {
            self.ask_metrics();
            return;
        }
        let Some(every) = self.metrics_every else { return };
        let due = self.metrics_at.map_or(true, |t| t.elapsed() >= every);
        if due {
            self.ask_metrics();
        }
    }

    // ── the builder ─────────────────────────────────────────────────────────

    /// Put the builder's choices into the statement box, through the DRIVER's
    /// template. The builder never writes SQL itself; it supplies a projection
    /// and a list of clauses, and the template decides where they go.
    fn sync_builder(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let driver = self.tabs[idx].driver.clone();
        let Some(meta) = self.meta(&driver).cloned() else { return };
        let Some((name, schema)) = self.tabs[idx].object.clone() else { return };

        // Read the typed values back before rendering, or the statement lags a
        // keystroke behind what is on the screen.
        for i in 0..self.tabs[idx].builder.conds.len() {
            if let Some(input) = self.inputs.get(&(tab_id, format!("b:{i}"))) {
                let v = input.read(cx).text().to_string();
                if let Some(c) = self.tabs[idx].builder.conds.get_mut(i) {
                    c.value = v;
                }
            }
        }

        let kind = self.tabs[idx].kind.clone();
        let (columns, clauses) = self.tabs[idx].builder.render(&meta.builder_ops);
        let sql = lode_proto::render_full(
            meta.browse_for(&kind),
            &name,
            schema.as_deref(),
            PAGE,
            &[],
            &clauses,
            &columns,
        );
        self.tabs[idx].sql = sql.clone();
        self.tabs[idx].rendered = sql.clone();
        if let Some(input) = self.inputs.get(&(tab_id, String::new())) {
            input.update(cx, |i, cx| i.set_text(sql, cx));
        }
    }

    fn add_condition(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let first = self
            .tabs[idx]
            .meta_columns
            .first()
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let i = self.tabs[idx].builder.conds.len();
        self.tabs[idx].builder.conds.push(Cond { col: first, op: 0, value: String::new() });
        let colors = input_colors(&self.theme);
        let input = cx.new(|cx| TextInput::new(cx, "value", colors));
        self.inputs.insert((tab_id, format!("b:{i}")), input);
        self.sync_builder(tab_id, cx);
    }

    /// Step a condition to the next column. A list of the object's columns is
    /// all there is to choose from — a free-text column name would be a place to
    /// mistype one and find out at run time.
    fn cycle_cond_column(&mut self, tab_id: u64, i: usize) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let names: Vec<String> = self.tabs[idx].meta_columns.iter().map(|c| c.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        if let Some(c) = self.tabs[idx].builder.conds.get_mut(i) {
            let at = names.iter().position(|n| *n == c.col).map_or(0, |p| (p + 1) % names.len());
            c.col = names[at].clone();
        }
    }

    /// none \u{2192} first column ascending \u{2192} descending \u{2192} next column \u{2026}
    fn cycle_order(&mut self, tab_id: u64) {
        let Some(idx) = self.tabs.iter().position(|x| x.id == tab_id) else { return };
        let names: Vec<String> = self.tabs[idx].meta_columns.iter().map(|c| c.name.clone()).collect();
        if names.is_empty() {
            return;
        }
        let b = &mut self.tabs[idx].builder;
        b.order = match b.order.take() {
            None => Some((names[0].clone(), true)),
            Some((c, true)) => Some((c, false)),
            Some((c, false)) => {
                let at = names.iter().position(|n| *n == c).map_or(0, |p| p + 1);
                if at >= names.len() {
                    None
                } else {
                    Some((names[at].clone(), true))
                }
            }
        };
    }

    // ── writing a row ───────────────────────────────────────────────────────

    /// Ask the ENGINE what a change would do. Nothing is written by this.
    fn propose_write(
        &mut self,
        tab_id: u64,
        row: usize,
        set: Vec<(String, Value)>,
        delete: bool,
        _cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let tab = &self.tabs[idx];
        if !self.meta(&tab.driver).map_or(false, |m| m.caps.edit) {
            self.toast(
                format!("the {} driver does not edit rows", tab.driver),
                true,
            );
            return;
        }
        let key = match row_key(tab, row) {
            Ok(k) => k,
            Err(why) => {
                // The refusal IS the message. Not a disabled button with no
                // explanation, which teaches nothing.
                self.toast(why, true);
                return;
            }
        };
        let Some(conn_id) = self.live(&tab.conn_name).map(|l| l.conn_id) else {
            self.toast(format!("{} is not connected", tab.conn_name), true);
            return;
        };
        let Some((name, schema)) = tab.object.clone() else { return };
        let params = json!({
            "conn_id": conn_id,
            "obj": { "name": name, "schema": schema },
            "key": key,
            "set": set,
            "delete": delete,
        });
        self.ask(
            Pending::RowPlan { tab: tab_id, row, key, set, delete },
            "row.write",
            params,
        );
    }

    /// Repeat the write the engine described, with `confirm`.
    fn apply_write(&mut self, tab_id: u64, _cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let Some(p) = self.tabs[idx].pending_write.clone() else { return };
        let tab = &self.tabs[idx];
        let Some(conn_id) = self.live(&tab.conn_name).map(|l| l.conn_id) else { return };
        let Some((name, schema)) = tab.object.clone() else { return };
        self.ask(
            Pending::RowApply { tab: tab_id },
            "row.write",
            json!({
                "conn_id": conn_id,
                "obj": { "name": name, "schema": schema },
                "key": p.key,
                "set": p.set,
                "delete": p.delete,
                "confirm": true,
            }),
        );
    }

    /// Start editing one cell, loading the input with what is there now.
    /// Put the cursor on a row.
    ///
    /// A click used to go straight to `begin_edit`, so clicking a cell either
    /// opened an editor or raised a toast explaining why it could not — and
    /// there was no way to simply SAY WHICH ROW, which is what a click on a
    /// grid means everywhere else. Selecting is the first click's job; editing
    /// is the second click on a cell of a row already chosen.
    fn select_row(&mut self, tab_id: u64, row: usize, cx: &mut Context<Self>) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            if row >= t.rows.len() {
                return;
            }
            t.selected = Some(row);
            // A detail panel that is open is showing "the row", not "that row".
            if t.detail.is_some() {
                t.detail = Some(row);
            }
        }
        cx.notify();
    }

    /// A click on a cell: choose the row, and edit only if the row was already
    /// chosen. The reason a cell cannot be edited goes in the STATUS line, not
    /// in a toast — a toast for something somebody merely clicked past is noise,
    /// and this one fired on every click on a key column.
    fn click_cell(&mut self, tab_id: u64, row: usize, col: usize, cx: &mut Context<Self>) {
        let already = self
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map_or(false, |t| t.selected == Some(row));
        // The CELL cursor goes where the click did, whichever of the two
        // things the click turns out to mean: `ctrl-c` after a click must copy
        // the cell that was clicked.
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            if let Some(at) = t.grid_columns().iter().position(|(i, _)| *i == col) {
                t.cursor_col = at;
            }
        }
        if !already {
            self.select_row(tab_id, row, cx);
            return;
        }
        self.begin_edit(tab_id, row, col, cx);
    }

    fn begin_edit(&mut self, tab_id: u64, row: usize, col: usize, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        if !column_editable(&self.tabs[idx], col) {
            let why = match self.tabs[idx].columns.get(col) {
                Some(c) if self.tabs[idx].meta_columns.iter().any(|m| m.name == c.name) => {
                    format!("{} is part of the key — changing it would move the row", c.name)
                }
                Some(c) => format!("{} is not a column of the table, so there is nothing to write it to", c.name),
                None => "no such column".into(),
            };
            self.status = why;
            cx.notify();
            return;
        }
        if let Err(why) = row_key(&self.tabs[idx], row) {
            self.toast(why, true);
            return;
        }
        let now = self.tabs[idx]
            .rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(|v| cell_text(v).unwrap_or_default())
            .unwrap_or_default();
        let colors = input_colors(&self.theme);
        let input = cx.new(|cx| {
            let mut i = TextInput::new(cx, "", colors);
            i.set_text(now, cx);
            i
        });
        // Enter commits. The grid's editor has no room for a tick beside it —
        // a row there is one line tall — so the keyboard has to be a complete
        // way through, and it is the one people reach for anyway.
        cx.subscribe(&input, move |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Submit) {
                this.commit_edit(tab_id, cx);
                cx.notify();
            }
        })
        .detach();
        self.inputs.insert((tab_id, "edit".into()), input);
        self.tabs[idx].editing = Some((row, col));
        // Focus is taken on the next frame, once the field exists in the tree.
        cx.defer(move |cx| {
            cx.refresh_windows();
        });
    }

    /// Take what was typed and ask about it. A value is sent as TEXT unless it
    /// parses as a number or is literally `null` — the engine binds whatever it
    /// gets, so what matters here is not inventing a type the person did not.
    fn commit_edit(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let Some((row, col)) = self.tabs[idx].editing else { return };
        let Some(input) = self.inputs.get(&(tab_id, "edit".into())) else { return };
        let typed = input.read(cx).text().to_string();
        let Some(name) = self.tabs[idx].columns.get(col).map(|c| c.name.clone()) else { return };
        let was = self.tabs[idx].rows.get(row).and_then(|r| r.get(col)).cloned();
        let value = parse_cell(&typed, was.as_ref());
        if Some(&value) == was.as_ref() {
            self.tabs[idx].editing = None;
            return;
        }
        self.propose_write(tab_id, row, vec![(name, value)], false, cx);
    }

    /// Open one document for editing, as a whole.
    ///
    /// Every writable top-level field gets a box, filled in. The alternative —
    /// a JSON text editor — would mean building a multi-line text widget inside
    /// the same component that masks passwords, and a parse error would become
    /// a new way to lose an edit. A document is a set of fields; this edits the
    /// set, and sends it as one write.
    fn begin_doc_edit(&mut self, tab_id: u64, row: usize, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        if !self.meta(&self.tabs[idx].driver).map_or(false, |m| m.caps.edit) {
            let d = self.tabs[idx].driver.clone();
            self.toast(format!("the {d} driver does not edit rows"), true);
            return;
        }
        if let Err(why) = row_key(&self.tabs[idx], row) {
            self.toast(why, true);
            return;
        }
        let colors = input_colors(&self.theme);
        let cols: Vec<(usize, String)> = self.tabs[idx]
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.name.clone()))
            .collect();
        for (ci, name) in cols {
            if !column_editable(&self.tabs[idx], ci) {
                continue;
            }
            let now = self.tabs[idx]
                .rows
                .get(row)
                .and_then(|r| r.get(ci))
                .map(|v| cell_text(v).unwrap_or_default())
                .unwrap_or_default();
            let input = cx.new(|cx| {
                let mut i = TextInput::new(cx, "", colors);
                i.set_text(now, cx);
                i
            });
            self.inputs.insert((tab_id, format!("doc-{name}")), input);
        }
        self.tabs[idx].editing_doc = Some(row);
        cx.notify();
    }

    /// Send every field that CHANGED, as one write.
    ///
    /// Unchanged fields are left out rather than rewritten with what they
    /// already hold: a write that touches a column somebody did not edit is a
    /// write they did not ask for, and it would show up in the audit log as one.
    fn commit_doc_edit(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) else { return };
        let Some(row) = self.tabs[idx].editing_doc else { return };
        let cols: Vec<(usize, String)> = self.tabs[idx]
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.name.clone()))
            .collect();
        let mut set: Vec<(String, Value)> = Vec::new();
        for (ci, name) in cols {
            let Some(input) = self.inputs.get(&(tab_id, format!("doc-{name}"))) else { continue };
            let typed = input.read(cx).text().to_string();
            let was = self.tabs[idx].rows.get(row).and_then(|r| r.get(ci)).cloned();
            let value = parse_cell(&typed, was.as_ref());
            if Some(&value) != was.as_ref() {
                set.push((name, value));
            }
        }
        if set.is_empty() {
            self.tabs[idx].editing_doc = None;
            self.toast("nothing changed".to_string(), false);
            cx.notify();
            return;
        }
        self.propose_write(tab_id, row, set, false, cx);
    }

    /// Put one run in the engine's history, once.
    ///
    /// The history is the ENGINE's, so what this client runs shows up in the
    /// editor's log too. Filed when it is OVER, with the state it reached:
    /// writing it at launch would mean every row said "running".
    fn record_run(&mut self, tab_id: u64, state: &str, ms: u64, rows: Option<u64>) {
        let found = self.tabs.iter_mut().find(|t| t.id == tab_id).and_then(|t| {
            if t.recorded || t.sql.trim().is_empty() {
                return None;
            }
            t.recorded = true;
            let what = if t.ran.trim().is_empty() { t.sql.clone() } else { t.ran.clone() };
            Some((what.trim().to_string(), t.conn_name.clone(), t.driver.clone()))
        });
        let Some((sql, conn, driver)) = found else { return };
        self.ask(
            Pending::Ignore("history"),
            "store.history.record",
            json!({
                "conn": conn, "driver": driver, "statement": sql,
                "state": state, "ms": ms, "rows": rows
            }),
        );
    }

    /// Let go of a call the client no longer has a tab for.
    ///
    /// A call that is still WORKING is cancelled and nothing more. Releasing it
    /// in the same breath would be wrong in a way that is easy to miss: release
    /// drops the engine's handle, and dropping a handle is not the same as
    /// telling a query to stop — and the two requests are separate lines, so
    /// the release can be read first and the cancel then finds nothing. The
    /// engine answers a cancel with a `query.state`, and the release happens
    /// there (see the orphan branch) once there is nothing left to stop.
    fn let_go_of_call(&mut self, call_id: u64, running: bool) {
        // NOT while an export is reading it. The export walks the engine's
        // buffer for this very call; releasing it would end the file halfway
        // with "no such call", and the file would look finished.
        if self.exporting.as_ref().map(|e| e.call_id) == Some(call_id) {
            self.calls.remove(&call_id);
            return;
        }
        self.calls.remove(&call_id);
        if running {
            self.ask(Pending::Ignore("cancel"), "query.cancel", json!({ "call_id": call_id }));
        } else {
            self.call_tasks.remove(&call_id);
            self.ask(Pending::Ignore("release"), "query.release", json!({ "call_id": call_id }));
        }
    }

    fn cancel_active(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let Some(call_id) = tab.call_id else { return };
        self.ask(Pending::Ignore("cancel"), "query.cancel", json!({ "call_id": call_id }));
    }

    fn clear_finished(&mut self) {
        self.tasks.retain(|t| t.running());
    }

    // ── painting ─────────────────────────────────────────────────────────────

    /// Switch theme now, and remember it.
    ///
    /// Everything is drawn from `self.theme` each frame, so there is nothing to
    /// repaint by hand: one assignment and the next frame is the new theme. The
    /// input fields are the exception — they cache their colours — so they are
    /// handed the new ones here.
    pub fn use_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(path) = self.themes.get(name).cloned() else { return };
        match theme::load(&path) {
            Ok(t) => {
                self.theme = t;
                self.theme_name = name.to_string();
                let colors = input_colors(&self.theme);
                for input in self.all_inputs() {
                    input.update(cx, |i, cx| i.set_colors(colors, cx));
                }
                // The SEPARATE WINDOWS too. They are handed a theme when they
                // open and were never told about a change, so a connection or
                // tunnel window left open through one kept the old palette
                // entirely — not just its fields.
                let th = self.theme.clone();
                if let Some(h) = self.form_window.clone() {
                    let _ = h.update(cx, |f, _, cx| f.set_theme(th.clone(), cx));
                }
                if let Some(h) = self.profile_window.clone() {
                    let _ = h.update(cx, |f, _, cx| f.set_theme(th.clone(), cx));
                }
                if let Some(h) = self.settings_window.clone() {
                    let _ = h.update(cx, |f, _, cx| f.set_theme(th.clone(), cx));
                }
                // And every open statement editor: it draws the statement in
                // the theme's own syntax colours, so a theme change that missed
                // it would leave the one place with the most colour in it
                // wearing the old palette.
                let eds: Vec<_> = self.editors.values().cloned().collect();
                for ed in eds {
                    let (th, co) = (self.theme.clone(), colors);
                    ed.update(cx, |e, cx| e.set_colors(co, th, cx));
                }
                self.remember();
                self.toast(format!("theme: {name}"), false);
                cx.notify();
            }
            // A theme file that will not parse says WHICH file and why, rather
            // than leaving the old one up with no explanation.
            Err(e) => {
                self.toast(format!("{name}: {e}"), true);
                self.log_push("error", format!("{name}: {e}"));
            }
        }
    }

    /// Everything the settings window shows of this window's own state.
    ///
    /// Read FRESH on every paint rather than copied in when the window opens: a
    /// copy is a second truth, and the first time somebody changed the text
    /// size from the keyboard the settings window would be showing the old
    /// number beside a stepper that moves the new one.
    pub fn appearance(&self) -> crate::settings::Appearance {
        crate::settings::Appearance {
            keys: self.key_over.clone(),
            theme_name: self.theme_name.clone(),
            themes: self.themes.keys().cloned().collect(),
            theme_dirs: theme::theme_dirs().iter().map(|d| d.display().to_string()).collect(),
            base: self.scale.base(),
            icon: self.scale.icon_size(),
            lang: self.lang,
            json_indent: self.json_indent,
            font_family: self.font_family.clone(),
            icon_family: self.icon_family.clone(),
        }
    }

    /// Ask the engine to resolve a sign-in for real.
    ///
    /// "Usable" is read off a config file; this is the engine actually trying
    /// it. A provider that reads as usable and fails when picked is what the
    /// Access page exists to prevent, and only the engine can tell the two
    /// apart.
    pub fn access_test(&mut self, profile: &str) {
        self.ask(
            Pending::AccessTest { profile: profile.to_string() },
            "access.test",
            json!({ "profile": profile }),
        );
        self.status = format!("testing {profile}\u{2026}");
    }

    /// Store a sign-in as a reusable profile.
    ///
    /// The engine stores the profile's NAME, never its key: the credential
    /// stays where the AWS tools rotate it.
    pub fn access_import(&mut self, profile: &str) {
        self.ask(
            Pending::AccessImport { profile: profile.to_string() },
            "access.import",
            json!({ "profile": profile }),
        );
    }

    /// Read an older client's store, and say what came across.
    pub fn import_store(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            // A cancelled dialog is not an error, and neither is a dialog the
            // platform refuses to open: nothing was chosen, so nothing happens.
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    let path = path.to_string_lossy().to_string();
                    let _ = this.update(cx, |w: &mut Workspace, _| {
                        w.ask(Pending::StoreImport, "store.import", json!({ "path": path }));
                    });
                }
            }
        })
        .detach();
    }

    /// Change what a key does, or put its default back.
    ///
    /// REBOUND at once, not on the next start: a setting that needs a restart
    /// is a setting nobody believes they have changed. gpui's bindings are
    /// replaced wholesale, which is why everything is bound again rather than
    /// one line being patched.
    pub fn set_key_override(&mut self, action: &str, keystroke: &str, cx: &mut Context<Self>) {
        let want = keystroke.trim().to_string();
        if want.is_empty() {
            self.key_over.remove(action);
        } else if !crate::app::valid_keystroke(&want) {
            self.toast(format!("{want:?} is not a keystroke"), true);
            return;
        } else {
            // WHO ELSE has it. Two actions on one key is a key that does
            // whichever gpui resolves first, which is not something to find out
            // by pressing it.
            let clash = KEYMAP
                .iter()
                .find(|(_, _, name, default)| {
                    *name != action
                        && self.key_over.get(*name).map(String::as_str).unwrap_or(default) == want
                })
                .map(|(_, label, _, _)| *label);
            if let Some(other) = clash {
                self.toast(format!("{want} is already {other}"), true);
                return;
            }
            self.key_over.insert(action.to_string(), want);
        }
        self.remember();
        // The session is written on a debounce and the binding is read from the
        // session at startup; this one is written NOW, because a key changed
        // and then lost in a crash is a key somebody changed twice.
        self.write_session();
        let over = self.key_over.clone();
        bind_changeable(cx, &over);
        self.poke_settings(cx);
        cx.notify();
    }

    /// Look for theme files again, and say what changed. Folders are read at
    /// startup, and a theme dropped in afterwards is the ordinary case.
    pub fn rescan_themes(&mut self, cx: &mut Context<Self>) {
        let before = self.themes.len();
        self.themes = theme::discover();
        let now = self.themes.len();
        self.toast(format!("{now} theme(s) found, was {before}"), false);
        self.poke_settings(cx);
        cx.notify();
    }

    pub fn set_base(&mut self, base: f32, cx: &mut Context<Self>) {
        self.scale = self.scale.with_base(base);
        self.remember();
        self.poke_editors(cx);
        self.poke_settings(cx);
        cx.notify();
    }

    pub fn set_icon(&mut self, size: f32, cx: &mut Context<Self>) {
        self.scale = self.scale.with_icon(size);
        self.remember();
        self.poke_editors(cx);
        self.poke_settings(cx);
        cx.notify();
    }

    pub fn reset_scale(&mut self, cx: &mut Context<Self>) {
        self.scale = Metrics::new(crate::metrics::DEFAULT_BASE);
        self.remember();
        self.poke_editors(cx);
        self.poke_settings(cx);
        cx.notify();
    }

    pub fn set_lang(&mut self, lang: Lang, cx: &mut Context<Self>) {
        self.lang = lang;
        self.relabel_inputs(cx);
        // The SEPARATE WINDOWS too. A connection window left open through a
        // language change kept its English labels, which reads as a half-done
        // translation rather than as a window nobody told.
        if let Some(h) = self.form_window.clone() {
            let _ = h.update(cx, |f, _, cx| f.set_lang(lang, cx));
        }
        self.remember();
        // The settings window reads the language off this one as it paints, so
        // it needs only the nudge.
        self.poke_settings(cx);
        cx.notify();
    }

    pub fn set_json_indent(&mut self, n: u8, cx: &mut Context<Self>) {
        self.json_indent = n;
        self.remember();
        self.poke_settings(cx);
        cx.notify();
    }

    /// The saved profiles of one kind, and who points at each — what the
    /// settings window lists.
    pub fn profile_rows(&self, kind: &str) -> Vec<ProfileRow> {
        self.profiles.get(kind).cloned().unwrap_or_default()
    }

    /// What a separate window inherits from this one: the palette, the scale,
    /// the two fonts and the language. See `widgets::Look`.
    pub fn look(&self) -> crate::widgets::Look {
        crate::widgets::Look {
            theme: self.theme.clone(),
            scale: self.scale,
            text_font: self.font_family.clone(),
            icon_font: self.icon_family.clone(),
            lang: self.lang,
        }
    }

    /// What the engine has said, for the settings window's reporting pages.
    pub fn facts(&self) -> crate::settings::Facts {
        let pair = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        crate::settings::Facts {
            proto: self.hello.as_ref().map(|h| h.proto).unwrap_or(0),
            version: self
                .hello
                .as_ref()
                .and_then(|h| h.version.clone())
                .unwrap_or_default(),
            engine_binary: self
                .engine
                .as_ref()
                .map(|e| e.binary().display().to_string())
                .unwrap_or_default(),
            store_path: self.store_path.clone(),
            store_ok: self.store_ok,
            audit_mode: self.audit_mode.clone(),
            external: self
                .hello
                .as_ref()
                .map(|h| h.external.clone())
                .unwrap_or_default()
                .iter()
                .map(|e| {
                    (
                        pair(e, "name"),
                        pair(e, "kind"),
                        e.get("ok").and_then(Value::as_bool).unwrap_or(false),
                        pair(e, "error"),
                    )
                })
                .collect(),
            access: self
                .access
                .iter()
                .map(|a| {
                    (
                        pair(a, "name"),
                        pair(a, "kind"),
                        a.get("usable").and_then(Value::as_bool).unwrap_or(false),
                        pair(a, "why"),
                    )
                })
                .collect(),
            approvals: self
                .approvals
                .iter()
                .map(|a| {
                    (
                        a.get("id")
                            .map(|x| x.to_string().trim_matches('"').to_string())
                            .unwrap_or_default(),
                        pair(a, "statement"),
                        pair(a, "state"),
                    )
                })
                .collect(),
        }
    }

    /// Hand the open editors the new sizes. Their line height is the scale's,
    /// and an editor still laying lines out at the old one draws them over each
    /// other.
    fn poke_editors(&mut self, cx: &mut Context<Self>) {
        let ms = self.scale;
        let eds: Vec<_> = self.editors.values().cloned().collect();
        for ed in eds {
            ed.update(cx, |e, cx| e.set_scale(ms, cx));
        }
    }

    /// Hand the settings window the sizes again and ask it to repaint.
    ///
    /// It keeps its own `Metrics` so it can draw without reaching back on every
    /// row; the cost of that is this call, and a setter that forgets it leaves
    /// a window whose own text does not change when it changes everything else.
    fn poke_settings(&mut self, cx: &mut Context<Self>) {
        let ms = self.scale;
        let look = self.appearance();
        let users: std::collections::BTreeMap<String, Vec<String>> = self
            .profile_users
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let facts = self.facts();
        if let Some(h) = self.settings_window.clone() {
            let _ = h.update(cx, |w, _, cx| {
                w.set_scale(ms, cx);
                w.look_changed(look, cx);
                w.users_arrived(users, cx);
                w.facts_arrived(facts, cx);
            });
        }
    }

    /// Put every placeholder in the chosen language.
    ///
    /// A placeholder is set when the field is BUILT, so it does not follow a
    /// language change on its own — the filter box stayed in English while the
    /// panel around it turned Bulgarian, which reads as a half-finished
    /// translation rather than as a field nobody re-labelled.
    fn relabel_inputs(&self, cx: &mut Context<Self>) {
        for (field, key) in [
            (&self.filter, "panel.filter"),
            (&self.history_filter, "panel.search_history"),
            (&self.audit_filter, "panel.search_audit"),
        ] {
            let text = self.lang.tr(key);
            field.update(cx, |i, cx| i.set_placeholder(text, cx));
        }
    }

    /// Every text field in the window, so a theme change reaches all of them.
    /// How many live fields there are, for the diagnostic.
    ///
    /// A number instead of a description: the list below is kept by hand, and
    /// the one time a field was left out of it that field wore the old palette
    /// on a light theme while everything around it changed.
    pub fn input_count(&self) -> usize {
        self.all_inputs().len()
    }

    /// Every live field, so a theme change reaches all of them.
    ///
    /// This list is kept BY HAND and that is its weakness: the theme filter was
    /// once added to the window and not added here, so on a light theme it
    /// stayed dark — the one field still wearing the old palette. Anything
    /// added below must be added here, and `--check-panels` prints the count so
    /// a disagreement shows up as a number rather than as somebody noticing.
    fn all_inputs(&self) -> Vec<Entity<TextInput>> {
        let mut out = vec![
            self.save_name.clone(),
            self.filter.clone(),
            self.console.clone(),
            self.export_path.clone(),
            self.history_filter.clone(),
            self.audit_filter.clone(),
            self.palette_input.clone(),
            self.board_name.clone(),
            self.tile_text.clone(),
            self.board_time_col.clone(),
            self.board_time_from.clone(),
            self.board_time_to.clone(),
        ];
        out.extend(self.inputs.values().cloned());
        out
    }

    /// One icon, or a readable stand-in when the font has no such glyph.
    ///
    /// A missing glyph draws an empty rectangle, and a rectangle is read as a
    /// badly-sized icon rather than as a missing one. A letter says what the
    /// button is; a box says nothing.
    fn ic(&self, glyph: &'static str) -> SharedString {
        let ch = glyph.chars().next().unwrap_or(' ');
        if !self.missing_glyphs.contains(&ch) {
            return SharedString::from(glyph);
        }
        let fallback = GLYPHS
            .iter()
            .find(|(g, _)| *g == glyph)
            .map(|(_, f)| *f)
            .unwrap_or("?");
        SharedString::from(fallback)
    }

    /// A SMALL icon, for the places one sits beside a line of text.
    ///
    /// The same fixed box as the rail's, scaled to the text it stands next to:
    /// the point is not the size, it is that every glyph gets the same square,
    /// so a tree of tables and views does not look like a tree of two sizes.
    fn icon_inline(&self, glyph: &'static str, colour: Rgba) -> gpui::Div {
        let ms = self.scale;
        let box_w = (ms.base() * 1.25).round();
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .w(px(box_w))
            .h(px(box_w))
            .line_height(px(box_w))
            .font_family(self.icon_family.clone())
            .text_size(ms.small())
            .text_color(colour)
            .child(self.ic(glyph))
    }

    /// A "copy this" control.
    ///
    /// GPUI has no selectable text, so a value somebody can SEE and cannot TAKE
    /// is half useless — and taking a piece of JSON and putting it somewhere
    /// else is most of what the JSON view is for. Until there is selection,
    /// this is the way out.
    fn copy_el(
        &self,
        id: String,
        what: String,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (t, ms) = (self.t(), self.scale);
        div()
            .id(SharedString::from(id))
            .flex_none()
            .px_1()
            .cursor_pointer()
            .hover(|d| d.bg(t.hover_mark()))
            .text_size(ms.tiny())
            .text_color(t.accent())
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(what.clone()));
                this.toast("copied".to_string(), false);
                cx.notify();
            }))
    }

    /// A row ACTION: the pencil that edits, the cross that closes.
    ///
    /// Drawn at the icon scale rather than at a text step. These were `tiny` and
    /// `small`, which on a palette with a quiet accent came out nearly invisible
    /// in a narrow row — and an action nobody can see is an action nobody knows
    /// is there.
    ///
    /// Two further things it fixes: the colour is the ordinary foreground, so it
    /// is visible WITHOUT hovering (hover brightens it rather than revealing
    /// it), and the clickable square is bigger than the glyph, because a target
    /// the size of its own drawing is a target people miss.
    fn action_el(&self, glyph: &'static str, colour: Rgba) -> gpui::Div {
        let ms = self.scale;
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .w(px(ms.action_box()))
            .h(px(ms.action_box()))
            .line_height(px(ms.action_box()))
            .font_family(self.icon_family.clone())
            .text_size(ms.action())
            .text_color(colour)
            .child(self.ic(glyph))
    }

    /// An icon in a fixed square, centred both ways.
    ///
    /// The reported complaint, and its cause: Font Awesome's glyphs are drawn
    /// at different widths and ink heights, so six of them at one point size,
    /// each sitting on a text baseline, come out visibly different sizes. In a
    /// box of its own, centred, what differs is only the drawing inside it.
    fn icon_el(&self, glyph: &'static str, colour: Rgba) -> gpui::Div {
        let ms = self.scale;
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .w(px(ms.icon_box()))
            .h(px(ms.icon_box()))
            // The line box is the square, so the glyph is centred on the box
            // and not hung off a text baseline.
            .line_height(px(ms.icon_box()))
            // The ICON font, which is not the text font: chosen for having the
            // glyphs and drawing them at one width.
            .font_family(self.icon_family.clone())
            .text_size(ms.icon())
            .text_color(colour)
            .child(self.ic(glyph))
    }

    /// A field, at the size of whatever it stands among.
    ///
    /// A field had no size of its own, so it inherited the window's root size
    /// and came out the same everywhere — which in a narrow panel is bigger
    /// than the rows beside it. The placeholder is drawn at the field's size,
    /// the same as the value, so what a field says before anything is typed is
    /// the size of what it will say after. The placeholder is the LESS
    /// important of the two, not the more; the difference between them is
    /// carried by the colour.
    ///
    /// The height follows too — the element asks for one line box — so a field
    /// in a panel is a panel-sized field.
    fn field<E: IntoElement>(&self, size: gpui::Pixels, el: E) -> gpui::Div {
        div().text_size(size).child(el)
    }

    /// A tip for a control that is only an icon.
    ///
    /// Takes the catalogue KEY, not a sentence: the words are the ones the
    /// panel already calls itself, so a translation reaches both at once.
    fn tip(
        &self,
        key: &str,
        shortcut: Option<&'static str>,
    ) -> impl Fn(&mut gpui::Window, &mut gpui::App) -> gpui::AnyView + 'static {
        crate::tip::Tip::build(self.lang.tr(key), shortcut, self.theme.clone(), self.scale)
    }

    /// One phrase of the interface, in the chosen language.
    fn tr(&self, key: &str) -> SharedString {
        SharedString::from(self.lang.tr(key))
    }

    fn t(&self) -> &Theme {
        &self.theme
    }

    /// A label in the muted ink, which is most of the chrome.
    fn small(&self, text: impl Into<SharedString>, colour: Rgba) -> impl IntoElement {
        div().text_size(self.scale.small()).text_color(colour).child(text.into())
    }

    /// The scrolling part of a panel: everything above it (a heading, a search
    /// box, a reload link) stays put, and the rows move under it.
    ///
    /// `min_h_0` is the half that is easy to leave out: without it a flex child
    /// refuses to shrink below its content, so a long list pushes the heading
    /// and the search box off the top instead of scrolling beneath them.
    /// A panel body that does NOT scroll, for the case where something inside
    /// it does instead.
    fn panel_column(&self, id: &'static str) -> gpui::Stateful<gpui::Div> {
        div().id(id).flex().flex_col().flex_1().min_h_0().w_full().overflow_hidden()
    }

    fn panel_list(&self, id: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            // Vertical scrolling only. `overflow_y_scroll` alone leaves the X
            // axis visible, and a child is then free to size itself to its
            // content — which is exactly how a long statement ends up wider
            // than the panel and gets clipped with no ellipsis instead of
            // being laid out to fit and truncated.
            .overflow_x_hidden()
            .overflow_y_scroll()
    }

    /// One line of free text that must not escape its panel: it ends in an
    /// ellipsis at the panel's edge rather than running over the result area.
    ///
    /// NO `min_w_0` here, and that is the whole lesson of this helper. `min_w_0`
    /// says "you may shrink to nothing", which is right for a flex ITEM sitting
    /// beside a fixed-width sibling and wrong for the text itself: the first
    /// version put it here, every metadata line computed to zero width, and the
    /// panel rendered a column of bare ellipses. The permission to shrink goes
    /// on the wrapper; the text takes the width it is given and cuts itself to
    /// fit.
    /// `w` is a real width in pixels, not `w_full`, and the comment on
    /// `Metrics::line_w` says why: GPUI draws the ellipsis only when the text
    /// was measured against a definite width, and a percentage inside a flex
    /// chain is not one. With `w_full` the text was clipped at the panel's edge
    /// mid-glyph and the `…` never appeared.
    /// A [`line`] for a slot in a flex ROW.
    ///
    /// The extra wrapper is not decoration. A flex item is measured by the flex
    /// algorithm before its own width is settled, and the text inside it is
    /// sized against that measurement — so a `line` placed straight into a row
    /// gets clipped at the slot's edge with no ellipsis, while the same `line`
    /// as a block child of a block truncates correctly. The wrapper is the
    /// block.
    /// Would this text be CUT at this width?
    ///
    /// The whole point of the tooltip is that it appears where something is
    /// hidden and nowhere else. The margin leans toward "it fits", because a
    /// tip on a line that was never truncated is noise on every row.
    fn cut(&self, text: &str, size: gpui::Pixels, w: f32) -> bool {
        let chars = text.chars().count() as f32;
        chars * self.char_ratio * f32::from(size) > w * 1.02
    }

    /// `line_in`, with the full text on hover WHEN it is cut.
    fn line_tip(
        &self,
        text: impl Into<SharedString>,
        size: gpui::Pixels,
        colour: Rgba,
        w: f32,
        id: impl Into<SharedString>,
    ) -> gpui::AnyElement {
        let text: SharedString = text.into();
        if !self.cut(&text, size, w) {
            return self.line_in(text, size, colour, w).into_any_element();
        }
        let full = text.to_string();
        div()
            .id(id.into())
            .flex_none()
            .w(px(w))
            .overflow_hidden()
            .tooltip(crate::tip::Tip::build(full, None, self.theme.clone(), self.scale))
            .child(self.line(text, size, colour, w))
            .into_any_element()
    }

    fn line_in(&self, text: impl Into<SharedString>, size: gpui::Pixels, colour: Rgba, w: f32) -> impl IntoElement {
        div()
            .flex_none()
            .w(px(w))
            .overflow_hidden()
            .child(self.line(text, size, colour, w))
    }

    fn line(&self, text: impl Into<SharedString>, size: gpui::Pixels, colour: Rgba, w: f32) -> impl IntoElement {
        div()
            .w(px(w))
            .flex_none()
            .truncate()
            .text_size(size)
            .text_color(colour)
            .child(text.into())
    }

    /// Nothing to show, said as a sentence and a hint.
    ///
    /// Centred, quiet, and ALWAYS BOTH: a panel that says "no connections" and
    /// stops leaves somebody looking for the button; one that says nothing at
    /// all is indistinguishable from one that failed to load.
    fn empty_state(&self, title: &str, hint: &str) -> impl IntoElement {
        let (t, ms) = (self.t(), self.scale);
        div()
            .flex()
            .flex_col()
            .w_full()
            .items_center()
            .justify_center()
            .gap_1()
            .px_3()
            .py_3()
            .child(
                div()
                    .text_size(ms.body())
                    .text_color(t.muted())
                    .child(self.tr(title)),
            )
            .child(
                div()
                    .text_size(ms.small())
                    .text_color(t.disabled())
                    .child(self.tr(hint)),
            )
    }

    fn section(&self, text: &str) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .text_size(self.scale.tiny())
            .text_color(self.t().title())
            .child(SharedString::from(text.to_uppercase()))
    }

    fn rail_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let ms = self.scale;
        // The width comes OFF the icon, so a bigger icon widens the rail by
        // itself. A second number here would be a second place to set the same
        // thing, and the two would drift.
        let mut col = div()
            .flex()
            .flex_col()
            .items_center()
            .w(px(ms.rail_w()))
            .flex_none()
            .h_full()
            .bg(t.status())
            .border_r_1()
            .border_color(t.border())
            .py_2()
            // Spaced, so six of them read as six BUTTONS. Touching vertically,
            // they read as one column of marks.
            .gap(px(ms.rail_gap()));
        // The settings are NOT in this run. The other five change what the
        // panel shows; settings is not a view onto data, and putting it in the
        // same column says it is. It goes to the bottom, after a gap and a
        // rule, and it opens a window rather than taking the panel over.
        for mode in [Rail::Connections, Rail::Instance, Rail::History, Rail::Audit, Rail::Tasks] {
            let on = self.rail == mode;
            col = col.child(
                div()
                    .id(SharedString::from(mode.label()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .h(px(ms.rail_h()))
                    // The chosen one says so twice: a filled background AND a
                    // bar down its leading edge. One of the two alone was
                    // invisible on a palette whose surfaces sit 1.06 apart —
                    // which is most of them. The bar is now three pixels and
                    // full height, because at two on a narrow rail neither mark
                    // could be read.
                    .when(on, |d| d.bg(t.selected_mark()).border_l_3().border_color(t.accent()))
                    .when(!on, |d| d.border_l_3().border_color(t.status()))
                    // The buttons touch, so the hover has to be what says where
                    // one ends: it fills exactly one button's height.
                    .hover(|d| d.bg(t.hover_mark()))
                    .cursor_pointer()
                    // The name of the panel, from the same catalogue key its own
                    // heading uses — a rail of glyphs is otherwise learned only
                    // by pressing all six.
                    .tooltip(self.tip(mode.key(), None))
                    // In a box of its own, centred both ways, so the six glyphs
                    // come out the same optical size whatever the font draws.
                    .child(self.icon_el(mode.glyph(), if on { t.accent() } else { t.muted() }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // The SAME button again folds the panel away. A rail
                        // where every button only ever opens something gives no
                        // way to get the room back except a keystroke somebody
                        // has to know about.
                        if this.rail == mode && !this.panel_hidden {
                            this.panel_hidden = true;
                            this.remember();
                            cx.notify();
                            return;
                        }
                        this.panel_hidden = false;
                        this.rail = mode;
                        // The log is read here, so the count beside the task
                        // total goes back to nothing.
                        if mode == Rail::Tasks {
                            this.log_unread = 0;
                        }
                        if mode == Rail::History && this.history.is_empty() {
                            this.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                        }
                        if mode == Rail::Audit && this.audit.is_empty() {
                            this.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
                        }
                        // Opening the panel is the request. Closing it is why
                        // nothing asks again.
                        if mode == Rail::Instance {
                            this.ask_metrics();
                        }
                        this.remember();
                        cx.notify();
                    })),
            );
        }
        // Pushed to the bottom, with a rule above it. It opens the settings
        // WINDOW: a panel is a narrow column, and the things that belong in
        // settings — tunnels, proxies, hooks, drivers, MCP policies — are lists
        // beside detail, which is a shape a column cannot hold. That narrowness
        // is what the whole of today's trouble was about.
        col = col.child(div().flex_1());
        col = col.child(
            div()
                .w_full()
                .h(px(1.0))
                .flex_none()
                .bg(t.border()),
        );
        col.child(
            div()
                .id("rail-settings")
                .flex()
                .items_center()
                .justify_center()
                .w_full()
                .h(px(ms.rail_h()))
                .flex_none()
                .border_l_3()
                .border_color(t.status())
                .hover(|d| d.bg(t.hover_mark()))
                .cursor_pointer()
                .tooltip(self.tip("rail.settings", None))
                .child(self.icon_el("\u{f013}", t.muted()))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_settings(cx);
                })),
        )
    }

    fn panel_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let ms = self.scale;
        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(ms.panel_w()))
            .flex_none()
            .h_full()
            // Nothing in a panel may paint outside its column. Set HERE rather
            // than on each row, so it holds for the panels not written yet: a
            // long path or a long statement leaking over the result area is a
            // bug per panel if the rule lives in the rows, and impossible if it
            // lives in the container.
            .overflow_hidden()
            // The leading for everything in here, set ONCE on the container
            // rather than per row. A line box the size of the text leaves
            // nothing below the baseline, and the tails of `p`, `y` and `д` are
            // drawn below it — which is why they were meeting the bottom edge
            // and being cut. Inherited, so a row written later cannot forget.
            .line_height(ms.line_h())
            .bg(t.panel())
            .border_r_1()
            .border_color(t.border());

        match self.rail {
            Rail::Instance => {
                let who = self.metrics_conn.clone().unwrap_or_else(|| "nothing connected".into());
                panel = panel.child(self.section(&format!("Instance \u{2014} {who}")));

                // How often, said plainly and chosen explicitly. "off" is the
                // default because repeating a statistics query against
                // somebody else\u{2019}s production database is their decision.
                let mut rate = div().flex().items_center().gap_1().px_2().py_1();
                rate = rate.child(self.small("read every", t.muted()));
                for (label, every) in [
                    ("off", None),
                    ("2s", Some(Duration::from_secs(2))),
                    ("10s", Some(Duration::from_secs(10))),
                    ("60s", Some(Duration::from_secs(60))),
                ] {
                    let on = self.metrics_every == every;
                    rate = rate.child(
                        chip(t, ms, format!("rate-{label}"), label, on)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.metrics_every = every;
                                cx.notify();
                            })),
                    );
                }
                rate = rate.child(
                    btn(t, ms, "metrics-now", "\u{f021} now", Btn::Link)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask_metrics();
                            cx.notify();
                        })),
                );
                panel = panel.child(rate);

                if let Some(at) = self.metrics_at {
                    panel = panel.child(div().px_2().child(self.small(
                        format!("read {} ago", fmt_duration(at.elapsed())),
                        t.muted(),
                    )));
                }
                if let Some(e) = &self.metrics_error {
                    panel = panel.child(div().px_2().child(self.small(e.clone(), t.error())));
                }

                // Before the first read it drew an empty column under a
                // heading, which reads as a server that answered nothing
                // rather than as one nobody has asked yet.
                if self.metrics.is_empty() && self.metrics_error.is_none() {
                    return panel.child(self.empty_state("empty.instance", "empty.instance_hint"));
                }
                let mut group = String::new();
                let mut list = self.panel_list("instance-list");
                for row in &self.metrics {
                    if row.group != group {
                        group = row.group.clone();
                        list = list.child(self.section(&group));
                    }
                    list = list.child(
                        div()
                            .flex()
                            .items_center()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .justify_between()
                            .px_2()
                            .py(px(1.0))
                            // The NAME gives way first: a server that answers
                            // with a long path or a long version string is
                            // answering with the part worth reading.
                            .child(self.line_in(row.name.clone(), ms.small(), t.muted(), ms.line_w() - ms.base() * 9.0 - 8.0))
                            .child(self.line_in(row.value.clone(), ms.small(), t.fg(), ms.base() * 9.0)),
                    );
                }
                return panel.child(list);
            }
            Rail::History => {
                panel = panel.child(
                    btn(t, ms, "history-reload", format!("{}  {}", "\u{f021}", self.lang.tr("panel.reload_history")), Btn::Link)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask(Pending::History, "store.history.list", json!({ "limit": 200 }));
                            cx.notify();
                        })),
                );
                panel = panel.child(div().px_2().py_1().child(self.field(ms.small(), self.history_filter.clone())));
                let needle = self.history_filter.read(cx).text().to_string();
                let shown: Vec<&HistoryRow> = self
                    .history
                    .iter()
                    .filter(|h| {
                        needle.is_empty()
                            || fuzzy_score(&h.statement, &needle).is_some()
                            || h.conn
                                .as_deref()
                                .map_or(false, |c| fuzzy_score(c, &needle).is_some())
                    })
                    .collect();
                panel = panel.child(self.section(&if needle.is_empty() {
                    format!("History ({})", self.history.len())
                } else {
                    format!("History ({} of {})", shown.len(), self.history.len())
                }));
                if shown.is_empty() {
                    if self.history.is_empty() {
                        return panel.child(self.empty_state("empty.history", "panel.reload_history"));
                    }
                    panel = panel.child(div().px_2().child(self.small(
                        self.lang.tr("panel.nothing_matches"),
                        t.muted(),
                    )));
                }
                let mut list = self.panel_list("history-list");
                for h in shown.into_iter().take(300) {
                    let failed = h.state.as_deref() == Some("failed");
                    let conn = h.conn.clone().unwrap_or_default();
                    let stmt = h.statement.clone();
                    // One line, with the newlines flattened. The ellipsis comes
                    // from the layout, not from a character count: 70 characters
                    // of a wide font is wider than the panel.
                    let one: String = stmt.replace('\n', " ");
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("hist-{}", h.id)))
                            .flex()
                            .flex_col()
                            .w_full()
                            .overflow_hidden()
                            .px_2()
                            .py(px(2.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover_mark()))
                            .child(self.line_tip(
                                one,
                                ms.small(),
                                if failed { t.error() } else { t.fg() },
                                ms.line_w(),
                                format!("hist-s-{}", h.id),
                            ))
                            .child(self.line(
                                format!(
                                    "{} · {} ms{}",
                                    if conn.is_empty() { "—".into() } else { conn.clone() },
                                    h.ms.unwrap_or(0),
                                    h.rows.map(|r| format!(" · {r} row(s)")).unwrap_or_default()
                                ),
                                ms.small(),
                                t.muted(),
                                ms.line_w(),
                            ))
                            // One click LOADS it; two RUN it. A single click
                            // used to run, so a history of writes meant a
                            // click in a list repeated a write.
                            .on_click(cx.listener({
                                let (c, s) = (conn.clone(), stmt.clone());
                                move |this, ev: &gpui::ClickEvent, _, cx| {
                                    let title = crate::workspace::draft_title(&s);
                                    this.open_sql(&c, &title, &s, cx);
                                    if ev.click_count() >= 2 {
                                        if let Some(tb) = this.tabs.get(this.active) {
                                            let id = tb.id;
                                            this.run_tab(id, cx);
                                        }
                                    }
                                    cx.notify();
                                }
                            })),
                    );
                }
                return panel.child(list);
            }
            Rail::Audit => {
                panel = panel.child(
                    btn(t, ms, "audit-reload", format!("{}  {}", "\u{f021}", self.lang.tr("panel.reload_audit")), Btn::Link)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.ask(Pending::Audit, "store.audit.list", json!({ "limit": 300 }));
                            cx.notify();
                        })),
                );
                panel = panel.child(div().px_2().py_1().child(self.field(ms.small(), self.audit_filter.clone())));
                let needle = self.audit_filter.read(cx).text().to_string();
                let shown: Vec<&AuditRow> = self
                    .audit
                    .iter()
                    .filter(|a| {
                        needle.is_empty()
                            || fuzzy_score(&a.statement, &needle).is_some()
                            || fuzzy_score(&a.outcome, &needle).is_some()
                            || fuzzy_score(&a.path, &needle).is_some()
                            || a.conn.as_deref().map_or(false, |c| fuzzy_score(c, &needle).is_some())
                            || a.reason.as_deref().map_or(false, |r| fuzzy_score(r, &needle).is_some())
                    })
                    .collect();
                let stopped = shown.iter().filter(|a| a.stopped()).count();
                panel = panel.child(self.section(&if needle.is_empty() {
                    format!("Audit ({}, {} stopped)", self.audit.len(), stopped)
                } else {
                    format!("Audit ({} of {})", shown.len(), self.audit.len())
                }));
                // WAITING FOR AN ANSWER, above the log. The audit panel is
                // where "what was stopped" is read, and an approval is the
                // same question before the fact rather than after it.
                if !self.approvals.is_empty() {
                    panel = panel.child(self.section(&format!(
                        "{} ({})",
                        self.lang.tr("approve.title"),
                        self.approvals.len()
                    )));
                    for a in self.approvals.clone().iter().take(20) {
                        let id = a.get("id").and_then(Value::as_i64).unwrap_or(0);
                        let stmt = a
                            .get("statement")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let who = a.get("asker").and_then(Value::as_str).unwrap_or("an agent");
                        let conn = a.get("conn").and_then(Value::as_str).unwrap_or("");
                        let why = a.get("reason").and_then(Value::as_str).unwrap_or("");
                        panel = panel.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .mx_2()
                                .mb_1()
                                .p_2()
                                .border_1()
                                .border_color(t.warning())
                                .child(self.small(format!("{who} \u{2192} {conn}"), t.muted()))
                                // The STATEMENT in full, wrapped: an approval
                                // about a statement nobody can read whole is an
                                // approval given blind.
                                .child(
                                    div()
                                        .text_size(ms.small())
                                        .text_color(t.fg())
                                        .child(SharedString::from(stmt)),
                                )
                                .children((!why.is_empty()).then(|| {
                                    self.small(why.to_string(), t.warning())
                                }))
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .child(
                                            btn(
                                                t,
                                                ms,
                                                format!("appr-yes-{id}"),
                                                self.tr("approve.yes"),
                                                Btn::Danger,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.decide_approval(id, true);
                                                cx.notify();
                                            })),
                                        )
                                        .child(
                                            btn(
                                                t,
                                                ms,
                                                format!("appr-no-{id}"),
                                                self.tr("approve.no"),
                                                Btn::Secondary,
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.decide_approval(id, false);
                                                cx.notify();
                                            })),
                                        ),
                                ),
                        );
                    }
                    panel = panel.child(div().px_2().pb_1().child(
                        self.small(self.lang.tr("approve.note"), t.muted()),
                    ));
                }
                // Why a statement might be blank. The engine decides this, and a
                // reader who is not told reads the blank as a broken client.
                if self.audit_mode != "full" {
                    panel = panel.child(div().px_2().child(self.small(
                        if self.audit_mode == "shape" {
                            "this engine keeps only the SHAPE of a statement (LODE_AUDIT_STATEMENTS=shape)"
                        } else {
                            "this engine keeps no statement text (LODE_AUDIT_STATEMENTS=off)"
                        },
                        t.warning(),
                    )));
                }
                if shown.is_empty() {
                    if self.audit.is_empty() {
                        return panel.child(self.empty_state("empty.audit", "empty.audit_hint"));
                    }
                    panel = panel.child(div().px_2().child(self.small(
                        self.lang.tr("panel.nothing_matches"),
                        t.muted(),
                    )));
                }
                let mut list = self.panel_list("audit-list");
                for a in shown.into_iter().take(400) {
                    // Three outcomes, three colours, and `applied` is the quiet
                    // one: the rows worth finding are the ones that were stopped.
                    let colour = match a.outcome.as_str() {
                        "refused" => t.warning(),
                        "failed" => t.error(),
                        "cancelled" => t.muted(),
                        "connected" | "disconnected" => t.muted(),
                        _ => t.fg(),
                    };
                    let open = self.audit_open == Some(a.id);
                    let id = a.id;
                    // The record's LINES, as data — and the box's height from
                    // the count of them. It used to be two lines drawn into a
                    // box whose height was left to be inferred, and the
                    // inference came out at one: records overlapped, descenders
                    // were cut, and a hovered second line lay over its
                    // neighbour. `audit_parts` is tested, so the shape of this
                    // is checkable without a screen.
                    let parts = crate::workspace::audit_parts(&a, open);
                    let line_h = f32::from(ms.line_h());
                    let mut row = div()
                        .id(SharedString::from(format!("aud-{}", a.id)))
                        .flex()
                        .flex_col()
                        .w_full()
                        .flex_none()
                        .px_2()
                        .py(px(2.0))
                        // Space BETWEEN records, so thirty of them read as
                        // thirty things rather than as one block.
                        .mb(px(2.0))
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover_mark()));
                    // A closed record is exactly its lines; an open one is free
                    // to grow, because a wrapped reason is as tall as it needs.
                    if !open {
                        row = row.h(px(crate::workspace::audit_row_h(&parts, line_h, 2.0)));
                    }
                    for p in parts {
                        if p.wraps {
                            // The one place text WRAPS instead of being cut: a
                            // refusal shortened to fit is a refusal that has
                            // stopped explaining itself.
                            row = row.child(
                                div()
                                    .w(px(ms.line_w()))
                                    .flex_none()
                                    .px_1()
                                    .text_size(ms.small())
                                    .text_color(colour)
                                    .child(SharedString::from(p.text)),
                            );
                            continue;
                        }
                        match p.outcome {
                            // The first line: a fixed outcome column so every
                            // statement starts at the same edge, since a list
                            // scanned for what was STOPPED is scanned down one.
                            Some(outcome) => {
                                row = row.child(
                                    div()
                                        .flex()
                                        .flex_none()
                                        .h(px(line_h))
                                        .w(px(ms.line_w()))
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(px(ms.outcome_w()))
                                                .flex_none()
                                                .whitespace_nowrap()
                                                .text_size(ms.tiny())
                                                .text_color(colour)
                                                .child(SharedString::from(outcome)),
                                        )
                                        .child(self.line_tip(
                                            p.text,
                                            ms.small(),
                                            t.fg(),
                                            ms.line_w() - ms.outcome_w() - 8.0,
                                            format!("aud-s-{}", a.id),
                                        )),
                                );
                            }
                            None => {
                                row = row.child(
                                    div().flex_none().h(px(line_h)).child(self.line_tip(
                                        p.text,
                                        ms.small(),
                                        t.muted(),
                                        ms.line_w(),
                                        format!("aud-m-{}", a.id),
                                    )),
                                );
                            }
                        }
                    }
                    list = list.child(row.on_click(cx.listener(move |this, _, _, cx| {
                        this.audit_open = if this.audit_open == Some(id) { None } else { Some(id) };
                        cx.notify();
                    })));
                }
                return panel.child(list);
            }
            Rail::Tasks => {
                panel = panel.child(self.section(&format!("Tasks ({})", self.tasks.len())));
                // It lived on the tasks strip, which the bottom drawer replaced.
                // The panel is where the whole list is, so it is where emptying
                // it belongs.
                if self.tasks.iter().any(|x| !x.running()) {
                    panel = panel.child(
                        div().px_2().py_1().child(
                            btn(t, ms, "clear-finished", self.tr("tasks.clear_finished"), Btn::Quiet)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.clear_finished();
                                    cx.notify();
                                })),
                        ),
                    );
                }
                if self.tasks.is_empty() && self.log.is_empty() {
                    return panel.child(self.empty_state("empty.tasks", "empty.tasks_hint"));
                }
                let mut list = self.panel_list("tasks-list");
                for task in self.tasks.iter().rev().take(200) {
                    let (id, running, call) = (task.id, task.running(), task.call_id);
                    // The row, and — while it is running — a Stop on it. A list
                    // that shows a statement working and offers no way to stop
                    // it is a list of things to wait for; the tab it came from
                    // may not even be open any more.
                    let mut row = div()
                        .id(SharedString::from(format!("task-{id}")))
                        .w_full()
                        .overflow_hidden()
                        .px_2()
                        .py_1()
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover_mark()));
                    row = row.child(
                        div().flex_1().min_w_0().child(self.line(
                            format!("{} · {}", task.label, fmt_duration(task.duration())),
                            ms.small(),
                            if task.failed() { t.error() } else { t.muted() },
                            ms.line_w() - ms.base() * 3.0,
                        )),
                    );
                    if let (true, Some(call_id)) = (running, call) {
                        row = row.child(
                            div()
                                .id(SharedString::from(format!("stop-task-{id}")))
                                .flex_none()
                                .cursor_pointer()
                                .hover(|d| d.bg(t.hover_mark()))
                                .tooltip(self.tip("tip.stop_task", None))
                                .child(self.action_el("\u{f00d}", t.error()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ask(
                                        Pending::Ignore("cancel"),
                                        "query.cancel",
                                        json!({ "call_id": call_id }),
                                    );
                                    cx.notify();
                                })),
                        );
                    }
                    // A failure keeps its words. `Task::open` has always said a
                    // failed task starts expanded, and nothing ever drew the
                    // expansion, so the reason was kept and never shown.
                    let mut cell = div().w_full().flex().flex_col();
                    cell = cell.child(row.on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(t) = this.tasks.iter_mut().find(|t| t.id == id) {
                            t.open = !t.open;
                        }
                        cx.notify();
                    })));
                    if task.open {
                        if let Some(err) = task.error.clone() {
                            cell = cell.child(
                                div()
                                    .w_full()
                                    .px_2()
                                    .pb_1()
                                    .text_size(ms.tiny())
                                    .text_color(t.error())
                                    .child(SharedString::from(err)),
                            );
                        }
                    }
                    list = list.child(cell);
                }
                // The log, under the tasks. Everything the client learned and
                // had nowhere to say: a plaintext link, a classify it could not
                // run, a panel that failed to load.
                if !self.log.is_empty() {
                    panel = panel.child(list);
                    panel = panel.child(self.section(&format!("Log ({})", self.log.len())));
                    let mut lines = self.panel_list("log-list");
                    for (level, text) in self.log.iter().rev().take(200) {
                        let colour = match level.as_str() {
                            "error" => t.error(),
                            "warn" => t.warning(),
                            _ => t.muted(),
                        };
                        lines = lines.child(
                            div().w_full().overflow_hidden().px_2().py(px(1.0)).child(self.line(
                                text.clone(),
                                ms.small(),
                                colour,
                                ms.line_w(),
                            )),
                        );
                    }
                    return panel.child(lines);
                }
                return panel.child(list);
            }
            Rail::Connections => {}
        }

        panel = panel.child(div().px_2().py_1().child(self.field(ms.small(), self.filter.clone())));
        panel = panel.child(
            btn(t, ms, "new-conn", format!("{}  {}", "\u{f067}", self.lang.tr("panel.new_connection")), Btn::Link)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_form(None, cx);
                    cx.notify();
                })),
        );
        let filter = self.filter.read(cx).text().to_string();
        let connected = self.conns.len();
        // The saved connections AND every open connection's schema below them:
        // one list, because a deep tree is exactly what pushes a heading off the
        // top of a panel that is not scrolling.
        let mut list = self.panel_list("connections-list");
        list = list.child(self.section(&format!(
            "{} ({connected} {} · {} {})",
            self.lang.tr("panel.connections"),
            self.lang.tr("panel.connected"),
            self.saved.len().saturating_sub(connected),
            self.lang.tr("panel.idle")
        )));

        if self.saved.is_empty() {
            list = list.child(self.empty_state("empty.connections", "empty.connections_hint"));
        }
        // GROUPED, where somebody has filed them. The folder is the store's,
        // so the Neovim client and this one show the same arrangement — see
        // `store.connections.save`.
        let groups = crate::workspace::group_connections(&self.saved);
        let mut order: Vec<&SavedConnection> = Vec::new();
        for (group, names) in &groups {
            let shown: Vec<&SavedConnection> = names
                .iter()
                .filter_map(|n| self.saved.iter().find(|c| &c.name == n))
                .filter(|c| crate::workspace::matches_filter(&c.name, &filter))
                .collect();
            // A folder with nothing matching the filter is not drawn: a heading
            // over nothing is a folder that looks empty rather than filtered.
            if shown.is_empty() {
                continue;
            }
            if let Some(name) = group {
                let key = format!("group:{name}");
                let open = !self.shut_groups.contains(&key);
                list = list.child(
                    div()
                        .id(SharedString::from(key.clone()))
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .py(px(1.0))
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover_mark()))
                        .child(self.icon_inline(
                            if open { "\u{f0d7}" } else { "\u{f0da}" },
                            t.muted(),
                        ))
                        .child(self.small(
                            format!("{} ({})", name, shown.len()),
                            t.title(),
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.shut_groups.remove(&key) {
                                this.shut_groups.insert(key.clone());
                            }
                            this.remember();
                            cx.notify();
                        })),
                );
                if !open {
                    continue;
                }
            }
            order.extend(shown);
        }
        for c in order {
            let live = self.conns.iter().find(|l| l.name == c.name);
            let is_live = live.is_some();
            let is_front = self.tabs.get(self.active).map(|x| x.conn_name.as_str()) == Some(c.name.as_str());
            // The padlock, for an OPEN connection only: a saved connection that
            // is not connected has no transport yet, and a lock drawn over a
            // spec would be a promise about something that has not happened.
            let link = live.map(|l| crate::workspace::link_label(l.encrypted, l.tunneled));
            let name = c.name.clone();
            let name2 = c.name.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("conn-{}", c.name)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    // Tall enough for the text AND the action in it, derived
                    // rather than chosen — see `panel_row_h`.
                    .h(px(ms.panel_row_h()))
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover_mark()))
                    // The connection the TAB IN FRONT belongs to carries the
                    // accent edge; being connected is a filled background. Both
                    // were the same background before, so with four connections
                    // open nothing said which one the result on screen came
                    // from.
                    .when(is_live, |d| d.bg(t.selected_mark()))
                    .when(is_front, |d| d.border_l_2().border_color(t.accent()))
                    .when(!is_front, |d| {
                        d.border_l_2().border_color(gpui::transparent_black())
                    })
                    .child(
                        div()
                            // At `small`, not `tiny`: the dot is the one mark in
                            // this row that is read from across the screen.
                            .text_size(ms.small())
                            .text_color(if is_live { t.success() } else { t.muted() })
                            .child(SharedString::from(if is_live { "\u{f111}" } else { "\u{f10c}" })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.line_tip(
                            c.name.clone(),
                            ms.body(),
                            if is_live { t.fg() } else { t.muted() },
                            // The budget for the name: the driver's label, the
                            // pencil, the x, and the padlock when there is one.
                            ms.line_w() - ms.base() * if link.is_some() { 10.5 } else { 9.0 },
                            format!("cn-{}", c.name),
                        )),
                    )
                    .children(link.map(|l| {
                        div()
                            .id(SharedString::from(format!("lock-{}", c.name)))
                            .flex_none()
                            .text_size(ms.tiny())
                            // Red, not muted: a plaintext link is the one thing
                            // in this row somebody has to notice without
                            // hovering anything.
                            .text_color(if l.safe { t.muted() } else { t.error() })
                            .tooltip(self.tip(l.tip, None))
                            .child(SharedString::from(l.glyph))
                    }))
                    .child(div().flex_none().child(self.small(c.driver.clone(), t.muted())))
                    .child(
                        div()
                            .id(SharedString::from(format!("edit-{}", c.name)))
                            .flex_none()
                            .cursor_pointer()
                            // It was `tiny` and `muted` at the far edge, and
                            // went unfound: somebody who cannot see how to edit
                            // a connection concludes it cannot be edited. Now it
                            // is at the icon size, in the accent, and it lights
                            // up under the pointer.
                            .hover(|d| d.bg(t.hover_mark()))
                            .tooltip(self.tip("tip.edit_conn", None))
                            // The ordinary foreground, not the accent: on a
                            // palette with a quiet accent the pencil was barely
                            // separable from the panel it sat on.
                            .child(self.action_el("\u{f044}", t.fg()))
                            .on_click(cx.listener({
                                let (n, d, sp) = (c.name.clone(), c.driver.clone(), c.spec.clone());
                                move |this, _, _, cx| {
                                    this.open_form(Some((n.clone(), d.clone(), sp.clone())), cx);
                                    cx.notify();
                                }
                            })),
                    )
                    // Removing one was possible from nowhere at all: a
                    // connection added by mistake stayed in the list forever.
                    .child(
                        div()
                            .id(SharedString::from(format!("rm-{}", c.name)))
                            .flex_none()
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover_mark()))
                            .tooltip(self.tip("tip.remove_conn", None))
                            .child(self.action_el("\u{f00d}", t.muted()))
                            .on_click(cx.listener({
                                let n = c.name.clone();
                                move |this, _, _, cx| {
                                    // ASK, and ask the engine what would go with
                                    // it while the question is on screen.
                                    this.confirm_conn_rm = Some((n.clone(), None));
                                    this.ask(
                                        Pending::ConnQueryCount { name: n.clone() },
                                        "store.queries.list",
                                        json!({ "conn": n }),
                                    );
                                    cx.notify();
                                }
                            })),
                    )
                    // The RIGHT button offers everything this row can do, most
                    // of which had no control at all: rename by opening the
                    // window, remove from one small x, refresh from nowhere.
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener({
                            let (n, live) = (c.name.clone(), is_live);
                            // Folders are the store's, so an engine without the
                            // field cannot be asked for one.
                            let folders = self.hello.as_ref().map_or(false, |h| h.proto >= 16);
                            move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                this.open_menu(
                                    crate::menu::Target::Connection {
                                        name: n.clone(),
                                        live,
                                        folders,
                                    },
                                    ev.position,
                                    cx,
                                );
                            }
                        }),
                    )
                    // A double click was tried here and taken out again. The
                    // first click of a double click has already fired by the
                    // time the second arrives, so "open the settings" also
                    // connected or disconnected on the way — somebody reaching
                    // for the form got their connection toggled instead. A
                    // shortcut that does something else first is worse than no
                    // shortcut. The pencil and the palette are the two ways in.
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.conns.iter().any(|l| l.name == name) {
                            this.disconnect(&name);
                        } else {
                            this.connect(&name);
                        }
                        cx.notify();
                    })),
            );
            let _ = name2;

            // The question stands where the row is, with the count in it.
            if let Some((who, count)) = self.confirm_conn_rm.clone() {
                if who == c.name {
                    let why = self
                        .lang
                        .tr("conn.remove_ask")
                        .replace("{name}", &who)
                        .replace("{n}", &count.map(|n| n.to_string()).unwrap_or_else(|| "…".into()));
                    list = list.child(div().px_2().child(self.small(why, t.warning())));
                    let (yes, no) = (who.clone(), who.clone());
                    list = list.child(
                        div()
                            .flex()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .child(
                                btn(t, ms, format!("rm-yes-{who}"), self.tr("conn.remove_it"), Btn::Danger)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        // Disconnected first if it is open: an
                                        // engine connection outliving the saved
                                        // connection it came from is a session
                                        // nothing in the window can name.
                                        if this.conns.iter().any(|l| l.name == yes) {
                                            this.disconnect(&yes);
                                        }
                                        this.ask(
                                            Pending::ConnRemoved { name: yes.clone() },
                                            "store.connections.remove",
                                            json!({ "name": yes }),
                                        );
                                        this.confirm_conn_rm = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                btn(t, ms, format!("rm-no-{no}"), self.tr("conn.keep_it"), Btn::Quiet)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_conn_rm = None;
                                        cx.notify();
                                    })),
                            ),
                    );
                }
            }
        }

        // The schema of every open connection, each under its own heading, so
        // two connections can be browsed at once — which is why a tab carries
        // its connection.
        for live in &self.conns {
            list = list.child(self.section(&live.name));
            if live.nodes.is_empty() {
                list = list.child(div().px_2().child(self.small(self.tr("panel.loading"), t.muted())));
            }
            if !live.charts.is_empty() {
                list = list.child(self.section(self.lang.tr("panel.saved_charts")));
                for c in &live.charts {
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("sc-{}-{}", live.name, c.name)))
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl(px(ms.gutter()))
                            .pr_2()
                            .py(px(2.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover_mark()))
                            .child(self.icon_inline("\u{f080}", t.accent()))
                            .child(self.line_in(c.name.clone(), ms.small(), t.fg(), ms.line_w() - ms.base() - 14.0))
                            .on_click(cx.listener({
                                let (cn, n, sql, m) =
                                    (live.name.clone(), c.name.clone(), c.sql.clone(), c.measure.clone());
                                move |this, _, _, cx| {
                                    this.open_chart(&cn, &n, &sql, &m, cx);
                                    cx.notify();
                                }
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener({
                                    let (cn, n) = (live.name.clone(), c.name.clone());
                                    move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                        this.open_menu(
                                            crate::menu::Target::Saved {
                                                conn: cn.clone(),
                                                name: n.clone(),
                                                kind: crate::menu::SavedKind::Chart,
                                            },
                                            ev.position,
                                            cx,
                                        );
                                    }
                                }),
                            ),
                    );
                }
            }
            // Dashboards come before the saved charts, because a board is what
            // somebody opens and a chart is what a board is made of.
            list = list.child(self.section(self.lang.tr("panel.dashboards")));
            list = list.child(
                div()
                    .id(SharedString::from(format!("nb-{}", live.name)))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl(px(ms.gutter()))
                    .pr_2()
                    .py(px(2.0))
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover_mark()))
                    .tooltip(self.tip("tip.new_board", None))
                    .child(self.icon_inline("\u{f067}", t.accent()))
                    .child(self.line_in(self.tr("panel.new_dashboard"), ms.small(), t.accent(), ms.line_w() - ms.base() - 14.0))
                    .on_click(cx.listener({
                        let cn = live.name.clone();
                        move |this, _, _, cx| {
                            this.open_board(&cn, "", cx);
                        }
                    })),
            );
            for name in &live.boards {
                list = list.child(
                    div()
                        .id(SharedString::from(format!("db-{}-{}", live.name, name)))
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl(px(ms.gutter()))
                        .pr_2()
                        .py(px(2.0))
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover_mark()))
                        .tooltip(self.tip("tip.open_board", None))
                        .child(self.icon_inline("\u{f00a}", t.accent()))
                        .child(self.line_in(name.clone(), ms.small(), t.fg(), ms.line_w() - ms.base() - 14.0))
                        .on_click(cx.listener({
                            let (cn, n) = (live.name.clone(), name.clone());
                            move |this, _, _, cx| {
                                this.open_board(&cn, &n, cx);
                            }
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener({
                                let (cn, n) = (live.name.clone(), name.clone());
                                move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                    this.open_menu(
                                        crate::menu::Target::Saved {
                                            conn: cn.clone(),
                                            name: n.clone(),
                                            kind: crate::menu::SavedKind::Board,
                                        },
                                        ev.position,
                                        cx,
                                    );
                                }
                            }),
                        ),
                );
            }
            if !live.queries.is_empty() {
                list = list.child(self.section(self.lang.tr("panel.saved_queries")));
                for q in &live.queries {
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("sq-{}-{}", live.name, q.name)))
                            .flex()
                            .items_center()
                            .gap_1()
                            .pl(px(ms.gutter()))
                            .pr_2()
                            .py(px(2.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover_mark()))
                            .child(self.icon_inline("\u{f15c}", t.accent()))
                            .child(self.line_in(q.name.clone(), ms.small(), t.fg(), ms.line_w() - ms.base() - 14.0))
                            // The same as the history: one click loads the
                            // saved query, two run it.
                            .on_click(cx.listener({
                                let (c, n, s) = (live.name.clone(), q.name.clone(), q.sql.clone());
                                move |this, ev: &gpui::ClickEvent, _, cx| {
                                    this.open_sql(&c, &n, &s, cx);
                                    if ev.click_count() >= 2 {
                                        if let Some(tb) = this.tabs.get(this.active) {
                                            let id = tb.id;
                                            this.run_tab(id, cx);
                                        }
                                    }
                                    cx.notify();
                                }
                            }))
                            // Removing a saved query was possible from nowhere
                            // at all: the engine has had the method since
                            // protocol 2 and no control ever called it.
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener({
                                    let (c, n) = (live.name.clone(), q.name.clone());
                                    move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                        this.open_menu(
                                            crate::menu::Target::Saved {
                                                conn: c.clone(),
                                                name: n.clone(),
                                                kind: crate::menu::SavedKind::Query,
                                            },
                                            ev.position,
                                            cx,
                                        );
                                    }
                                }),
                            ),
                    );
                }
                list = list.child(self.section(self.lang.tr("panel.schema")));
            }
            for node in live
                .nodes
                .iter()
                .filter(|n| crate::workspace::node_matches(n, &filter))
            {
                list = list.child(self.node_el(&live.name, node, 0, cx));
            }
        }
        panel.child(list)
    }

    /// One schema node. A branch toggles; a leaf OPENS and runs.
    fn node_el(&self, conn: &str, node: &Node, depth: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let key = format!("{conn}/{}/{}", node.kind, node.name);
        let is_branch = !node.children.is_empty();
        // A table or a collection is a LEAF of the schema and still has an
        // inside: its fields and its indexes. The engine has answered both all
        // along and the tree stopped here.
        let is_object = matches!(node.kind.as_str(), "table" | "view" | "collection");
        let open = self.open_nodes.contains(&key);
        let glyph = match node.kind.as_str() {
            "database" => "\u{f1c0}",
            "schema" => "\u{f07b}",
            "table" => "\u{f0ce}",
            "view" => "\u{f06e}",
            // Its own glyph. A collection drew the DATABASE icon, so in a Mongo
            // tree the database and everything in it were the same mark and the
            // only difference was the indent.
            "collection" => "\u{f0c5}",
            _ => "\u{f0ce}",
        };

        let (conn_s, name_s, schema_s, kind_s, key_s) = (
            conn.to_string(),
            node.name.clone(),
            node.schema.clone(),
            node.kind.clone(),
            key.clone(),
        );

        let row = div()
            .id(SharedString::from(key.clone()))
            .flex()
            .items_center()
            .gap_1()
            .pl(px(ms.gutter() + depth as f32 * ms.indent()))
            .pr_2()
            .py(px(2.0))
            .cursor_pointer()
            .hover(|d| d.bg(t.hover_mark()))
            .child({
                // A table has TWO things to do: open it, and look inside it.
                // The row keeps the common one; the arrow is its own target for
                // the other, so neither can be mistaken for the other.
                let mark = if is_branch || is_object {
                    if open { "\u{f0d7}" } else { "\u{f0da}" }
                } else {
                    " "
                };
                let plain = div()
                    .w(px(ms.base()))
                    .flex_none()
                    .text_size(ms.tiny())
                    .text_color(t.muted())
                    .child(SharedString::from(mark));
                if !is_object {
                    return_plain(plain)
                } else {
                    let (k, c, n, sc, kd) = (
                        key.clone(),
                        conn.to_string(),
                        node.name.clone(),
                        node.schema.clone(),
                        node.kind.clone(),
                    );
                    plain
                        .id(SharedString::from(format!("ar-{key}")))
                        .cursor_pointer()
                        .tooltip(crate::tip::Tip::build(
                            "Fields and indexes",
                            None,
                            self.theme.clone(),
                            self.scale,
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.toggle_inside(&k, &c, &n, sc.as_deref(), &kd);
                                cx.notify();
                            }),
                        )
                        .into_any_element()
                }
            })
            .child(self.icon_inline(glyph, t.accent()))
            // A node is indented by its depth, so the room it has shrinks as
            // the tree goes deeper — the width has to say so, or a deep leaf
            // would be cut without an ellipsis again.
            .child(self.line_tip(
                node.name.clone(),
                ms.body(),
                t.fg(),
                (ms.line_w() - 8.0 - depth as f32 * 12.0 - ms.base() * 2.2).max(ms.base() * 3.0),
                format!("nd-{conn}-{}-{}", node.kind, node.name),
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if is_branch {
                        if this.open_nodes.contains(&key_s) {
                            this.open_nodes.remove(&key_s);
                        } else {
                            this.open_nodes.insert(key_s.clone());
                        }
                        cx.notify();
                        return;
                    }
                    // The row OPENS the object. Looking inside it is the arrow
                    // at the left, which has its own handler — guessing from
                    // the pointer's x would be the window making assumptions
                    // about its own layout.
                    this.open_object(&conn_s, &name_s, schema_s.as_deref(), &kind_s, cx);
                    cx.notify();
                }),
            )
            // And the right button: open, look inside, put the name in the
            // statement, copy the name, copy the DDL. Four of those five had no
            // control anywhere in the window.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let (c, n, sc, kd) = (
                        conn.to_string(),
                        node.name.clone(),
                        node.schema.clone(),
                        node.kind.clone(),
                    );
                    // Whether the DRIVER has DDL at all — the engine declares
                    // it, and offering a copy that answers nothing is worse
                    // than a dimmed line.
                    let has_ddl = self
                        .live(conn)
                        .and_then(|l| self.meta(&l.driver))
                        .map_or(false, |m| m.caps.ddl)
                        && is_object;
                    move |this, ev: &gpui::MouseDownEvent, _, cx| {
                        this.open_menu(
                            crate::menu::Target::Node {
                                conn: c.clone(),
                                name: n.clone(),
                                schema: sc.clone(),
                                kind: kd.clone(),
                                has_ddl,
                            },
                            ev.position,
                            cx,
                        );
                    }
                }),
            );

        let mut out = div().flex().flex_col().child(row);
        if is_branch && open {
            for child in &node.children {
                out = out.child(self.node_el(conn, child, depth + 1, cx));
            }
        }
        if is_object && open {
            out = out.child(self.node_inside(&key, depth + 1, cx));
        }
        out.into_any_element()
    }

    /// What is INSIDE a table or a collection: its fields and its indexes.
    ///
    /// Both come straight from the engine — `schema.columns` and
    /// `schema.indexes` — and neither has ever been shown. The type beside each
    /// field is the same word the grid's headers and the Tree's badges use,
    /// because one dictionary serving three places is the whole point of having
    /// written it.
    fn node_inside(&self, key: &str, depth: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (t, ms) = (self.t(), self.scale);
        let pad = 16.0 + depth as f32 * 12.0;
        let w = (ms.line_w() - pad - ms.base() * 3.0).max(ms.base() * 4.0);
        let mut out = div().flex().flex_col();

        let fields = self.node_fields.get(key);
        let indexes = self.node_indexes.get(key);
        if fields.is_none() && indexes.is_none() {
            return out
                .child(
                    div()
                        .pl(px(pad))
                        .py(px(1.0))
                        .child(self.small("reading…", t.muted())),
                )
                .into_any_element();
        }

        if let Some(cols) = fields {
            // WHERE the types came from, said on the heading. A type the server
            // declared and a type worked out from twenty documents are
            // different claims and must not look alike — the same rule as the
            // column headers' "67% of 3".
            let sample = self.node_sample.get(key);
            let basis = match sample {
                Some(rows) if !rows.is_empty() => {
                    format!("Fields ({}) \u{b7} types from a sample of {}", cols.len(), rows.len())
                }
                _ => format!("Fields ({})", cols.len()),
            };
            out = out.child(
                div().pl(px(pad)).py(px(1.0)).child(self.small(basis, t.title())),
            );
            for c in cols {
                // The declared type, when the driver declares one. A document
                // store does NOT — `schema.columns` comes back with an empty
                // type for every field of a collection — and there are no
                // values here to derive one from, so the honest answer is to
                // say nothing. Running it through the dictionary anyway would
                // print `null` beside all fourteen fields, which is not a gap
                // but a wrong answer.
                let declared = c.type_name.trim();
                let shown: Option<&'static str> = if !declared.is_empty() {
                    // The driver says. Relational drivers do.
                    Some(crate::workspace::type_badge(&Value::Null, Some(declared)))
                } else {
                    // It does not, so the documents are asked. `column_summary`
                    // is the same function the grid's headers use, so `mixed`
                    // means the same thing in both places and a field that is
                    // sometimes a string and sometimes a number says so.
                    self.node_sample
                        .get(key)
                        .zip(self.node_sample_cols.get(key))
                        .and_then(|(rows, names)| {
                            let i = names.iter().position(|n| n == &c.name)?;
                            let sum = crate::workspace::column_summary(rows, i, None);
                            (sum.present > 0).then_some(sum.kind)
                        })
                };
                let name = c.name.clone();
                out = out.child(
                    div()
                        .id(SharedString::from(format!("fld-{key}-{}", c.name)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl(px(pad + 12.0))
                        .py(px(1.0))
                        .cursor_pointer()
                        .hover(|d| d.bg(t.hover_mark()))
                        // A click puts the NAME where text is being written.
                        // Details belong on the row, not behind a click, and
                        // the name is the thing somebody is about to type.
                        .tooltip(crate::tip::Tip::build(
                            match shown {
                                Some(ty) => format!(
                                    "{} · {ty}{}",
                                    c.name,
                                    if c.primary { " · key" } else { "" }
                                ),
                                None => format!(
                                    "{}{}",
                                    c.name,
                                    if c.primary { " · key" } else { "" }
                                ),
                            },
                            None,
                            self.theme.clone(),
                            self.scale,
                        ))
                        .child(self.line_in(c.name.clone(), ms.small(), t.fg(), w))
                        .when_some(shown, |d, ty| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_size(ms.tiny())
                                    .text_color(t.badge(ty))
                                    .child(ty),
                            )
                        })
                        .when(c.primary, |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_size(ms.tiny())
                                    .text_color(t.accent())
                                    .child("key"),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.insert_into_editor(&name, cx);
                        })),
                );
            }
        }

        if let Some(idx) = indexes {
            out = out.child(
                div()
                    .pl(px(pad))
                    .py(px(1.0))
                    .child(self.small(format!("Indexes ({})", idx.len()), t.title())),
            );
            for i in idx {
                out = out.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl(px(pad + 12.0))
                        .py(px(1.0))
                        .child(self.line_tip(
                            format!("{} ({})", i.name, i.columns.join(", ")),
                            ms.small(),
                            t.muted(),
                            w,
                            format!("idx-{key}-{}", i.name),
                        ))
                        .when(i.unique, |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_size(ms.tiny())
                                    .text_color(t.accent())
                                    .child("unique"),
                            )
                        }),
                );
            }
        }
        out.into_any_element()
    }

    fn tabs_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.t();
        let ms = self.scale;
        // It SCROLLS. Twelve tabs pushed the twelfth off the right edge with no
        // sign that it was there, and nothing could reach it but the palette.
        let mut strip = div()
            .id("tab-strip")
            .flex()
            .items_center()
            .w_full()
            .h(px(ms.tab_h()))
            .flex_none()
            .overflow_x_scroll()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border());

        for (i, tab) in self.tabs.iter().enumerate() {
            let on = i == self.active;
            let id = tab.id;
            // What this tab's statement is doing, in one glyph. A tab said
            // nothing about its own state, so a query running in the third tab
            // looked exactly like the four that were idle.
            let mark = crate::workspace::tab_mark(tab.state);
            let mark_colour = match tab.state {
                RunState::Failed => t.error(),
                RunState::Cancelled => t.warning(),
                _ => t.accent(),
            };
            strip = strip.child(
                div()
                    .id(SharedString::from(format!("tab-{}", tab.id)))
                    .group(SharedString::from(format!("tab-g-{}", tab.id)))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h_full()
                    .flex_none()
                    .cursor_pointer()
                    .border_r_1()
                    .border_color(t.border())
                    // The chosen tab is marked by an EDGE as well as a fill: on
                    // several palettes `base` and `panel` are a hair apart, and
                    // the fill alone left the strip looking like one block.
                    .when(on, |d| d.bg(t.base()).border_b_2().border_color(t.accent()))
                    .when(!on, |d| d.border_b_2().border_color(t.panel()))
                    .hover(|d| d.bg(t.hover_mark()))
                    .child(
                        // The tab carries its connection's driver, because tabs
                        // from different connections sit side by side — and in
                        // the same square as every other inline icon, so a row
                        // of tabs does not look like a row of two sizes.
                        self.icon_inline(DB_GLYPH, t.accent()),
                    )
                    .children(mark.map(|g| self.icon_inline(g, mark_colour)))
                    .child(
                        // CUT, with the whole statement on hover. A tab titled
                        // by the first forty characters of a query made the
                        // strip as wide as the query.
                        self.line_tip(
                            tab.title.clone(),
                            ms.body(),
                            if on { t.fg() } else { t.muted() },
                            ms.base() * 14.0,
                            format!("tabt-{}", tab.id),
                        ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("tabx-{}", tab.id)))
                            // Shown on the chosen tab and under the pointer. On
                            // every tab at once it is a row of crosses, and the
                            // one people press by accident is a neighbour's.
                            .when(!on, |d| {
                                d.invisible().group_hover(
                                    SharedString::from(format!("tab-g-{}", tab.id)),
                                    |d| d.visible(),
                                )
                            })
                            .tooltip(self.tip("tip.close_tab", None))
                            .hover(|d| d.bg(t.hover_mark()))
                            .child(self.action_el("\u{f00d}", t.muted()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_tab(id);
                                cx.notify();
                            })),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if let Some(p) = this.tabs.iter().position(|t| t.id == id) {
                                this.active = p;
                                crate::workspace::mru_touch(&mut this.mru, id);
                            }
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener({
                            let others = self.tabs.len() > 1;
                            move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                this.open_menu(
                                    crate::menu::Target::Tab { tab: id, others },
                                    ev.position,
                                    cx,
                                );
                            }
                        }),
                    ),
            );
        }
        strip
    }

    fn result_el(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let Some(tab) = self.tabs.get(self.active) else {
            return div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(self.empty_state("result.empty_title", "result.empty_hint"))
                .into_any_element();
        };

        // A dashboard tab is not a result tab: no statement box, no grid, no
        // row. It draws saved charts on a grid of twelve columns.
        if tab.board.is_some() {
            return self.board_el(tab, cx);
        }

        let mut body = div().flex().flex_col().size_full();

        // ── breadcrumbs and numbers ─────────────────────────────────────────
        let rows_word = self.lang.tr("result.rows");
        let count = match (tab.total, tab.rows.len()) {
            (Some(total), _) => format!("{total} {rows_word}"),
            (None, n) => format!("{n} {rows_word} loaded"),
        };
        let mut sub_row = div().flex().items_center().gap_3();
        let tab_id = tab.id;
        for s in SubTab::all() {
            let on = tab.sub == s;
            let usable = s.implemented();
            // **They take a click now.** These were bare `div`s with no id and
            // no handler, so `tab.sub` could only be set by restoring a session
            // — four words that look like a row of tabs and did nothing at all.
            // The ones that are not built stay unclickable rather than
            // switching to an empty view.
            sub_row = sub_row.child(
                div()
                    .id(SharedString::from(format!("sub-{}-{}", tab_id, s.label())))
                    .when(usable && !on, |d| d.cursor_pointer().hover(|d| d.text_color(t.fg())))
                    .text_size(ms.small())
                    .text_color(if on {
                        t.fg()
                    } else if usable {
                        t.muted()
                    } else {
                        // Laid out but not built; shown so the row does not
                        // rearrange when they arrive.
                        t.disabled()
                    })
                    .child(SharedString::from(s.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !usable {
                            return;
                        }
                        if let Some(t) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                            t.sub = s;
                        }
                        this.remember();
                        cx.notify();
                    })),
            );
        }

        body = body.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .w_full()
                .px_3()
                .py_1()
                .bg(t.panel())
                .border_b_1()
                .border_color(t.border())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .children(self.live(&tab.conn_name).map(|l| {
                            let link = crate::workspace::link_label(l.encrypted, l.tunneled);
                            div()
                                .id("crumb-lock")
                                .text_size(ms.tiny())
                                .text_color(if link.safe { t.muted() } else { t.error() })
                                .tooltip(self.tip(link.tip, None))
                                .child(SharedString::from(link.glyph))
                        }))
                        .child(self.small(tab.conn_name.clone(), t.muted()))
                        .child(self.small("›", t.divider()))
                        .child(self.small(tab.title.clone(), t.fg()))
                        .child(self.small(count, t.muted()))
                        .child(self.small(format!("{} ms", tab.ms), t.muted())),
                ),
        );

        // ── the naming row ──────────────────────────────────────────────────
        // Above the views, only while a save is waiting for a name. Not a modal
        // over the result: the thing being named is the result, and covering it
        // up to ask what to call it is exactly backwards.
        if let Some(what) = self.naming.clone() {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .px_3()
                    .py_1()
                    .flex_none()
                    .bg(t.raised())
                    .border_b_1()
                    .border_color(t.accent())
                    .child(self.small(
                        match &what {
                            NameFor::Folder(conn) => {
                                format!("{} {conn}", self.lang.tr("name.folder"))
                            }
                            other => {
                                format!("{} {}", self.lang.tr("result.name_it"), other.what())
                            }
                        },
                        t.muted(),
                    ))
                    .child(div().flex_1().text_size(ms.body()).child(self.save_name.clone()))
                    .child(
                        btn(t, ms, "name-ok", self.tr("result.save"), Btn::Link)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let name = this.save_name.read(cx).text().trim().to_string();
                                this.named(name, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        btn(t, ms, "name-no", self.tr("tip.cancel_edit"), Btn::Quiet)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.naming = None;
                                cx.notify();
                            })),
                    ),
            );
        }

        // ── view switch ─────────────────────────────────────────────────────
        // The sub-tabs belong HERE, next to the views, not pinned to the far
        // corner of the breadcrumbs: they are about the result below them.
        let mut views = div().flex().items_center().gap_2().px_3().py_1();
        views = views.child(sub_row).child(self.small("│", t.divider()));
        for v in [ResultView::Tree, ResultView::Table, ResultView::Json, ResultView::Chart] {
            let on = tab.view == v;
            let tab_id = tab.id;
            views = views.child(
                chip(t, ms, format!("view-{}-{}", tab_id, v.label()), v.label(), on)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                            tb.view = v;
                        }
                        cx.notify();
                    })),
            );
        }
        let running = tab.state == RunState::Running;
        // Off, not merely grey: a disabled control that still takes the click
        // looks like one that worked. See `widgets::btn_off`.
        if !running {
            views = views.child(crate::widgets::btn_off(t, ms, self.tr("result.cancel"), Btn::Quiet));
        }
        if running {
            views = views.child(
                btn(t, ms, "cancel", self.tr("result.cancel"), Btn::Danger)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.cancel_active();
                        cx.notify();
                    })),
            );
        }
        if tab.view == ResultView::Chart {
            views = views.child(
                btn(t, ms, "save-chart", self.tr("result.save_chart"), Btn::Link)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_chart(cx);
                        cx.notify();
                    })),
            );
        }
        views = views.child(
            btn(t, ms, "save-query", self.tr("result.save_query"), Btn::Link)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.save_query(cx);
                    cx.notify();
                })),
        );
        let has_rows = !tab.rows.is_empty();
        // A control nobody can use still has to be READABLE — greyed out to the
        // border colour it simply vanished, and a button that is invisible is
        // not disabled, it is missing.
        if !has_rows {
            views = views.child(crate::widgets::btn_off(t, ms, self.tr("result.export"), Btn::Quiet));
        }
        if has_rows {
            views = views.child(
                btn(t, ms, "export-toggle", self.tr("result.export"), Btn::Link)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.export_open = !this.export_open;
                        cx.notify();
                    })),
            );
        }
        body = body.child(views);
        body = body.child(self.query_bar(tab, cx));
        if self.export_open {
            body = body.child(self.export_row(tab, cx));
        }

        // A statement the engine called destructive: the reason, the statement,
        // and two buttons. The same card the console shows, in the tab that
        // asked — nothing has been sent to the database at this point.
        if let Some((statement, reason)) = tab.confirm.clone() {
            let id = tab.id;
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .m_3()
                    .p_2()
                    .border_1()
                    .border_color(t.error())
                    .child(self.small(reason, t.error()))
                    .child(self.small(statement, t.fg()))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                btn(t, ms, "tab-confirm-run", self.tr("write.run_it"), Btn::Danger)
                                    .on_click(cx.listener(move |this, _, _, _cx| {
                                        // The statement the card is ABOUT, which
                                        // is the one that was classified — not
                                        // whatever the editor holds now.
                                        let mut asked = None;
                                        if let Some(tab) =
                                            this.tabs.iter_mut().find(|t| t.id == id)
                                        {
                                            asked = tab.confirm.take().map(|(s, _)| s);
                                        }
                                        // Confirmed by a person, and the engine
                                        // is told so — it refuses when it was not.
                                        if let Some(s) = asked {
                                            this.run_tab_confirmed(id, &s, true);
                                        }
                                    })),
                            )
                            .child(
                                btn(t, ms, "tab-confirm-leave", self.tr("write.leave_it"), Btn::Quiet)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(tab) =
                                            this.tabs.iter_mut().find(|t| t.id == id)
                                        {
                                            tab.confirm = None;
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
            return body.into_any_element();
        }

        if let Some(err) = &tab.error {
            body = body.child(
                div()
                    .m_3()
                    .p_2()
                    .border_1()
                    .border_color(t.error())
                    .text_size(ms.body())
                    .text_color(t.error())
                    .child(SharedString::from(err.clone())),
            );
            return body.into_any_element();
        }
        if running {
            // No percentage: no driver reports one, and a bar that moved would
            // be inventing progress.
            body = body.child(div().p_3().child(self.small("running…", t.muted())));
            return body.into_any_element();
        }

        // The Schema sub-tab describes the OBJECT, so the view switch and the
        // paging do not apply to it: it is not a different rendering of the
        // rows, it is a different subject.
        if tab.sub == SubTab::Schema {
            return body.child(self.schema_el(tab)).into_any_element();
        }
        if tab.sub == SubTab::Builder {
            return body.child(self.builder_el(tab, cx)).into_any_element();
        }
        let view = match tab.view {
            ResultView::Table => self.table_el(tab, cx),
            ResultView::Tree => self.tree_el(tab, cx),
            ResultView::Json => self.json_el(tab, cx),
            ResultView::Chart => self.chart_el(tab, cx),
        };
        // The detail panel sits BESIDE the grid, not over it: the row it is
        // showing stays visible in its place among the others, which is half of
        // what you are looking at when you open one.
        body = body.child(match self.detail_el(tab, cx) {
            Some(detail) => div()
                .flex()
                .flex_1()
                .overflow_hidden()
                .child(div().flex_1().overflow_hidden().child(view))
                .child(detail)
                .into_any_element(),
            None => view,
        });
        body.into_any_element()
    }

    /// The result as bars.
    ///
    /// Drawn from the ROWS, not from the declared column types: a driver that
    /// declares nothing — sqlite usually does — would otherwise never be
    /// chartable, and a column declared INTEGER can still arrive holding text.
    // ── the dashboard canvas ─────────────────────────────────────────────────

    /// A board: a toolbar, a time range, and tiles on a grid of twelve columns.
    ///
    /// Tiles are placed ABSOLUTELY, with the left edge and the width as a
    /// fraction of twelve. That is the whole reason the grid is twelve: a
    /// fraction of twelve is exact for halves, thirds, quarters and sixths, so
    /// two tiles side by side meet on a pixel rather than nearly. The gutter is
    /// the tile's own padding, not a gap in the layout, which keeps the
    /// arithmetic to one division.
    fn board_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let Some(b) = tab.board.as_ref() else {
            return div().into_any_element();
        };
        let tab_id = tab.id;
        let cols = b.columns.max(1) as f32;
        let row_h = ms.base() * 4.0;

        let mut top = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border())
            ;
        let editing = b.editing;
        // A board that has never been saved has no name, and the box is the
        // only way to give it one — so it is shown whenever the name is empty,
        // not only while arranging.
        if editing || b.name.is_empty() {
            top = top
                .child(div().w(px(ms.base() * 12.0)).child(self.field(ms.small(), self.board_name.clone())))
                .child(
                    btn(t, ms, "board-rename", self.tr("board.name_it"), Btn::Link)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let n = this.board_name.read(cx).text().trim().to_string();
                            if n.is_empty() {
                                this.toast("a dashboard needs a name", true);
                                return;
                            }
                            if let Some(bd) = this.board_mut(tab_id) {
                                bd.name = n.clone();
                                bd.dirty = true;
                            }
                            if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                                tb.title = n;
                            }
                            cx.notify();
                        })),
                );
        } else {
            top = top.child(
                div()
                    .text_size(ms.heading())
                    .text_color(t.title())
                    .child(SharedString::from(b.name.clone())),
            );
        }
        top = top.child(
            chip(
                t,
                ms,
                "board-edit",
                if editing { self.tr("board.done") } else { self.tr("board.arrange") },
                editing,
            )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(bd) = this.board_mut(tab_id) {
                        bd.editing = !bd.editing;
                        bd.selected = None;
                    }
                    cx.notify();
                })),
        );
        if editing {
            for (label, kind) in [
                ("+ chart", TileKind::Chart),
                ("+ divider", TileKind::Divider),
                ("+ text", TileKind::Text),
            ] {
                top = top.child(
                    btn(t, ms, format!("add-{label}"), label, Btn::Link)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.board_add(tab_id, kind);
                            cx.notify();
                        })),
                );
            }
        }
        top = top
            .child(div().flex_1())
            .child(
                btn(t, ms, "board-refresh", format!("\u{f021}  {}", self.tr("result.refresh")), Btn::Link)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.run_board(tab_id);
                        cx.notify();
                    })),
            )
            .child(
                chip(t, ms, "board-save", if b.dirty { self.tr("board.save") } else { self.tr("board.saved") }, b.dirty)
                    .on_click(cx.listener(move |this, _, _, cx| this.save_board(tab_id, cx))),
            );

        // The shared time range. One range, every chart on the board, and the
        // tiles that could not take it say so on themselves.
        let mut timebar = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .bg(t.status())
            .child(self.small(self.tr("board.time"), t.muted()))
            // Real widths: a text field with none collapses to nothing, and the
            // three of them drew on top of one another.
            .child(div().w(px(ms.base() * 9.0)).flex_none().child(self.field(ms.small(), self.board_time_col.clone())))
            .child(self.small(self.tr("board.from"), t.muted()))
            .child(div().w(px(ms.base() * 9.0)).flex_none().child(self.field(ms.small(), self.board_time_from.clone())))
            .child(self.small(self.tr("board.to"), t.muted()))
            .child(div().w(px(ms.base() * 9.0)).flex_none().child(self.field(ms.small(), self.board_time_to.clone())))
            .child(
                btn(t, ms, "board-time-apply", self.tr("board.apply"), Btn::Link)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.board_apply_time(tab_id, cx);
                    })),
            );
        // The windows anybody actually asks for. Typing two timestamps to see
        // the last hour is how the wrong hour gets asked for.
        for (label, secs) in [
            ("15m", 900u64),
            ("1h", 3_600),
            ("6h", 21_600),
            ("24h", 86_400),
            ("7d", 604_800),
        ] {
            timebar = timebar.child(
                btn(t, ms, format!("board-win-{label}"), label, Btn::Quiet).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.board_preset_time(tab_id, secs, cx);
                    },
                )),
            );
        }
        timebar = timebar.child(self.small(self.tr("board.every"), t.muted()));
        for (label, every) in [
            ("off", None),
            ("10s", Some(Duration::from_secs(10))),
            ("60s", Some(Duration::from_secs(60))),
            ("5m", Some(Duration::from_secs(300))),
        ] {
            let on = b.refresh_every == every;
            timebar = timebar.child(
                chip(t, ms, format!("board-every-{label}"), label, on).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(bd) = this.board_mut(tab_id) {
                            bd.refresh_every = every;
                            // From NOW, so choosing an interval does not fire
                            // one immediately on top of the run that is already
                            // on screen.
                            bd.refreshed_at = Some(Instant::now());
                        }
                        cx.notify();
                    },
                )),
            );
        }
        if b.dash.time.is_some() {
            timebar = timebar.child(
                btn(t, ms, "board-time-clear", self.tr("board.clear"), Btn::Quiet)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(bd) = this.board_mut(tab_id) {
                            bd.dash.time = None;
                            bd.dirty = true;
                        }
                        this.run_board(tab_id);
                        cx.notify();
                    })),
            );
        }
        // How many charts the range did NOT reach. Counted and SAID, because a
        // range that silently applies to four charts out of six is worse than
        // one that says which two it missed.
        if b.dash.time.is_some() {
            let missed = b
                .dash
                .tiles
                .iter()
                .enumerate()
                .filter(|(i, tl)| {
                    tl.kind == TileKind::Chart
                        && b.panes.get(i).map(|p| p.fit) != Some(TimeFit::Applied)
                })
                .count();
            if missed > 0 {
                timebar = timebar.child(self.small(
                    format!("{missed} chart(s) are NOT time-filtered — their statement has no {{{{time}}}} in it"),
                    t.warning(),
                ));
            }
        }

        // What the selected tile is, and the two things that can be done to it:
        // say what it shows, and take it off the board.
        let mut selected_row = None;
        if editing {
            if let Some(i) = b.selected {
                if let Some(tile) = b.dash.tiles.get(i) {
                    let mut row = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .bg(t.raised())
                        .child(self.small(
                            match tile.kind {
                                TileKind::Chart => format!("chart tile \u{2014} {}", tile.chart),
                                TileKind::Divider => "divider".to_string(),
                                TileKind::Text => "text tile".to_string(),
                            },
                            t.muted(),
                        ));
                    if tile.kind != TileKind::Divider {
                        row = row
                            .child(div().w(px(ms.base() * 16.0)).child(self.field(ms.small(), self.tile_text.clone())))
                            .child(
                                btn(t, ms, "tile-set-text", "Set", Btn::Link)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let txt = this.tile_text.read(cx).text().to_string();
                                        if let Some(bd) = this.board_mut(tab_id) {
                                            if let Some(tl) = bd.dash.tiles.get_mut(i) {
                                                tl.text = txt;
                                                bd.dirty = true;
                                            }
                                        }
                                        cx.notify();
                                    })),
                            );
                    }
                    // Which saved chart this tile shows, chosen by NAME from the
                    // charts that exist — so a tile cannot come to name one that
                    // does not.
                    if tile.kind == TileKind::Chart {
                        for c in &b.charts {
                            let on = c.name == tile.chart;
                            let name = c.name.clone();
                            row = row.child(
                                chip(t, ms, format!("pick-{}", c.name), c.name.clone(), on)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(bd) = this.board_mut(tab_id) {
                                            if let Some(tl) = bd.dash.tiles.get_mut(i) {
                                                tl.chart = name.clone();
                                                bd.dirty = true;
                                            }
                                        }
                                        this.run_board(tab_id);
                                        cx.notify();
                                    })),
                            );
                        }
                    }
                    row = row.child(div().flex_1()).child(
                        btn(t, ms, "tile-remove", self.tr("board.remove"), Btn::Danger)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(bd) = this.board_mut(tab_id) {
                                    bd.remove(i);
                                }
                                this.run_board(tab_id);
                                cx.notify();
                            })),
                    );
                    selected_row = Some(row);
                }
            }
        }

        let mut canvas = div().relative().w_full().p_2();
        if b.dash.tiles.is_empty() {
            canvas = canvas.child(
                div().p_3().child(self.small(
                    self.tr("board.empty"),
                    t.muted(),
                )),
            );
        }
        for (i, tile) in b.dash.tiles.iter().enumerate() {
            canvas = canvas.child(self.tile_el(tab_id, b, i, tile, cols, row_h, &tab.driver, cx));
        }

        let mut out = div().flex().flex_col().size_full().child(top).child(timebar);
        if let Some(row) = selected_row {
            out = out.child(row);
        }
        if let Some(stale) = &b.stale {
            out = out.child(
                div()
                    .px_3()
                    .py_1()
                    .bg(t.status())
                    .child(self.small(stale.clone(), t.warning())),
            );
        }
        out.child(div().id("board-canvas").flex_1().overflow_y_scroll().child(canvas))
            .into_any_element()
    }

    /// One tile.
    #[allow(clippy::too_many_arguments)]
    fn tile_el(
        &self,
        tab_id: u64,
        b: &Board,
        i: usize,
        tile: &Tile,
        cols: f32,
        row_h: f32,
        driver: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let selected = b.selected == Some(i);
        let editing = b.editing;
        // Stateful from the start: `.id()` changes a `Div` into a
        // `Stateful<Div>`, so adding it later in an `if` cannot be an
        // assignment back to the same binding.
        let mut el = div()
            .id(SharedString::from(format!("tile-{tab_id}-{i}")))
            .absolute()
            .left(gpui::relative(tile.x as f32 / cols))
            .top(px(tile.y as f32 * row_h))
            .w(gpui::relative(tile.w as f32 / cols))
            .h(px(tile.h as f32 * row_h))
            // The gutter is padding, not a gap in the grid: one division to get
            // a tile's place, and two tiles side by side still meet exactly.
            .p(px(ms.gutter()));

        let mut inner = div()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(t.panel())
            .border_1()
            .border_color(if selected { t.accent() } else { t.border() });

        match tile.kind {
            TileKind::Divider => {
                inner = div()
                    .flex()
                    .items_center()
                    .size_full()
                    .bg(t.base())
                    // `divider`, not `border`: a rule drawn in the box-edge
                    // colour vanishes into the base on half the palettes, and a
                    // divider nobody can see is a tile that looks empty.
                    .child(div().w_full().h(px(1.0)).bg(if selected {
                        t.accent()
                    } else {
                        t.divider()
                    }));
            }
            TileKind::Text => {
                let empty = tile.text.is_empty();
                inner = inner.bg(t.base()).child(
                    div()
                        .p_2()
                        .text_size(ms.body())
                        // A tile with nothing in it says what to do, in the
                        // quiet ink — the hint read as the heading itself when
                        // it was drawn in the ordinary one.
                        .text_color(if empty { t.disabled() } else { t.fg() })
                        .when(empty, |d| d.italic())
                        .child(SharedString::from(if empty {
                            self.lang.tr("board.text_hint").to_string()
                        } else {
                            tile.text.clone()
                        })),
                );
            }
            TileKind::Chart => {
                let heading = if tile.text.is_empty() { tile.chart.clone() } else { tile.text.clone() };
                let pane = b.panes.get(&i);
                let mut head = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .bg(t.header_bg())
                    .child(
                        div()
                            .flex_1()
                            .text_size(ms.small())
                            .text_color(t.header_fg())
                            .child(SharedString::from(heading)),
                    );
                // The honest label on a tile the range did not reach. Shown
                // only when there IS a range: "not time-filtered" on a board
                // with no range is noise about a thing nobody asked for.
                if b.dash.time.is_some() {
                    if let Some(note) = pane.and_then(|p| p.fit.note(driver)) {
                        head = head.child(
                            div()
                                .text_size(ms.tiny())
                                .text_color(t.warning())
                                .child(SharedString::from(note)),
                        );
                    }
                }
                inner = inner.child(head).child(self.tile_chart_el(pane, ms, &t));
            }
        }

        el = el.child(inner);

        if editing {
            let mode_move = DragMode::Move;
            el = el
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
                        this.board_drag_start(tab_id, i, mode_move, pos);
                        cx.notify();
                    }),
                )
                // The resize grip, in the corner, and OUTSIDE the move handler
                // so a drag that starts on it resizes instead of moving.
                .child(
                    div()
                        .id(SharedString::from(format!("grip-{tab_id}-{i}")))
                        .absolute()
                        .right(px(ms.gutter()))
                        .bottom(px(ms.gutter()))
                        .w(px(ms.base()))
                        .h(px(ms.base()))
                        .bg(t.accent())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                                let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
                                this.board_drag_start(tab_id, i, DragMode::Resize, pos);
                                cx.notify();
                            }),
                        ),
                );
        }
        el.into_any_element()
    }

    /// The bars inside a chart tile. The same rules as the chart view: a
    /// measurement is a column whose every non-null value is a number, a missing
    /// value is drawn as NOTHING rather than as a bar of zero, and the axis
    /// includes zero.
    fn tile_chart_el(&self, pane: Option<&Pane>, ms: Metrics, t: &Theme) -> gpui::AnyElement {
        let Some(pane) = pane else {
            return div()
                .flex_1()
                .p_2()
                .child(self.small("not loaded", t.muted()))
                .into_any_element();
        };
        if let Some(e) = &pane.error {
            return div()
                .flex_1()
                .p_2()
                .text_size(ms.small())
                .text_color(t.error())
                .child(SharedString::from(e.clone()))
                .into_any_element();
        }
        if pane.state == RunState::Running {
            return div()
                .flex_1()
                .p_2()
                .child(self.small("running\u{2026}", t.muted()))
                .into_any_element();
        }
        let shape = match ChartShape::of(&pane.columns, &pane.rows) {
            Ok(s) => s,
            Err(why) => {
                return div()
                    .flex_1()
                    .p_2()
                    .child(self.small(why, t.muted()))
                    .into_any_element()
            }
        };
        let col = shape.values[0];
        let (lo, hi) = value_range(&pane.rows, col);
        let span = hi - lo;
        let mut bars = div().flex().flex_col().flex_1().overflow_hidden().px_2().py_1().gap(px(1.0));
        for r in 0..pane.rows.len().min(24) {
            let v = pane.rows.get(r).and_then(|row| row.get(col)).and_then(|x| x.as_f64());
            let frac = v.map(|n| ((n - lo) / span).clamp(0.0, 1.0)).unwrap_or(0.0);
            bars = bars.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .h(px(ms.base()))
                    .child(self.line(
                        bar_label_of(&pane.rows, 0, &shape, r),
                        ms.tiny(),
                        t.muted(),
                        ms.base() * 6.0,
                    ))
                    .child(
                        div().flex_1().h_full().child(
                            div()
                                .h_full()
                                .w(gpui::relative(frac as f32))
                                .bg(if v.is_some() { t.accent() } else { t.panel() }),
                        ),
                    ),
            );
        }
        bars.into_any_element()
    }

    fn chart_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let shape = match ChartShape::of(&tab.columns, &tab.rows) {
            Ok(s) => s,
            // Say WHY there is no chart. "No chart" on its own reads as a bug.
            Err(why) => {
                return div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(div().text_color(t.muted()).child(SharedString::from(why)))
                    .child(self.small(
                        "a chart needs one column of numbers and one to name the bars",
                        t.muted(),
                    ))
                    .into_any_element();
            }
        };

        let tab_id = tab.id;
        // A saved chart names its measurement. Match it by NAME here, so a
        // statement that gained or reordered a column still measures what the
        // chart was saved measuring — and falls back visibly rather than
        // drawing a different column under the old name.
        let by_name = tab.chart_measure.as_ref().and_then(|m| {
            shape
                .values
                .iter()
                .copied()
                .find(|&i| tab.columns.get(i).map(|c| &c.name) == Some(m))
        });
        let chosen = tab
            .chart_col
            .filter(|c| shape.values.contains(c))
            .or(by_name)
            .unwrap_or(shape.values[0]);

        // Which measurement is being drawn, when there is more than one.
        let mut picker = div().flex().items_center().gap_2().px_3().py_1();
        if shape.values.len() > 1 {
            picker = picker.child(self.small("measure", t.muted()));
            for &i in &shape.values {
                let on = i == chosen;
                let name = tab.columns.get(i).map(|c| c.name.clone()).unwrap_or_default();
                picker = picker.child(
                    chip(t, ms, format!("ch-{tab_id}-{i}"), name, on)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                                tb.chart_col = Some(i);
                            }
                            cx.notify();
                        })),
                );
            }
        }

        let (lo, hi) = value_range(&tab.rows, chosen);
        let span = hi - lo;
        let mut bars = div().flex().flex_col().flex_1().overflow_hidden().px_3().py_1().gap(px(2.0));
        // Only what fits is drawn, and the count below says how many there are:
        // a chart of two hundred bars three pixels wide is not a chart.
        let shown = tab.rows.len().min(40);
        for r in 0..shown {
            let v = tab.rows.get(r).and_then(|row| row.get(chosen)).and_then(|x| x.as_f64());
            let frac = v.map(|n| ((n - lo) / span).clamp(0.0, 1.0)).unwrap_or(0.0);
            let label = bar_label(tab, &shape, r);
            bars = bars.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(ms.chart_row_h()))
                    .child(
                        // CUT, with the whole label on hover. A category whose
                        // name is longer than the column ran over the bar it
                        // belongs to, so the chart read as one block of text.
                        self.line_tip(
                            label,
                            ms.small(),
                            t.muted(),
                            ms.base() * 11.5,
                            format!("barl-{}-{r}", tab.id),
                        ),
                    )
                    .child(
                        div().flex_1().h_full().child(
                            div()
                                .h_full()
                                .w(gpui::relative(frac as f32))
                                // A missing measurement is drawn as NOTHING, not
                                // as a bar of zero: "no value" and "zero" are
                                // different answers.
                                .bg(if v.is_some() { t.accent() } else { t.panel() }),
                        ),
                    )
                    .child(
                        div()
                            .w(px(ms.base() * 8.0))
                            .flex_none()
                            .truncate()
                            // To the RIGHT, like every other number in this
                            // client: a column of measurements lines up by
                            // place value or it is read one at a time.
                            .text_right()
                            .text_size(ms.small())
                            .text_color(if v.is_some() { t.fg() } else { t.null_fg() })
                            .when(v.is_none(), |d| d.italic())
                            .child(SharedString::from(match v {
                                Some(n) => format!("{n}"),
                                None => "NULL".into(),
                            })),
                    ),
            );
        }

        let footer = self.small(
            format!(
                "{}\u{2013}{} of {} loaded row(s) \u{2014} axis from {lo} to {hi}",
                if shown == 0 { 0 } else { 1 },
                shown,
                tab.rows.len()
            ),
            t.muted(),
        );

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(picker)
            .child(bars)
            .child(div().px_3().py_1().child(footer))
            .into_any_element()
    }

    /// The visual builder: choices in, a statement out.
    ///
    /// What comes out goes into the statement box above, where it can be read
    /// and edited before it runs — and when it runs it goes through everything
    /// else does, including the connection's policy. The builder is a way of
    /// WRITING a statement, not a way around the rules about running one.
    fn builder_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let tab_id = tab.id;
        let ops = self.meta(&tab.driver).map(|m| m.builder_ops.clone()).unwrap_or_default();
        let mut body = div().flex().flex_col().flex_1().overflow_hidden().p_3().gap_1();

        if ops.is_empty() {
            return body
                .child(self.small(
                    format!("the {} driver has no builder — its statements are not SQL", tab.driver),
                    t.muted(),
                ))
                .into_any_element();
        }
        if tab.object.is_none() {
            return body
                .child(self.small(
                    "the builder works on an object — open a table from the tree",
                    t.muted(),
                ))
                .into_any_element();
        }

        // ── columns ─────────────────────────────────────────────────────────
        body = body.child(self.section("Show"));
        let all = tab.builder.columns.is_empty();
        let mut cols_row = div().flex().flex_wrap().gap_2().child(
            chip(t, ms, "b-all", "every column", all)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                        tb.builder.columns.clear();
                    }
                    this.sync_builder(tab_id, cx);
                    cx.notify();
                })),
        );
        for c in &tab.meta_columns {
            let on = tab.builder.columns.contains(&c.name);
            let name = c.name.clone();
            cols_row = cols_row.child(
                chip(t, ms, format!("b-col-{}", c.name), c.name.clone(), on)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                            match tb.builder.columns.iter().position(|x| *x == name) {
                                Some(i) => {
                                    tb.builder.columns.remove(i);
                                }
                                None => tb.builder.columns.push(name.clone()),
                            }
                        }
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
        }
        body = body.child(cols_row);

        // ── conditions ──────────────────────────────────────────────────────
        body = body.child(self.section("Where"));
        for (i, cond) in tab.builder.conds.iter().enumerate() {
            let mut row = div().flex().items_center().gap_2();
            // column
            let cur = cond.col.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("b-cc-{i}")))
                    .w(px(ms.base() * 11.5))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.fg())
                    .border_1()
                    .border_color(t.border())
                    .child(SharedString::from(if cur.is_empty() {
                        "pick a column".to_string()
                    } else {
                        cur
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cycle_cond_column(tab_id, i);
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
            // operator
            let label = ops.get(cond.op).map(|o| o.label.clone()).unwrap_or_default();
            row = row.child(
                div()
                    .id(SharedString::from(format!("b-op-{i}")))
                    .w(px(ms.base() * 8.0))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.accent())
                    .border_1()
                    .border_color(t.border())
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let n = this
                            .meta(&this.tabs.iter().find(|x| x.id == tab_id).map(|x| x.driver.clone()).unwrap_or_default())
                            .map(|m| m.builder_ops.len())
                            .unwrap_or(1)
                            .max(1);
                        if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                            if let Some(c) = tb.builder.conds.get_mut(i) {
                                c.op = (c.op + 1) % n;
                            }
                        }
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
            // value, only where the operator takes one
            if ops.get(cond.op).map_or(true, |o| o.takes_value()) {
                if let Some(input) = self.inputs.get(&(tab_id, format!("b:{i}"))) {
                    row = row.child(div().w(px(ms.base() * 17.0)).child(self.field(ms.small(), input.clone())));
                }
            } else {
                row = row.child(div().w(px(ms.base() * 17.0)).child(self.small("takes no value", t.muted())));
            }
            row = row.child(
                div()
                    .id(SharedString::from(format!("b-rm-{i}")))
                    .tooltip(self.tip("tip.remove_tile", None))
                    .hover(|d| d.bg(t.hover_mark()))
                    .px_1()
                    .cursor_pointer()
                                                            .child(self.action_el("\u{f00d}", t.muted()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                            if i < tb.builder.conds.len() {
                                tb.builder.conds.remove(i);
                            }
                        }
                        this.sync_builder(tab_id, cx);
                        cx.notify();
                    })),
            );
            body = body.child(row);
        }
        body = body.child(
            btn(t, ms, "b-add", "+ condition", Btn::Link)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.add_condition(tab_id, cx);
                    cx.notify();
                })),
        );

        // ── order ───────────────────────────────────────────────────────────
        body = body.child(self.section("Order"));
        let order_label = match &tab.builder.order {
            Some((c, asc)) => format!("{c} {}", if *asc { "ascending" } else { "descending" }),
            None => "none".to_string(),
        };
        body = body.child(
            div()
                .id("b-order")
                .w(px(ms.base() * 21.5))
                .px_1()
                .cursor_pointer()
                .text_size(ms.small())
                .text_color(t.fg())
                .border_1()
                .border_color(t.border())
                .child(SharedString::from(order_label))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.cycle_order(tab_id);
                    this.sync_builder(tab_id, cx);
                    cx.notify();
                })),
        );

        body.child(self.small(
            "the statement above is what runs \u{2014} it goes through the connection\u{2019}s policy \
             like anything else",
            t.muted(),
        ))
        .into_any_element()
    }

    /// What the object IS: its columns, which of them identify a row, its
    /// indexes, and the engine's own CREATE statement where it has one.
    ///
    /// Every part of this comes from the engine; none of it is reconstructed
    /// here. A CREATE assembled from column metadata reads as authoritative and
    /// quietly drops constraints, defaults and storage clauses — so a driver
    /// that cannot ask its server for one says so instead.
    fn schema_el(&self, tab: &Tab) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let mut body = div().flex().flex_col().flex_1().overflow_hidden().p_3().gap_2();

        if tab.object.is_none() {
            return body
                .child(self.small(
                    "this tab is a statement, not an object — there is nothing to describe",
                    t.muted(),
                ))
                .into_any_element();
        }

        body = body.child(self.section(&format!("Columns ({})", tab.meta_columns.len())));
        if tab.meta_columns.is_empty() {
            body = body.child(self.small("the engine has not answered yet", t.muted()));
        }
        for c in &tab.meta_columns {
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(ms.base() * 15.5)).text_size(ms.body()).text_color(t.fg()).child(
                        SharedString::from(c.name.clone()),
                    ))
                    .child(div().w(px(ms.base() * 11.5)).child(self.small(c.type_name.clone(), t.muted())))
                    .when(c.primary, |d| d.child(self.small("key", t.accent()))),
            );
        }

        let keyed = tab.meta_columns.iter().any(|c| c.primary);
        body = body.child(self.small(
            if keyed {
                "a row here can be addressed exactly, so it can be edited".to_string()
            } else {
                "no key: a row here cannot be addressed exactly, so it is not editable".to_string()
            },
            if keyed { t.muted() } else { t.error() },
        ));

        if !tab.indexes.is_empty() {
            body = body.child(self.section(&format!("Indexes ({})", tab.indexes.len())));
            for i in &tab.indexes {
                let mut tags = Vec::new();
                if i.primary {
                    tags.push("primary");
                }
                if i.unique {
                    tags.push("unique");
                }
                body = body.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(ms.base() * 15.5)).text_size(ms.body()).text_color(t.fg()).child(
                            SharedString::from(i.name.clone()),
                        ))
                        .child(div().w(px(ms.base() * 18.5)).child(self.small(i.columns.join(", "), t.muted())))
                        .child(self.small(tags.join(" · "), t.accent())),
                );
            }
        }

        if let Some(ddl) = &tab.ddl {
            body = body.child(self.section("CREATE"));
            // Scrolled to, not clipped. A CREATE statement with a wide column
            // list ran off the edge and the rest of it was simply unreachable —
            // the same loss as a grid column you cannot scroll to.
            body = body.child(
                div()
                    .id("schema-ddl")
                    .flex_1()
                    .overflow_scroll()
                    .child(
                        div()
                            .whitespace_nowrap()
                            .text_size(ms.small())
                            .text_color(t.fg())
                            .child(SharedString::from(ddl.clone())),
                    ),
            );
        }
        body.into_any_element()
    }

    /// One row, read downward — the only way to read a row with forty columns
    /// in it, and the place the clipboard buttons belong.
    fn detail_el(&self, tab: &Tab, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let i = tab.detail?;
        let row = tab.rows.get(i)?;
        let t = self.t();
        let ms = self.scale;

        let mut panel = div()
            .flex()
            .flex_col()
            .w(px(ms.base() * 27.0))
            .h_full()
            .bg(t.panel())
            .border_l_1()
            .border_color(t.border())
            .overflow_hidden();

        panel = panel.child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_1()
                .bg(t.header_bg())
                .child(
                    div()
                        .text_size(ms.small())
                        .text_color(t.header_fg())
                        .child(SharedString::from(format!("Row {}", tab.from + i + 1))),
                )
                .child(
                    div()
                        .id("detail-close")
                        .tooltip(self.tip("tip.close_detail", Some("esc")))
                        .hover(|d| d.bg(t.hover_mark()))
                        .px_1()
                        .cursor_pointer()
                                                                        .child(self.action_el("\u{f00d}", t.muted()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(tb) = this.tabs.get_mut(this.active) {
                                tb.detail = None;
                            }
                            cx.notify();
                        })),
                ),
        );

        let tab_id = tab.id;
        let editable = self.meta(&tab.driver).map_or(false, |m| m.caps.edit);
        let mut fields = div().flex().flex_col().flex_1().overflow_hidden().py_1();
        for (ci, c) in tab.columns.iter().enumerate() {
            let v = row.get(ci).unwrap_or(&serde_json::Value::Null);
            let null = v.is_null();
            let key_col = tab.meta_columns.iter().any(|m| m.name == c.name && m.primary);
            let editing_this = tab.editing == Some((i, ci));
            let can = editable && column_editable(tab, ci) && tab.pending_write.is_none();

            let label = div()
                .flex()
                .items_center()
                .gap_2()
                .child(self.small(c.name.clone(), t.header_fg()))
                .child(self.small(
                    crate::workspace::type_badge(v, Some(&c.type_name)),
                    t.muted(),
                ))
                // A key column says so. It is why it cannot be edited, and
                // saying it here is cheaper than a refusal later.
                .when(key_col, |d| d.child(self.small("key", t.accent())));

            let value: gpui::AnyElement = if editing_this {
                // The same editor the tree uses — one definition, so the two
                // cannot come to disagree about what committing means.
                self.editor_el(tab_id, cx)
            } else {
                div()
                    .id(SharedString::from(format!("val-{tab_id}-{ci}")))
                    .text_size(ms.body())
                    // The same two marks the grid uses for an absent value:
                    // the null ink and italic. It was the ordinary muted ink
                    // here, which is also what a present-but-dim value gets.
                    .text_color(if null { t.null_fg() } else { t.fg() })
                    .when(null, |d| d.italic())
                    .when(can, |d| d.cursor_pointer().hover(|h| h.bg(t.hover_mark())))
                    .child(SharedString::from(
                        cell_text(v).unwrap_or_else(|| "NULL".to_string()),
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_edit(tab_id, i, ci, cx);
                        cx.notify();
                    }))
                    .into_any_element()
            };

            fields = fields.child(
                div().flex().flex_col().px_3().py(px(2.0)).child(label).child(value),
            );
        }
        panel = panel.child(fields);

        // A write the engine has described but not done.
        if let Some(p) = &tab.pending_write {
            let mut ask = div()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_1()
                .bg(t.panel())
                .border_t_1()
                .border_color(t.error())
                .child(self.small(
                    p.reason.clone().unwrap_or_else(|| {
                        "this changes stored data".to_string()
                    }),
                    t.error(),
                ))
                // The statement is the ENGINE's, shown verbatim. A client that
                // displayed its own version and sent another would be asking
                // about one thing and doing a different one.
                .child(
                    div()
                        .text_size(ms.small())
                        .text_color(t.fg())
                        .child(SharedString::from(p.statement.clone())),
                );
            ask = ask.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        btn(
                            t,
                            ms,
                            "write-yes",
                            if p.delete { "Delete it" } else { "Write it" },
                            Btn::Danger,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                                this.apply_write(tab_id, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        btn(t, ms, "write-no", self.tr("write.leave_it"), Btn::Quiet)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                                    tb.pending_write = None;
                                }
                                cx.notify();
                            })),
                    ),
            );
            panel = panel.child(ask);
        }

        // Copy as Query. See `copy_as_select` for why the client is allowed to
        // write SQL here: this goes to a person, not to a database.
        let sql = self.meta(&tab.driver).map_or(false, |m| m.caps.sql);
        let mut foot = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(t.border());
        if sql {
            foot = foot
                .child(
                    btn(t, ms, "copy-select", self.tr("write.copy_select"), Btn::Link)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_row(false, cx);
                            cx.notify();
                        })),
                )
                .child(
                    btn(t, ms, "copy-insert", self.tr("write.copy_insert"), Btn::Link)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_row(true, cx);
                            cx.notify();
                        })),
                );
        } else {
            foot = foot.child(self.small("this driver does not speak SQL", t.muted()));
        }
        if editable && tab.pending_write.is_none() {
            foot = foot.child(div().flex_1()).child(
                btn(t, ms, "row-delete", self.tr("write.delete_row"), Btn::Danger)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.propose_write(tab_id, i, Vec::new(), true, cx);
                        cx.notify();
                    })),
            );
        }
        Some(panel.child(foot).into_any_element())
    }

    /// The query bar: the driver's declared fields, or one statement box.
    ///
    /// Which one is NOT decided here — `is_structured` is true exactly when the
    /// engine sent a `query_form` for that driver. MongoDB sends four fields and
    /// the JSON each contributes; every SQL driver sends none and gets a box.
    fn query_bar(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let meta = self.meta(&tab.driver);
        let structured = meta.map(|m| m.is_structured()).unwrap_or(false);
        let tab_id = tab.id;

        let mut bar = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border());

        if structured {
            for f in &meta.unwrap().query_form {
                let Some(input) = self.inputs.get(&(tab_id, f.key.clone())) else { continue };
                bar = bar.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(self.small(f.label.clone(), t.muted()))
                        // A limit box does not need the width a filter does.
                        .child(div().w(px(ms.base() * if f.key == "limit" { 4.5 } else { 15.5 })).child(self.field(ms.small(), input.clone()))),
                );
            }
        } else if let Some(ed) = self.editors.get(&tab_id) {
            // Its own row, full width: an editor three to sixteen lines tall
            // beside a Run button is a column of buttons down one side.
            bar = bar
                .flex_col()
                .child(
                    div()
                        .w_full()
                        .border_1()
                        .border_color(t.border())
                        .text_size(ms.small())
                        .child(ed.clone()),
                );
        }

        // A browse the tree produced can be narrowed. The fields come from the
        // DRIVER — it is the one that knows where a WHERE goes in its own
        // dialect, and which dialects have one at all.
        let browse = meta
            .filter(|_| tab.object.is_some())
            .map(|m| m.browse_form.clone())
            .unwrap_or_default();
        let mut narrow = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border());
        let has_narrow = !browse.is_empty();
        for f in &browse {
            let Some(input) = self.inputs.get(&(tab_id, format!("browse:{}", f.key))) else {
                continue;
            };
            narrow = narrow.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(self.small(f.label.clone(), t.muted()))
                    .child(div().w(px(ms.base() * 17.0)).child(self.field(ms.small(), input.clone()))),
            );
        }
        if has_narrow {
            narrow = narrow.child(div().flex_1()).child(self.small(
                "the statement above is what runs — edit it and these stop rewriting it",
                t.muted(),
            ));
        }

        // The run button says what it does in this engine's words.
        let run_label = if structured { self.tr("result.find") } else { self.tr("result.run") };
        bar = bar.child(
            btn(t, ms, format!("run-{tab_id}"), format!("{run_label} (↵)"), Btn::Primary)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sync_tab_from_inputs(tab_id, cx);
                    this.run_tab(tab_id, cx);
                    cx.notify();
                })),
        );

        // Paging, beside what it pages.
        let can_prev = tab.state == RunState::Done && tab.from > 0;
        let can_next = tab.state == RunState::Done && tab.has_more;
        bar = bar.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .children(if can_prev {
                    Some(
                        btn(t, ms, "prev", "◀", Btn::Quiet)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page_by(-1);
                                cx.notify();
                            }))
                            .into_any_element(),
                    )
                } else {
                    Some(crate::widgets::btn_off(t, ms, "◀", Btn::Quiet).into_any_element())
                })
                .child(self.small(
                    format!("{}–{}", tab.from + 1, tab.from + tab.rows.len()),
                    t.muted(),
                ))
                .children(if can_next {
                    Some(
                        btn(t, ms, "next", "▶", Btn::Quiet)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.page_by(1);
                                cx.notify();
                            }))
                            .into_any_element(),
                    )
                } else {
                    Some(crate::widgets::btn_off(t, ms, "▶", Btn::Quiet).into_any_element())
                }),
        );
        if has_narrow {
            return div()
                .flex()
                .flex_col()
                .w_full()
                .child(bar)
                .child(narrow)
                .into_any_element();
        }
        bar.into_any_element()
    }

    /// The export row. It says what it writes — the LOADED rows — because an
    /// export that silently paged the whole table would be a different feature.
    fn export_row(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border())
            .child(self.small(
                format!("Export {} loaded row(s) as", tab.rows.len()),
                t.muted(),
            ));

        for f in [Format::Csv, Format::Tsv, Format::Json] {
            let on = self.export_format == f;
            row = row.child(
                chip(t, ms, format!("fmt-{}", f.label()), f.label(), on)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Keep the extension honest when the format changes.
                        let old = this.export_format.extension();
                        this.export_format = f;
                        let path = this.export_path.read(cx).text().to_string();
                        if path.ends_with(old) {
                            let stem = path.trim_end_matches(old).trim_end_matches('.').to_string();
                            let next = format!("{stem}.{}", f.extension());
                            this.export_path.update(cx, |i, cx| i.set_text(next, cx));
                        }
                        cx.notify();
                    })),
            );
        }

        row.child(div().w(px(ms.base() * 18.0)).child(self.field(ms.small(), self.export_path.clone())))
            // The file CHOOSER, so a path does not have to be typed. The same
            // portal the connection window's "browse…" uses.
            .child(
                btn(t, ms, "export-pick", self.tr("form.browse"), Btn::Secondary).on_click(
                    cx.listener(|this, _, _, cx| {
                        let input = this.export_path.clone();
                        crate::form::pick_new_path(
                            input,
                            |why, cx| {
                                let _ = cx;
                                eprintln!("lode: {why}");
                            },
                            cx,
                        );
                    }),
                ),
            )
            .child(
                btn(t, ms, "export-write", self.tr("export.loaded"), Btn::Primary)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.write_export(cx);
                        cx.notify();
                    })),
            )
            // ALL of it, not just what is on screen. The row has always said
            // "loaded row(s)" and been honest about it; this is the other
            // answer, and the one somebody exporting a table actually wants.
            .children((tab.total.unwrap_or(0) > tab.rows.len() || tab.has_more).then(|| {
                btn(t, ms, "export-all", self.tr("export.all"), Btn::Secondary).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.start_export_all(cx);
                        cx.notify();
                    }),
                )
            }))
            .into_any_element()
    }

    fn table_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let cols: Vec<(usize, String)> = tab
            .grid_columns()
            .into_iter()
            .map(|(i, c)| (i, c.name.clone()))
            .collect();
        if cols.is_empty() {
            return div().p_3().child(self.small(self.tr("result.no_columns"), t.muted())).into_any_element();
        }
        // A statement that ran and matched nothing. It drew the headings over
        // an empty rectangle, which reads as a result still loading.
        if tab.rows.is_empty() && tab.state == RunState::Done {
            return div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(self.empty_state("empty.rows", "empty.rows_hint"))
                .into_any_element();
        }

        let sort = tab.sort;
        let tab_id = tab.id;
        // The grid's REAL width. A scrolling container needs content wider than
        // itself or there is nothing to scroll, and the width has to be exact:
        // one pixel short and the last column is unreachable, which is the bug
        // this is fixing.
        let total_w: f32 = ms.num_w()
            + cols
                .iter()
                .map(|(ci, _)| tab.widths.get(ci).copied().unwrap_or(ms.col_w()) + 3.0)
                .sum::<f32>();
        let mut header = div()
            .flex()
            .w_full()
            .bg(t.header_bg())
            .border_b_1()
            .border_color(t.border())
            .flex_none()
            .overflow_hidden()
            .child(
                div()
                    .w(px(ms.num_w()))
                    .flex_none()
                    .px(px(ms.cell_pad()))
                    .child(self.small("#", t.header_fg())),
            );
        for (ci, name) in &cols {
            let (ci, name) = (*ci, name.clone());
            let arrow = match sort {
                Some((c, asc)) if c == ci => {
                    if asc {
                        " \u{f0de}"
                    } else {
                        " \u{f0dd}"
                    }
                }
                _ => "",
            };
            header = header.child(
                div()
                    .id(SharedString::from(format!("hdr-{tab_id}-{ci}")))
                    .w(px(tab.widths.get(&ci).copied().unwrap_or(ms.col_w())))
                    // A long column name is cut like a long value: a header that
                    // wrapped would make the header taller than the rows and
                    // every row would be drawn at the wrong offset.
                    .flex_none()
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover_mark()))
                    .child({
                        // The NAME with its type, and under it how much of the
                        // page actually has the field. A document store
                        // declares no column types, so the only honest source
                        // is the page in hand — which is why the count it is
                        // out of is shown beside the percentage and never on
                        // its own.
                        let w = tab.widths.get(&ci).copied().unwrap_or(ms.col_w());
                        let declared = tab
                            .columns
                            .get(ci)
                            .map(|c| c.type_name.clone())
                            .filter(|s| !s.trim().is_empty());
                        let sum = crate::workspace::column_summary(
                            &tab.rows,
                            ci,
                            declared.as_deref(),
                        );
                        let mut head = div().flex().flex_col().w(px(w)).overflow_hidden();
                        head = head.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .w(px(w))
                                .overflow_hidden()
                                .px(px(ms.cell_pad()))
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .text_size(ms.small())
                                        .text_color(t.header_fg())
                                        .child(SharedString::from(name.clone())),
                                )
                                // The sort arrow, its OWN element. Glued to the
                                // end of the name it was cut off with the name:
                                // a column narrow enough to truncate lost the
                                // one mark that says how it is ordered.
                                .children((!arrow.is_empty()).then(|| {
                                    div()
                                        .flex_none()
                                        .text_size(ms.tiny())
                                        .text_color(t.accent())
                                        .child(arrow.trim())
                                }))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(ms.tiny())
                                        .text_color(t.badge(sum.kind))
                                        .child(sum.kind),
                                ),
                        );
                        // Only when some of the page is MISSING the field. A bar
                        // that is always full and a line that always says
                        // "100% of 200" are a row of the grid spent saying
                        // nothing — and they hid the columns that do have gaps.
                        if sum.total > 0 && sum.present < sum.total {
                            let frac = (sum.present as f32 / sum.total as f32).clamp(0.0, 1.0);
                            head = head
                                .child(
                                    // The bar, then the number. A bar alone is a
                                    // feeling; a number alone is read without
                                    // noticing. Both is how it is scanned.
                                    div()
                                        .flex()
                                        .w(px(w))
                                        .h(px(2.0))
                                        .flex_none()
                                        .bg(t.border())
                                        .child(
                                            div()
                                                .w(gpui::relative(frac))
                                                .h_full()
                                                .bg(if sum.present == sum.total {
                                                    t.success()
                                                } else {
                                                    t.warning()
                                                }),
                                        ),
                                )
                                .child(
                                    div()
                                        .w(px(w))
                                        .truncate()
                                        .px(px(ms.cell_pad()))
                                        .text_size(ms.tiny())
                                        .text_color(t.warning())
                                        .child(SharedString::from(format!(
                                            "{} of {}",
                                            sum.present, sum.total
                                        ))),
                                );
                        }
                        head
                    })
                    // The whole story on hover, including the percentage, which
                    // does not have to take a line of the grid to be available.
                    .tooltip({
                        let (th, sc) = (self.theme.clone(), self.scale);
                        let sum = crate::workspace::column_summary(
                            &tab.rows,
                            ci,
                            tab.columns
                                .get(ci)
                                .map(|c| c.type_name.as_str())
                                .filter(|s| !s.trim().is_empty()),
                        );
                        let words = format!(
                            "{name} — {} · {}% of {} row(s) have it",
                            sum.kind,
                            sum.percent(),
                            sum.total
                        );
                        crate::tip::Tip::build(words, None, th, sc)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.sort_by(tab_id, ci);
                        cx.notify();
                    })),
            );
            // The handle between this column and the next. It is its own strip
            // so that dragging never sorts: a click that moved is not a click.
            let start_w = tab.widths.get(&ci).copied().unwrap_or(ms.col_w());
            header = header.child(
                div()
                    .id(SharedString::from(format!("grip-{tab_id}-{ci}")))
                    .w(px(3.0))
                    .h_full()
                    // A grip is something to aim a pointer at, so it uses the
                    // visible `divider` rather than the box-edge `border`,
                    // which vanishes into the base on half the palettes.
                    .bg(t.divider())
                    .hover(|d| d.bg(t.accent()))
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                            this.resizing = Some((tab_id, ci, f32::from(ev.position.x), start_w));
                            cx.notify();
                        }),
                    ),
            );
        }

        let rows: Vec<Vec<Value>> = tab.rows.clone();
        let first = tab.from;
        let (num_fg, null_fg, row_alt, fg) = (t.row_number(), t.null_fg(), t.row_alt(), t.fg());
        let cols2 = cols.clone();
        let widths: HashMap<usize, f32> = tab.widths.clone();
        // Which columns hold NUMBERS, worked out once for the page rather than
        // per cell: a column of right-aligned numbers lines its digits up by
        // place value, which is most of what scanning a column of amounts is.
        let aligns: HashMap<usize, crate::workspace::Align> = cols
            .iter()
            .map(|(ci, _)| {
                let declared = tab
                    .columns
                    .get(*ci)
                    .map(|c| c.type_name.as_str())
                    .filter(|x| !x.trim().is_empty());
                let sum = crate::workspace::column_summary(&tab.rows, *ci, declared);
                (*ci, crate::workspace::cell_align(sum.kind))
            })
            .collect();

        let list = uniform_list(
            "rows",
            rows.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx2| {
                // Read from the workspace rather than from a captured copy: a
                // captured one is this frame's answer to last frame's question,
                // and the editor has to appear in the frame the click lands in.
                let tb = this.tabs.iter().find(|t| t.id == tab_id);
                let editing_cell = tb.and_then(|t| t.editing);
                let editable_here = tb.map_or(false, |t| {
                    this.meta(&t.driver).map_or(false, |m| m.caps.edit) && t.object.is_some()
                });
                let edit_input = this.inputs.get(&(tab_id, "edit".to_string())).cloned();
                let hover_bg = this.t().hover_mark();
                // WHICH ROW AM I ON. Read from the workspace, like the editor
                // above: the grid drew no cursor at all, so `j` and `k` moved
                // something invisible and a click on a row did nothing anybody
                // could see.
                let here = tb.and_then(|t| t.selected);
                // WHICH CELL. Only on the row the cursor is on — a column
                // highlight running down the whole grid would be a second
                // cursor competing with the first.
                let here_col = tb.map(|t| t.cursor_col).unwrap_or(0);
                let (sel_bg, accent) = (this.t().selected_mark(), this.t().accent());
                let mut out = Vec::new();
                for i in range {
                    let on = here == Some(i);
                    let mut line = div()
                        .id(SharedString::from(format!("row-{tab_id}-{i}")))
                        .flex()
                        .w_full()
                        .h(px(ms.row_h()))
                        // The same mark the rail uses for the chosen panel: a
                        // filled background AND an edge. The background alone
                        // is nearly invisible on several light palettes even
                        // after `selected_mark`; the edge is unmistakable.
                        .when(on, |d| d.bg(sel_bg).border_l_2().border_color(accent))
                        // The edge is reserved on EVERY row, in the row's own
                        // colour: without it the chosen row is two pixels wider
                        // than its neighbours and the whole grid shifts as the
                        // cursor moves.
                        .when(!on, |d| d.border_l_2().border_color(gpui::transparent_black()))
                        .on_click(cx2.listener(move |this, _, _, cx| {
                            this.select_row(tab_id, i, cx);
                        }))
                        // The right button on a row: show it, copy it three
                        // ways, edit it, delete it. Copying a row at all was
                        // possible only as part of an export.
                        .on_mouse_down(
                            MouseButton::Right,
                            cx2.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                                this.select_row(tab_id, i, cx);
                                let editable = this
                                    .tabs
                                    .iter()
                                    .find(|t| t.id == tab_id)
                                    .map_or(false, |t| {
                                        this.meta(&t.driver).map_or(false, |m| m.caps.edit)
                                            && t.object.is_some()
                                    });
                                this.open_menu(
                                    crate::menu::Target::Row { tab: tab_id, row: i, editable },
                                    ev.position,
                                    cx,
                                );
                            }),
                        )
                        // ONE ROW TALL, whatever is in it. `uniform_list` puts
                        // every row at a fixed offset, so a cell that grows
                        // taller than its row does not push the next one down —
                        // it paints OVER it. A MongoDB document with a nested
                        // array in a column turned rows two to twelve into one
                        // unreadable stack, and the last row poured a column of
                        // text hundreds of pixels into the empty space below.
                        .flex_none()
                        .overflow_hidden()
                        // Alternating rows: the stripe is a surface from the
                        // theme, not a computed shade.
                        .when(i % 2 == 1, |d| d.bg(row_alt))
                        .child(
                            div()
                                .w(px(ms.num_w()))
                                .flex_none()
                                .truncate()
                                .px(px(ms.cell_pad()))
                                .text_size(ms.tiny())
                                .text_color(num_fg)
                                .child(SharedString::from((first + i + 1).to_string())),
                        );
                    for (at, (ci, _)) in cols2.iter().enumerate() {
                        let on_cell = on && at == here_col;
                        let cell = rows.get(i).and_then(|r| r.get(*ci));
                        let (text, colour, is_null) = match cell.and_then(cell_text) {
                            None => ("NULL".to_string(), null_fg, true),
                            Some(s) => (s.replace('\n', "⏎"), fg, false),
                        };
                        let right = aligns.get(ci).copied()
                            == Some(crate::workspace::Align::Right);
                        let w = widths.get(ci).copied().unwrap_or(ms.col_w());
                        // A key column is not editable — changing it would move
                        // the row rather than change it — and neither is a
                        // column the engine never mentioned.
                        let col_editable =
                            tb.map_or(false, |t| crate::workspace::column_editable(t, *ci));
                        // The GRID had no way to edit at all. Editing existed in
                        // the detail panel only, which is not where somebody
                        // looks for it — "there is no editing in table" was
                        // exactly right.
                        if editable_here && editing_cell == Some((i, *ci)) {
                            if let Some(input) = edit_input.clone() {
                                line = line
                                    .child(
                                        div()
                                            .w(px(w))
                                            .flex_none()
                                            .overflow_hidden()
                                            .px(px(ms.cell_pad()))
                                            .text_size(ms.body())
                                            .child(input),
                                    )
                                    .child(div().w(px(3.0)).flex_none());
                                continue;
                            }
                        }
                        line = line
                            .child(
                                // ONE LINE, cut at the column's edge with an
                                // ellipsis. A grid is for scanning; the whole
                                // value is read in the detail panel and in the
                                // JSON view, which exist for exactly that.
                                // Making a row as tall as its tallest cell
                                // would mean measuring every row before laying
                                // out any, which is what `uniform_list` exists
                                // not to do — and it would buy a worse grid.
                                // A block inside the flex slot, both with the
                                // same real width. A flex ITEM is measured by
                                // the flex algorithm before its own width is
                                // settled, and GPUI draws the ellipsis only
                                // against a width known at measure time — so
                                // the slot alone cut the text off mid-glyph
                                // with no `…` to say it had.
                                div()
                                    .id(SharedString::from(format!("cell-{tab_id}-{i}-{ci}")))
                                    .w(px(w))
                                    .flex_none()
                                    .overflow_hidden()
                                    // Visibly editable: the pointer changes and
                                    // the cell lights, so the gesture is
                                    // discoverable instead of being something to
                                    // be told about.
                                    .when(editable_here && col_editable, |d| {
                                        d.cursor_pointer().hover(move |h| h.bg(hover_bg))
                                    })
                                    // The cell under the cursor is OUTLINED,
                                    // not filled: the row it is on is already
                                    // filled, and a second fill on top of it
                                    // reads as a different kind of selection.
                                    .when(on_cell, |d| d.border_1().border_color(accent))
                                    .child(
                                        div()
                                            .w(px(w))
                                            .truncate()
                                            .px(px(ms.cell_pad()))
                                            .text_size(ms.body())
                                            .text_color(colour)
                                            .when(right, |d| d.text_right())
                                            // NULL is not a value called NULL.
                                            // Italic says it is the absence of
                                            // one, so a cell holding the four
                                            // letters as TEXT can still be told
                                            // apart from an empty one.
                                            .when(is_null, |d| d.italic())
                                            .child(SharedString::from(text)),
                                    )
                                    .on_click(cx2.listener({
                                        let ci = *ci;
                                        move |this, _, _, cx| {
                                            this.click_cell(tab_id, i, ci, cx);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(div().w(px(3.0)).flex_none());
                    }
                    out.push(line);
                }
                out
            }),
        )
        .track_scroll(self.grid_rows.clone())
        .h_full();

        // Header and rows share ONE horizontally scrolling parent, so the
        // headings move with their columns for free. Headings that stay put
        // while the data slides under them are worse than no headings: every
        // value would be read under the wrong name.
        //
        // The row-number column is NOT pinned. Pinning it means a second column
        // outside the scroller, which means a second virtualised list, which
        // means two lists sharing one vertical scroll position — real work for
        // a small gain, and this grid already numbers its rows in order. Said
        // here rather than left to be wondered about.
        let body = div()
            .id("grid-h")
            .flex_1()
            .overflow_x_scroll()
            .track_scroll(&self.grid_scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(total_w))
                    .h_full()
                    .child(header)
                    .child(div().flex_1().child(list)),
            )
            // Three ways, because people try them in this order and conclude
            // from the FIRST one that there is no horizontal scrolling at all.
            //
            //   1. A horizontal delta of its own — two fingers on a trackpad, a
            //      mouse wheel that tilts. This is what gets tried first and it
            //      was the one being thrown away: the handler returned unless
            //      shift was held, so the most natural gesture did nothing.
            //   2. Shift and the wheel, for a mouse with one wheel.
            //   3. The bar below, which now drags.
            //
            // A plain vertical wheel is left alone so it still scrolls rows.
            .on_scroll_wheel(cx.listener(move |this, ev: &gpui::ScrollWheelEvent, _, cx| {
                let d = ev.delta.pixel_delta(px(ms.row_h()));
                let (dx, dy) = (f32::from(d.x), f32::from(d.y));
                if dx.abs() > 0.0 {
                    this.scroll_grid(-dx);
                    cx.notify();
                    return;
                }
                if ev.modifiers.shift && dy.abs() > 0.0 {
                    this.scroll_grid(dy);
                    cx.notify();
                }
            }));

        let mut out = div().flex().flex_col().size_full().child(body);
        // A visible bar. GPUI draws none for a scrolling div, and a grid that
        // scrolls with no sign that it scrolls is a grid whose extra columns
        // are still invisible — which was the whole complaint.
        // The MEASURED width of the scroller, not a width derived from the
        // window. The derivation subtracts the rail, the panel and a padding,
        // and every one of those is a chance to be wrong by a few pixels — and
        // being wrong HIGH gives a thumb that is nearly full while the last
        // columns cannot be reached, which is one fault wearing two faces.
        //
        // `bounds()` is only filled once the element has been painted, so the
        // first frame falls back to the derivation and every frame after uses
        // the truth.
        let measured = f32::from(self.grid_scroll.bounds().size.width);
        let shown = if measured > 1.0 { measured } else { self.canvas_w.max(1.0) };
        let bar = crate::workspace::hbar(
            total_w,
            shown,
            f32::from(self.grid_scroll.offset().x),
        );
        self.grid_overflow.set(bar.map(|b| b.max).unwrap_or(0.0));
        if let Some(b) = bar {
            let (frac, at) = (b.frac, b.at);
            // A flex row with a spacer, not an absolutely positioned thumb.
            // The absolute version drew nothing: a percentage width inside a
            // six-pixel absolutely positioned box is the kind of thing that
            // resolves to zero without complaining, and a scrollbar that is
            // invisible is the bug it was added to fix.
            let dragging = self.hbar_drag.get();
            out = out.child(
                div()
                    .id("grid-hbar")
                    .flex()
                    .w_full()
                    // Thicker than the six pixels it was: a bar has to be
                    // grabbable, and a seven-pixel target is a target people
                    // miss. Derived, so it stays grabbable at every text size.
                    .h(px(ms.hbar_h() * 2.0))
                    .flex_none()
                    .bg(t.status())
                    .cursor_pointer()
                    .child(div().w(gpui::relative(at)).h_full())
                    .child(
                        div()
                            .w(gpui::relative(frac))
                            .h_full()
                            // The thumb says it is being dragged. Both arms of
                            // this were the accent, so a drag looked exactly
                            // like a hover and the bar gave no feedback at all.
                            .bg(if dragging { t.fg() } else { t.accent() }),
                    )
                    // Pressing anywhere on the track jumps there, which is what
                    // a click on a scrollbar's empty half should do anyway.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _, cx| {
                            this.hbar_drag.set(true);
                            this.drag_hbar_to(f32::from(ev.position.x), total_w);
                            cx.notify();
                        }),
                    ),
            );
        }
        // The move and the release are on the WHOLE result area, not on the bar:
        // a drag that stopped the moment the pointer left a twelve-pixel strip
        // would be a drag nobody could finish.
        let out = out
            .on_mouse_move(cx.listener(move |this, ev: &gpui::MouseMoveEvent, _, cx| {
                if this.hbar_drag.get() {
                    this.drag_hbar_to(f32::from(ev.position.x), total_w);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    if this.hbar_drag.replace(false) {
                        cx.notify();
                    }
                }),
            );
        out.into_any_element()
    }

    /// Where the result area starts, in window coordinates.
    ///
    /// The rail plus the panel plus the left padding — the same subtraction the
    /// layout performs, so the two cannot disagree about anything but a resize
    /// in flight. It had been left at `base * 3.1`, the rail's width BEFORE the
    /// rail got its own size, so every figure derived from it was out by
    /// twenty-odd pixels: the grid thought it was wider than it was and the
    /// scrollbar thought there was less to scroll.
    fn grid_left(&self) -> f32 {
        self.scale.rail_w() + self.scale.panel_w() + 8.0
    }

    /// Put the grid where a pointer at `x` says, treating the bar as a map of
    /// the whole width.
    fn drag_hbar_to(&self, x: f32, total_w: f32) {
        // The same measured width and the same left edge the BAR was drawn
        // from. Drawing from one number and dragging from another is how a
        // thumb ends up not following the pointer.
        let b = self.grid_scroll.bounds();
        let measured = f32::from(b.size.width);
        let shown = if measured > 1.0 { measured } else { self.canvas_w.max(1.0) };
        let left = if measured > 1.0 { f32::from(b.origin.x) } else { self.grid_left() };
        let next = crate::workspace::hbar_offset(
            x,
            left,
            shown,
            total_w,
            self.grid_overflow.get(),
        );
        self.grid_scroll
            .set_offset(gpui::point(px(next), self.grid_scroll.offset().y));
    }

    /// Move the grid sideways by `dx` pixels, clamped to what there is.
    ///
    /// One handle on the workspace rather than one per tab: only one grid is
    /// being looked at, and a per-tab handle would be state that outlives the
    /// thing it describes.
    fn scroll_grid(&self, dx: f32) {
        // The derived overflow, not the handle's — see `grid_overflow`.
        let max = self.grid_overflow.get();
        let now = f32::from(self.grid_scroll.offset().x);
        let next = clamped_scroll(now, dx, max);
        self.grid_scroll
            .set_offset(gpui::point(px(next), self.grid_scroll.offset().y));
    }

    /// The document view: one expandable record per row, with a type badge on
    /// every field. This is what a document store should open in, and what the
    /// previous client could not show at all.
    fn tree_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let mut out = div().flex().flex_col().size_full().overflow_hidden();
        // Everything on screen, in one go — the answer to "I want this
        // somewhere else" when the something else is the whole result.
        if !tab.rows.is_empty() {
            let docs: Vec<Value> = (0..tab.rows.len().min(200)).map(|i| tab.document(i)).collect();
            let all = serde_json::to_string_pretty(&docs).unwrap_or_else(|_| "[]".into());
            out = out.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(self.small(format!("{} document(s)", tab.rows.len()), t.muted()))
                    .child(
                        btn(t, ms, "tree-copy-all", "copy all", Btn::Link)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(all.clone()));
                                this.toast("copied".to_string(), false);
                                cx.notify();
                            })),
                    ),
            );
        }
        for i in 0..tab.rows.len().min(200) {
            let doc = tab.document(i);
            let open = tab.expanded.contains(&i);
            let tab_id = tab.id;
            out = out.child(
                div()
                    .id(SharedString::from(format!("doc-{tab_id}-{i}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(2.0))
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover_mark()))
                    .child(self.small(format!("{}", i + 1), t.row_number()))
                    .child(self.small(if open { "\u{f0d7}" } else { "\u{f0da}" }, t.muted()))
                    // The first few fields, not `{6}`. Thirty rows of `{6}`,
                    // `{5}`, `{7}` cannot be told apart without opening every
                    // one, which is the opposite of what a list is for.
                    .child(self.line_tip(
                        crate::workspace::doc_preview(&doc, 120),
                        ms.small(),
                        t.fg(),
                        (self.canvas_w - ms.base() * 6.0).max(ms.base() * 10.0),
                        format!("dp-{tab_id}-{i}"),
                    ))
                    .child(
                        btn(t, ms, format!("dcopy-{tab_id}-{i}"), "copy", Btn::Link)
                            .on_click(cx.listener({
                                let text = serde_json::to_string_pretty(&doc)
                                    .unwrap_or_else(|_| "{}".into());
                                move |this, _, _, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        text.clone(),
                                    ));
                                    this.toast("copied".to_string(), false);
                                    cx.notify();
                                }
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|t| t.id == tab_id) {
                            if !tb.expanded.remove(&i) {
                                tb.expanded.insert(i);
                            }
                        }
                        cx.notify();
                    })),
            );
            if open {
                out = out.child(self.value_el(&doc, tab, 1, Some(i), cx));
            }
        }
        out.into_any_element()
    }

    /// The in-place editor: the field, a tick and a cross.
    ///
    /// One definition, used wherever a value is edited. The tree edits through
    /// exactly this, so a document's field and a table's cell take the same
    /// road out — `commit_edit`, `propose_write`, the engine's `row.write`, the
    /// same classification, the same confirmation and the same audit row. There
    /// is no second, softer path, which is the only way the promise about
    /// writing means anything.
    fn editor_el(&self, tab_id: u64, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (t, ms) = (self.t(), self.scale);
        let Some(input) = self.inputs.get(&(tab_id, "edit".to_string())) else {
            return div().into_any_element();
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().flex_1().child(self.field(ms.small(), input.clone())))
            .child(
                btn(t, ms, "edit-ok", "\u{f00c}", Btn::Link)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.commit_edit(tab_id, cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("edit-no")
                    .tooltip(self.tip("tip.cancel_edit", Some("esc")))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(t.muted())
                    .child("\u{f00d}")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(tb) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                            tb.editing = None;
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// One value inside a document, with its type beside it.
    fn value_el(
        &self,
        v: &Value,
        tab: &Tab,
        depth: usize,
        // Which result ROW this document is, when it is one. Without it a field
        // cannot be addressed and so cannot be written.
        row: Option<usize>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let t = self.t();
        let ms = self.scale;
        let mut out = div().flex().flex_col();
        let tab_id_for_edit = tab.id;
        match v {
            Value::Object(m) => {
                for (k, val) in m {
                    let declared = tab
                        .columns
                        .iter()
                        .find(|c| &c.name == k)
                        .map(|c| c.type_name.as_str());
                    let badge = type_badge(val, declared);
                    let fgroup = SharedString::from(format!("tf-{}-{k}", tab.id));
                    out = out.child(
                        div()
                            .group(fgroup.clone())
                            .flex()
                            .items_center()
                            .gap_2()
                            .pl(px(ms.marker_w() + depth as f32 * ms.indent()))
                            .py(px(1.0))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(ms.small())
                                    .text_color(t.muted())
                                    .child(SharedString::from(k.clone())),
                            )
                            // BEFORE the value, not after it. On the right it
                            // was the last thing in the row and the first thing
                            // the edge cut off, so the one word saying what the
                            // value IS was the one word that went missing.
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(ms.tiny())
                                    .text_color(t.badge(badge))
                                    .child(SharedString::from(badge)),
                            )
                            .child({
                                // A TOP-LEVEL field of a document that is a
                                // result row can be written, so it is editable
                                // here exactly as a cell is in the grid.
                                let col = tab.columns.iter().position(|c| &c.name == k);
                                // A field whose value is an OBJECT or an ARRAY
                                // is not offered: `row.write` sets one value,
                                // and what the box would be filled with is a
                                // blob of JSON that nobody can sensibly retype.
                                // Shown and not clickable, like a nested field —
                                // an editor that opens on something it cannot
                                // write is worse than no editor.
                                let scalar = !matches!(val, Value::Object(_) | Value::Array(_));
                                let col = if scalar { col } else { None };
                                match (row, col) {
                                    (Some(r), Some(ci)) if tab.editing == Some((r, ci)) => {
                                        self.editor_el(tab.id, cx)
                                    }
                                    (Some(r), Some(ci)) => div()
                                        .id(SharedString::from(format!("tv-{}-{r}-{ci}", tab.id)))
                                        .flex_1()
                                        .cursor_pointer()
                                        .hover(|d| d.bg(t.hover_mark()))
                                        .text_size(ms.small())
                                        .text_color(t.fg())
                                        .child(SharedString::from(preview(val)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            let id = this
                                                .tabs
                                                .iter()
                                                .find(|x| x.id == tab_id_for_edit)
                                                .map(|x| x.id);
                                            if let Some(id) = id {
                                                this.begin_edit(id, r, ci, cx);
                                            }
                                            cx.notify();
                                        }))
                                        .into_any_element(),
                                    _ => div()
                                        .flex_1()
                                        .text_size(ms.small())
                                        .text_color(t.fg())
                                        .child(SharedString::from(preview(val)))
                                        .into_any_element(),
                                }
                            })
                            // A value that can be seen and not taken is half
                            // useless, and GPUI has no selectable text. UNDER
                            // THE POINTER, though: a document of forty fields
                            // had forty "copy" links down its right-hand side,
                            // and the column of them was read before the data.
                            .child({
                                let raw = match val {
                                    Value::String(s) => s.clone(),
                                    other => serde_json::to_string_pretty(other)
                                        .unwrap_or_else(|_| other.to_string()),
                                };
                                div()
                                    .id(SharedString::from(format!(
                                        "fcopy-{}-{}-{k}",
                                        tab_id_for_edit,
                                        row.unwrap_or(0)
                                    )))
                                    .flex_none()
                                    .px_1()
                                    .invisible()
                                    .group_hover(fgroup.clone(), |d| d.visible())
                                    .cursor_pointer()
                                    .hover(|d| d.bg(t.hover_mark()))
                                    .text_size(ms.tiny())
                                    .text_color(t.muted())
                                    .child("copy")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            raw.clone(),
                                        ));
                                        this.toast("copied".to_string(), false);
                                        cx.notify();
                                    }))
                            }),
                    );
                    // Not while this field is being edited: the expansion
                    // under an open editor is what made the two read as one
                    // smear of text.
                    let editing_this = row
                        .zip(tab.columns.iter().position(|c| &c.name == k))
                        .map_or(false, |(r, ci)| tab.editing == Some((r, ci)));
                    if matches!(val, Value::Object(_)) && depth < 4 && !editing_this {
                        // A nested object's fields are NOT offered for editing:
                        // `row.write` sets a top-level field, and pretending to
                        // write `a.b.c` by sending `c` would write the wrong
                        // thing. Shown, not editable, is the honest pairing.
                        out = out.child(self.value_el(val, tab, depth + 1, None, cx));
                    }
                }
            }
            other => {
                out = out.child(
                    div()
                        .pl(px(ms.marker_w() + depth as f32 * ms.indent()))
                        .text_size(ms.small())
                        .text_color(t.fg())
                        .child(SharedString::from(preview(other))),
                );
            }
        }
        out.into_any_element()
    }

    /// JSON, coloured by what each run IS — see `workspace::json_runs`.
    ///
    /// `StyledText` with one run per piece rather than a `div` per token: a
    /// forty-field document is a few hundred tokens, and that many elements per
    /// document per frame is a layout pass nobody is paying for. The runs are
    /// byte lengths that cover the text exactly, which is what the text system
    /// requires — a gap is a panic, not a wrong colour.
    fn json_text(&self, text: &str) -> gpui::StyledText {
        let t = self.t();
        let runs: Vec<gpui::TextRun> = crate::workspace::json_runs(text)
            .into_iter()
            .map(|(len, role)| gpui::TextRun {
                len,
                font: gpui::Font {
                    family: self.font_family.clone().into(),
                    features: Default::default(),
                    fallbacks: None,
                    weight: Default::default(),
                    style: Default::default(),
                },
                color: t.json_role(role).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            })
            .collect();
        // The SIZE comes from the element this sits in, not from the run: a
        // `TextRun` names a family and a colour, and the parent's `text_size`
        // is what the layout measures against.
        gpui::StyledText::new(SharedString::from(text.to_string())).with_runs(runs)
    }

    fn json_el(&self, tab: &Tab, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (t, ms) = (self.t(), self.scale);
        let tab_id = tab.id;
        let can_edit = self.meta(&tab.driver).map_or(false, |m| m.caps.edit)
            && tab.object.is_some();
        // `to_string_pretty` is fixed at two spaces, so the indent is chosen by
        // handing the formatter the exact run of spaces to use.
        let pad = " ".repeat(self.json_indent as usize);
        let render = |v: &Value| -> String {
            let mut buf = Vec::new();
            let fmt = serde_json::ser::PrettyFormatter::with_indent(pad.as_bytes());
            let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
            match serde::Serialize::serialize(v, &mut ser) {
                Ok(()) => String::from_utf8(buf).unwrap_or_else(|_| "{}".into()),
                Err(_) => "{}".into(),
            }
        };

        let mut out = div().id("json-view").size_full().overflow_y_scroll().p_2();
        // One block per DOCUMENT rather than one blob for all of them. A blob
        // can be read and nothing in it can be pointed at; a document that can
        // be pointed at is a document that can be edited.
        for i in 0..tab.rows.len().min(200) {
            let doc = tab.document(i);
            let editing = tab.editing_doc == Some(i);
            let group = SharedString::from(format!("jdoc-g-{tab_id}-{i}"));
            let mut block = div()
                .group(group.clone())
                .flex()
                .flex_col()
                .w_full()
                .py_1()
                .border_b_1()
                .border_color(t.border());
            let text = render(&doc);
            // The number stays; "copy" and "edit" come out UNDER THE POINTER.
            // Two hundred documents each carrying two controls is four hundred
            // links down the view, and the eye reads the column of them before
            // it reads any of the data.
            let mut head = div()
                .flex()
                .items_center()
                .gap_2()
                .child(self.small(format!("{}", i + 1), t.row_number()))
                .child(
                    div()
                        .invisible()
                        .group_hover(group.clone(), |d| d.visible())
                        .child(self.copy_el(
                            format!("jcopy-{tab_id}-{i}"),
                            text.clone(),
                            "copy",
                            cx,
                        )),
                );
            if can_edit && !editing {
                head = head.child(
                    div()
                        .invisible()
                        .group_hover(group.clone(), |d| d.visible())
                        .child(
                            btn(t, ms, format!("jedit-{tab_id}-{i}"), "edit", Btn::Link).on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.begin_doc_edit(tab_id, i, cx);
                                }),
                            ),
                        ),
                );
            }
            block = block.child(head);

            if editing {
                // Every writable field at once. Saving sends only what CHANGED,
                // as a single `row.write` — one classification, one
                // confirmation, one audit row for one document.
                for (ci, col) in tab.columns.iter().enumerate() {
                    let Some(input) = self.inputs.get(&(tab_id, format!("doc-{}", col.name)))
                    else {
                        // Not editable: shown, so the document still reads whole.
                        let shown = tab
                            .rows
                            .get(i)
                            .and_then(|r| r.get(ci))
                            .map(|v| preview(v))
                            .unwrap_or_default();
                        block = block.child(
                            div()
                                .flex()
                                .gap_2()
                                .py(px(1.0))
                                .child(div().w(px(ms.base() * 9.0)).child(
                                    self.small(col.name.clone(), t.muted()),
                                ))
                                .child(self.small(shown, t.muted())),
                        );
                        continue;
                    };
                    block = block.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py(px(1.0))
                            .child(
                                div()
                                    .w(px(ms.base() * 9.0))
                                    .flex_none()
                                    .child(self.small(col.name.clone(), t.fg())),
                            )
                            .child(div().flex_1().child(self.field(ms.small(), input.clone()))),
                    );
                }
                block = block.child(
                    div()
                        .flex()
                        .gap_2()
                        .py_1()
                        .child(
                            btn(t, ms, format!("jsave-{tab_id}-{i}"), "save this document", Btn::Secondary)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.commit_doc_edit(tab_id, cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            btn(t, ms, format!("jcancel-{tab_id}-{i}"), "cancel", Btn::Quiet)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(tb) =
                                        this.tabs.iter_mut().find(|x| x.id == tab_id)
                                    {
                                        tb.editing_doc = None;
                                    }
                                    cx.notify();
                                })),
                        ),
                );
            } else {
                // A long line is scrolled to, not wrapped: a JSON document
                // rewrapped at the window's edge stops lining up, and lining up
                // is what reading it as a tree depends on.
                block = block.child(
                    div()
                        .id(SharedString::from(format!("jdoc-{tab_id}-{i}")))
                        .w_full()
                        .overflow_x_scroll()
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_size(ms.small())
                                .child(self.json_text(&text)),
                        ),
                );
            }
            out = out.child(block);
        }
        out.into_any_element()
    }

    /// The bottom: one status line, and a drawer under it when something is
    /// asked for.
    ///
    /// THREE STRIPS BECAME ONE. The console carried its own bar with its own
    /// heading, the tasks another, the status line a third — all three present
    /// whether they held anything or not, which at the default text size was
    /// about a fifth of the window spent saying what the strips were rather
    /// than showing anything. The counters in the status line are the way in:
    /// they say how much there is and open the page that holds it.
    fn bottom_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (t, ms) = (self.t(), self.scale);
        let mut out = div().flex().flex_col().w_full().flex_none();
        let Some(which) = self.drawer else {
            return out.child(self.status_el(cx));
        };

        // The page chooser, and a way out of it.
        let mut tabs = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_1()
            .flex_none()
            .bg(t.panel())
            .border_t_1()
            .border_color(t.border());
        for page in [Drawer::Tasks, Drawer::Log, Drawer::Command] {
            tabs = tabs.child(
                chip(t, ms, format!("drawer-{}", page.id()), self.tr(page.key()), page == which)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.drawer = Some(page);
                        if page == Drawer::Log {
                            this.log_unread = 0;
                        }
                        cx.notify();
                    })),
            );
        }
        tabs = tabs.child(div().flex_1()).child(
            btn(t, ms, "drawer-close", "\u{f00d}", Btn::Quiet).on_click(cx.listener(
                |this, _, _, cx| {
                    this.drawer = None;
                    cx.notify();
                },
            )),
        );
        out = out.child(tabs);

        // The page itself, at a FIXED height. A drawer that grows with its
        // content takes the result's room without being asked, and the result
        // is what the window is for.
        let mut body = div()
            .id("drawer-body")
            .flex()
            .flex_col()
            .w_full()
            .h(px(f32::from(ms.line_h()) * 7.0))
            .flex_none()
            .overflow_y_scroll()
            .bg(t.panel())
            .px_3()
            .py_1();
        match which {
            Drawer::Tasks => {
                if self.tasks.is_empty() {
                    body = body.child(self.empty_state("empty.tasks", "empty.tasks_hint"));
                }
                for task in self.tasks.iter().rev() {
                    let (id, running, call) = (task.id, task.running(), task.call_id);
                    let mut row = div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .child(self.line_in(
                            task.label.clone(),
                            ms.small(),
                            if task.failed() { t.error() } else { t.fg() },
                            ms.base() * 20.0,
                        ))
                        .child(self.small(fmt_duration(task.duration()), t.muted()));
                    // Stoppable from here too: this is where somebody looks
                    // when something is taking too long.
                    if let (true, Some(call_id)) = (running, call) {
                        row = row.child(
                            btn(t, ms, format!("drawer-stop-{id}"), "\u{f00d}", Btn::Quiet)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ask(
                                        Pending::Ignore("cancel"),
                                        "query.cancel",
                                        json!({ "call_id": call_id }),
                                    );
                                    cx.notify();
                                })),
                        );
                    }
                    body = body.child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .child(row)
                            // A failure keeps its WHOLE message: the point of
                            // keeping a failed task is that it is read.
                            .when_some(task.error.clone(), |d, e| {
                                d.child(
                                    div()
                                        .pl_3()
                                        .text_size(ms.tiny())
                                        .text_color(t.error())
                                        .child(SharedString::from(e)),
                                )
                            }),
                    );
                }
            }
            Drawer::Log => {
                if self.log.is_empty() {
                    body = body.child(self.empty_state("empty.log", "empty.log_hint"));
                }
                for (level, text) in self.log.iter().rev() {
                    let colour = match level.as_str() {
                        "error" => t.error(),
                        "warn" => t.warning(),
                        _ => t.muted(),
                    };
                    body = body.child(
                        div()
                            .w_full()
                            .text_size(ms.small())
                            .text_color(colour)
                            .child(SharedString::from(text.clone())),
                    );
                }
            }
            Drawer::Command => {
                // The same answer the line will be sent to, so the prompt
                // cannot name one database while the statement goes to another.
                let target = self.target_conn().unwrap_or_else(|| "nothing connected".into());
                body = body.child(
                    div()
                        // Its OWN key context, so `up` here walks the history
                        // and `up` in the grid still moves a row. The field
                        // inside carries `TextInput`; both are on the path, and
                        // a binding named for either one matches.
                        .key_context("Console")
                        .on_action(cx.listener(|this, _: &ConsolePrev, _, cx| {
                            this.step_console_history(true, cx)
                        }))
                        .on_action(cx.listener(|this, _: &ConsoleNext, _, cx| {
                            this.step_console_history(false, cx)
                        }))
                        .flex()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .child(self.small(format!("{target}>"), t.accent()))
                        .child(div().flex_1().child(self.field(ms.small(), self.console.clone()))),
                );
                body = body.child(self.small(
                    format!("{}", self.lang.tr("console.hint")),
                    t.muted(),
                ));
                // The confirmation. The REASON is the sentence, not a title bar.
                if let Some((_, statement, reason)) = &self.confirm {
                    body = body.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .my_1()
                            .p_2()
                            .border_1()
                            .border_color(t.error())
                            .child(self.small(reason.clone(), t.error()))
                            .child(self.small(statement.clone(), t.fg()))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        btn(t, ms, "confirm-run", self.tr("write.run_it"), Btn::Danger)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                if let Some((conn, stmt, _)) = this.confirm.take() {
                                                    this.run_statement_approved(&conn, &stmt, cx);
                                                    this.console
                                                        .update(cx, |i, cx| i.set_text("", cx));
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        btn(t, ms, "confirm-cancel", self.tr("result.cancel"), Btn::Quiet)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.confirm = None;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    );
                }
            }
        }
        out.child(body).child(self.status_el(cx))
    }

    fn status_el(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (t, ms) = (self.t(), self.scale);
        let failed = self.tasks.iter().filter(|x| x.failed()).count();
        let running = self.tasks.iter().filter(|x| x.running()).count();
        let last = self
            .tasks
            .iter()
            .rev()
            .find(|x| !x.running())
            .map(|x| format!("{} · {}", x.label, fmt_duration(x.duration())))
            .unwrap_or_default();
        div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .h(px(self.scale.status_h()))
            .px_3()
            .bg(t.status())
            .border_t_1()
            .border_color(t.border())
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(self.small(self.status.clone(), t.muted()))
                    .child(self.small(last, t.muted())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(self.small(
                        if self.store_ok { "store ok" } else { "no store" },
                        if self.store_ok { t.muted() } else { t.warning() },
                    ))
                    // The three counters, and each one OPENS the page that
                    // holds what it counts. They used to be three numbers
                    // nobody could follow: "2 failed" with no way to see which
                    // two is a number that only worries somebody.
                    .children((running > 0).then(|| {
                        btn(
                            t,
                            ms,
                            "st-running",
                            format!("{running} {}", self.lang.tr("bottom.running")),
                            Btn::Quiet,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.drawer = Some(Drawer::Tasks);
                            cx.notify();
                        }))
                    }))
                    .children((failed > 0).then(|| {
                        btn(
                            t,
                            ms,
                            "st-failed",
                            format!("{failed} {}", self.lang.tr("tasks.failed")),
                            Btn::Danger,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.drawer = Some(Drawer::Tasks);
                            cx.notify();
                        }))
                    }))
                    // Zero is not drawn: a permanent "0 unread" teaches the eye
                    // to skip the place where the number appears.
                    // WAITING. Louder than the rest, because something is
                    // held up until a person answers.
                    .children((!self.approvals.is_empty()).then(|| {
                        btn(
                            t,
                            ms,
                            "st-approvals",
                            format!("{} {}", self.approvals.len(), self.lang.tr("approve.title")),
                            Btn::Danger,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.go_rail(Rail::Audit, cx);
                        }))
                    }))
                    .children((self.log_unread > 0).then(|| {
                        btn(
                            t,
                            ms,
                            "st-unread",
                            format!("{} {}", self.log_unread, self.lang.tr("bottom.unread")),
                            Btn::Quiet,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.drawer = Some(Drawer::Log);
                            this.log_unread = 0;
                            cx.notify();
                        }))
                    }))
                    .child(
                        btn(t, ms, "st-command", self.tr("console.title"), Btn::Quiet).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.drawer = match this.drawer {
                                    Some(Drawer::Command) => None,
                                    _ => Some(Drawer::Command),
                                };
                                cx.notify();
                            }),
                        ),
                    ),
            )
    }
}

/// A field's colours, taken from the theme like everything else. The selection
/// is `Visual` — selected TEXT — which is the same role the grid uses for a
/// selected row and a different one from a selected list entry.
fn input_colors(t: &Theme) -> InputColors {
    InputColors {
        text: t.fg(),
        placeholder: t.muted(),
        cursor: t.accent(),
        selection: t.selection_bg(),
        background: t.base(),
        border: t.border(),
    }
}

/// The glyph for a driver, by the shape of what it is rather than by a list of
/// names: the engine says whether it is relational, and that is the only
/// distinction a one-character icon can honestly carry.
/// The glyph beside a tab's name.
///
/// ONE glyph for every driver, deliberately. This was a `match` whose three
/// arms returned the same codepoint — a function pretending to make a decision
/// it was not making, and the first thing anybody would do with it is add a
/// fourth arm and believe the other three. A per-driver icon needs icons that
/// exist in the icon font for every driver the engine can grow, including the
/// external ones, which is a promise this cannot keep; the DRIVER'S NAME is
/// already on the row next to it and says more than a logo would.
const DB_GLYPH: &str = "\u{f1c0}";

impl Workspace {
    /// The corner messages. They stack upward from the bottom right, newest
    /// nearest the corner, and they do not take clicks: nothing is hidden
    /// behind one for longer than it lasts.
    fn toasts_el(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.toasts.is_empty() {
            return None;
        }
        let t = self.t();
        let ms = self.scale;
        let inset = self.scale.toast_inset();
        let mut stack = div()
            .absolute()
            .right(px(inset))
            // ABOVE the status bar, not over it. They sat on the bottom edge
            // and covered the one line that says what the window is doing.
            .bottom(px(inset + ms.status_h()))
            .flex()
            .flex_col()
            .gap_1();
        for x in self.toasts.iter().rev().take(5) {
            let id = x.id;
            stack = stack.child(
                div()
                    .id(SharedString::from(format!("toast-{id}")))
                    .px_3()
                    .py_1()
                    // A width, so a long message WRAPS instead of running off
                    // the window: an engine's refusal is a sentence, and the
                    // half of it that mattered was the half off the screen.
                    .max_w(px(ms.base() * 36.0))
                    .cursor_pointer()
                    // A bad one carries the error colour in its background as
                    // well, faintly: red text on the panel is easy to miss
                    // when the eye is on the grid.
                    .bg(if x.bad {
                        Theme::mix(t.error(), t.panel(), 0.12)
                    } else {
                        t.panel()
                    })
                    .border_l_2()
                    .border_color(if x.bad { t.error() } else { t.accent() })
                    .text_size(ms.body())
                    .text_color(if x.bad { t.error() } else { t.fg() })
                    .child(SharedString::from(x.text.clone()))
                    // Dismissed by a click. A message worth reading is
                    // sometimes in the way of the thing it is about.
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toasts.retain(|y| y.id != id);
                        cx.notify();
                    })),
            );
        }
        Some(stack.into_any_element())
    }

    /// "It is still working" — the three answers, and what each one does.
    ///
    /// A card rather than a toast: a toast expires, and this is a question.
    /// Three buttons rather than two, because "cancel" is ambiguous here —
    /// cancel the disconnect, or cancel the query? Both are real answers, so
    /// both are offered by name.
    fn busy_el(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (name, busy) = self.confirm_disconnect.clone()?;
        let (t, ms) = (self.t(), self.scale);
        let title = self
            .lang
            .tr("busy.title")
            .replace("{name}", &name)
            .replace("{n}", &busy.len().to_string());
        let (a, b) = (name.clone(), name);
        Some(
            div()
                .absolute()
                .top(px(ms.palette_top()))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .occlude()
                        .w(px(ms.base() * 40.0))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .bg(t.raised())
                        .border_1()
                        .border_color(t.warning())
                        .child(div().text_size(ms.body()).text_color(t.fg()).child(title))
                        // WHICH statements, by the names their tabs carry: a
                        // count alone leaves somebody guessing whether it is
                        // the one they care about.
                        .children(busy.iter().take(5).map(|b| {
                            self.small(format!("\u{2022} {b}"), t.muted())
                        }))
                        .child(self.small(self.lang.tr("busy.note"), t.muted()))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .pt_1()
                                .child(
                                    btn(t, ms, "busy-wait", self.tr("busy.wait"), Btn::Secondary)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_disconnect = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    btn(t, ms, "busy-cancel", self.tr("busy.cancel"), Btn::Primary)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel_running_on(&a);
                                            this.disconnect_now(&a);
                                            this.confirm_disconnect = None;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    btn(t, ms, "busy-anyway", self.tr("busy.anyway"), Btn::Danger)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.disconnect_now(&b);
                                            this.confirm_disconnect = None;
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The context menu, where the pointer opened it.
    ///
    /// Drawn LAST and absolutely placed, like the palette — it is the same
    /// shape of thing, and two overlays with two sets of rules would drift. It
    /// `occlude`s, so a click on it never also reaches what is beneath.
    fn menu_el(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let open = self.menu.as_ref()?;
        let (t, ms) = (self.t(), self.scale);
        let items = open.items();
        let mut list = div()
            .flex()
            .flex_col()
            .w(px(ms.base() * 18.0))
            .bg(t.raised())
            .border_1()
            .border_color(t.divider())
            .py(px(2.0));
        for (n, item) in items.iter().enumerate() {
            let on = n == open.sel;
            let act = item.act.clone();
            let enabled = item.enabled;
            list = list.child(
                div()
                    .id(SharedString::from(format!("menu-{n}")))
                    .flex()
                    .items_center()
                    .h(px(ms.control_h()))
                    .px_2()
                    .text_size(ms.small())
                    // A line that cannot be used is DIMMED, not missing: a menu
                    // whose length changes with the state is a menu nobody
                    // learns the shape of.
                    .text_color(if !enabled {
                        t.disabled()
                    } else if on {
                        t.fg()
                    } else {
                        t.muted()
                    })
                    .when(on && enabled, |d| {
                        d.bg(t.selected_mark()).border_l_2().border_color(t.accent())
                    })
                    .when(!(on && enabled), |d| {
                        d.border_l_2().border_color(gpui::transparent_black())
                    })
                    .when(enabled, |d| d.cursor_pointer().hover(|h| h.bg(t.hover_mark())))
                    .child(self.tr(item.key))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if enabled {
                            this.menu = None;
                            this.run_menu(act.clone(), cx);
                        }
                    })),
            );
        }
        Some(
            div()
                .absolute()
                .left(px(open.at.0))
                .top(px(open.at.1))
                .occlude()
                .child(list)
                .into_any_element(),
        )
    }

    /// Open the menu for a target, where the pointer is.
    fn open_menu(
        &mut self,
        target: crate::menu::Target,
        at: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.menu = Some(crate::menu::Open::new(target, (f32::from(at.x), f32::from(at.y))));
        cx.notify();
    }

    /// Carry out one item. Every arm is something that already existed and had
    /// no way of being asked for.
    fn run_menu(&mut self, act: crate::menu::Act, cx: &mut Context<Self>) {
        use crate::menu::{Act, SavedKind};
        match act {
            Act::Connect(name) => self.connect(&name),
            Act::Disconnect(name) => self.disconnect(&name),
            Act::EditConnection(name) => {
                if let Some(c) = self.saved.iter().find(|c| c.name == name) {
                    let (n, d, sp) = (c.name.clone(), c.driver.clone(), c.spec.clone());
                    self.open_form(Some((n, d, sp)), cx);
                }
            }
            Act::RemoveConnection(name) => {
                self.confirm_conn_rm = Some((name.clone(), None));
                self.ask(
                    Pending::ConnQueryCount { name: name.clone() },
                    "store.queries.list",
                    json!({ "conn": name }),
                );
            }
            Act::MoveToFolder(name) => {
                // The naming row again, with the folder it is in now as the
                // suggestion — moving a connection between two folders is
                // mostly typing the other one's name.
                let now = self
                    .saved
                    .iter()
                    .find(|c| c.name == name)
                    .and_then(|c| c.group.clone())
                    .unwrap_or_default();
                self.naming = Some(NameFor::Folder(name));
                self.save_name.update(cx, |i, cx| i.set_text(now, cx));
            }
            Act::RefreshConnection(name) => {
                if let Some(l) = self.live(&name) {
                    let conn_id = l.conn_id;
                    self.ask(
                        Pending::Structure { name: name.clone() },
                        "schema.structure",
                        json!({ "conn_id": conn_id }),
                    );
                }
            }
            Act::CopyText(text) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                self.toast("copied".to_string(), false);
            }
            Act::OpenNode { conn, name, schema, kind } => {
                self.open_object(&conn, &name, schema.as_deref(), &kind, cx)
            }
            Act::InsideNode { conn, name, schema, kind } => {
                let key = format!("{conn}/{kind}/{name}");
                self.toggle_inside(&key, &conn, &name, schema.as_deref(), &kind);
                cx.notify();
            }
            Act::CopyDdl { conn, name, schema } => {
                // The DDL is the ENGINE's answer, not a string this client can
                // assemble: it is asked for, and the reply goes to the
                // clipboard — see `Pending::DdlCopy`.
                if let Some(l) = self.live(&conn) {
                    let conn_id = l.conn_id;
                    let objref = match &schema {
                        Some(s) => json!({ "name": name, "schema": s }),
                        None => json!({ "name": name }),
                    };
                    self.ask(
                        Pending::DdlCopy,
                        "schema.ddl",
                        json!({ "conn_id": conn_id, "object": objref }),
                    );
                } else {
                    self.toast(format!("{conn} is not connected"), true);
                }
            }
            Act::InsertIntoEditor(name) => self.insert_into_editor(&name, cx),
            Act::CopyRowTsv { tab, row } => self.copy_row_tsv(tab, row, cx),
            // The two SQL forms work off the row in front, which is why the
            // menu selects it first.
            Act::CopyRowSelect { tab, row } => {
                self.select_row(tab, row, cx);
                self.copy_row(false, cx);
            }
            Act::CopyRowInsert { tab, row } => {
                self.select_row(tab, row, cx);
                self.copy_row(true, cx);
            }
            Act::ShowRow { tab, row } => {
                if let Some(tb) = self.tabs.iter_mut().find(|t| t.id == tab) {
                    tb.selected = Some(row);
                    tb.detail = Some(row);
                }
                cx.notify();
            }
            Act::EditRow { tab, row } => {
                self.select_row(tab, row, cx);
                // The first editable column of that row: the menu says "edit
                // this row", and which cell it lands in is this client's to
                // choose.
                let col = self
                    .tabs
                    .iter()
                    .find(|t| t.id == tab)
                    .and_then(|t| {
                        (0..t.columns.len()).find(|c| crate::workspace::column_editable(t, *c))
                    });
                if let Some(col) = col {
                    self.begin_edit(tab, row, col, cx);
                }
            }
            Act::DeleteRow { tab, row } => self.propose_write(tab, row, Vec::new(), true, cx),
            Act::CloseTab(id) => self.close_tab(id),
            Act::CloseOtherTabs(id) => {
                let others: Vec<u64> =
                    self.tabs.iter().map(|t| t.id).filter(|x| *x != id).collect();
                for o in others {
                    self.close_tab(o);
                }
                cx.notify();
            }
            Act::DuplicateTab(id) => self.run_in_new_tab(id, cx),
            Act::OpenSaved { conn, name, kind } => match kind {
                SavedKind::Query => {
                    let sql = self
                        .live(&conn)
                        .and_then(|l| l.queries.iter().find(|q| q.name == name))
                        .map(|q| q.sql.clone());
                    if let Some(sql) = sql {
                        self.open_sql(&conn, &name, &sql, cx);
                    }
                }
                SavedKind::Chart => {
                    let sql = self
                        .live(&conn)
                        .and_then(|l| l.charts.iter().find(|c| c.name == name))
                        .map(|c| c.sql.clone());
                    if let Some(sql) = sql {
                        self.open_sql(&conn, &name, &sql, cx);
                    }
                }
                SavedKind::Board => self.open_board(&conn, &name, cx),
            },
            Act::RemoveSaved { conn, name, kind } => {
                let (method, pend) = match kind {
                    SavedKind::Query => (
                        "store.queries.remove",
                        Pending::SavedQueries { conn: conn.clone() },
                    ),
                    SavedKind::Chart => {
                        ("store.charts.remove", Pending::SavedCharts { conn: conn.clone() })
                    }
                    SavedKind::Board => {
                        ("store.dashboards.remove", Pending::Dashboards { conn: conn.clone() })
                    }
                };
                self.ask(pend, method, json!({ "conn": conn, "name": name }));
                self.toast(format!("removed: {name}"), false);
            }
        }
        cx.notify();
    }

    /// The command palette: one box, and everything this window can do ranked
    /// against what has been typed. ctrl-p opens it, escape leaves it,
    /// ctrl-n/ctrl-k move, enter or ctrl-y takes the one highlighted.
    fn palette_el(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.palette_open {
            return None;
        }
        let t = self.t();
        let ms = self.scale;
        let matches = self.palette_matches(cx);
        let sel = self.palette_sel.min(matches.len().saturating_sub(1));

        let mut list = div().flex().flex_col().max_h(px(ms.palette_h())).overflow_hidden();
        if matches.is_empty() {
            list = list.child(div().px_3().py_1().child(self.small(self.lang.tr("panel.nothing_matches"), t.muted())));
        }
        // The window SLIDES to the selection — see `palette_window`. It drew the
        // first fourteen, so ctrl-n fifteen times moved a highlight nobody
        // could see and enter took a command that was never on screen.
        let shown = crate::workspace::palette_window(sel, matches.len(), 14);
        let hint_w = ms.base() * 11.0;
        for (n, (idx, it)) in matches.iter().enumerate() {
            if !shown.contains(&n) {
                continue;
            }
            let on = n == sel;
            let act = it.act.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("pal-{idx}")))
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py(px(3.0))
                    .cursor_pointer()
                    .when(on, |d| d.bg(t.selected_mark()).border_l_2().border_color(t.accent()))
                    .when(!on, |d| d.border_l_2().border_color(t.panel()))
                    .hover(|d| d.bg(t.hover_mark()))
                    .child(
                        // Both sides CUT at a real width. A long label pushed
                        // the shortcut off the right edge of the palette, so
                        // the one thing the row exists to teach was the first
                        // thing to go.
                        self.line_in(
                            it.label.clone(),
                            ms.body(),
                            t.fg(),
                            ms.base() * 48.5 - hint_w - ms.base() * 2.0,
                        ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(hint_w))
                            .truncate()
                            .text_size(ms.small())
                            .text_color(t.muted())
                            .child(SharedString::from(it.hint.clone())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.palette_run(act.clone(), cx);
                        cx.notify();
                    })),
            );
        }

        Some(
            div()
                .absolute()
                .top(px(ms.palette_top()))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .key_context("Palette")
                        .w(px(ms.base() * 48.5))
                        .bg(t.panel())
                        .border_1()
                        .border_color(t.accent())
                        .flex()
                        .flex_col()
                        .child(div().px_2().py_1().child(self.field(ms.body(), self.palette_input.clone())))
                        .child(div().h(px(1.0)).w_full().bg(t.border()))
                        .child(list)
                        .child(
                            div().px_3().py(px(2.0)).child(self.small(
                                format!(
                                    "{} of {} \u{2014} ctrl-n / ctrl-k move, ctrl-y takes, esc leaves",
                                    // WHICH of them, not how many are drawn: "14
                                    // of 60" said the same thing at the top of
                                    // the list and at the bottom.
                                    sel + 1,
                                    matches.len()
                                ),
                                t.muted(),
                            )),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // How wide the dashboard canvas is, for the drag arithmetic. DERIVED
        // from the viewport rather than measured: GPUI lays out after the
        // element tree is built, so a width measured this frame would be last
        // frame's — and this is the same subtraction the layout below performs,
        // so the two cannot disagree about anything but a resize in flight.
        // rail + panel + the canvas's own padding.
        self.canvas_w = (f32::from(window.viewport_size().width) - self.grid_left() - 8.0).max(120.0);

        // Toasts expire on the next frame they are late for; the pump redraws
        // often enough that nothing sits there after its time.
        let now = Instant::now();
        self.toasts.retain(|x| !x.expired(now));

        // Somebody asked for the keyboard back from a place with no `Window` —
        // the editor's escape. Here is the first moment there is one.
        if std::mem::take(&mut self.focus_back) {
            window.focus(&self.focus);
        }

        let t = self.t();

        let ms = self.scale;
        let root = div()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.move_row(20, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.move_row(-20, cx)))
            .on_action(cx.listener(|this, _: &FirstRow, _, cx| this.move_row(isize::MIN, cx)))
            .on_action(cx.listener(|this, _: &LastRow, _, cx| this.move_row(isize::MAX, cx)))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.step_tab(1, cx)))
            // A column's width at a time, so a press moves something visible.
            .on_action(cx.listener(|this, _: &ScrollLeft, _, cx| {
                this.scroll_grid(this.scale.col_w());
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ScrollRight, _, cx| {
                this.scroll_grid(-this.scale.col_w());
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevTab, _, cx| this.step_tab(-1, cx)))
            .on_action(cx.listener(|this, _: &RunActive, _, cx| {
                if let Some(id) = this.tabs.get(this.active).map(|x| x.id) {
                    this.sync_tab_from_inputs(id, cx);
                    this.run_tab(id, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleDetail, _, cx| {
                // Enter TAKES the menu's highlighted line when one is open; it
                // opens the row's detail otherwise.
                if let Some(act) = this.menu.as_ref().and_then(|m| m.chosen()) {
                    this.menu = None;
                    this.run_menu(act, cx);
                    return;
                }
                if let Some(tab) = this.tabs.get_mut(this.active) {
                    tab.detail = match tab.detail {
                        Some(_) => None,
                        None => tab.selected.or(Some(0)),
                    };
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                this.palette_open = !this.palette_open;
                this.palette_sel = 0;
                if this.palette_open {
                    this.palette_input.update(cx, |i, cx| i.set_text(String::new(), cx));
                    let h = this.palette_input.read(cx).focus_handle.clone();
                    window.focus(&h);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextRow, _, cx| {
                // In a menu, j and the down arrow move the MENU. Out of one
                // they move the grid, as before.
                if let Some(m) = this.menu.as_mut() {
                    m.step(1);
                    cx.notify();
                } else {
                    this.move_row(1, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &PrevRow, _, cx| {
                if let Some(m) = this.menu.as_mut() {
                    m.step(-1);
                    cx.notify();
                } else {
                    this.move_row(-1, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                // Escape leaves whatever is in front, innermost first.
                if this.menu.is_some() {
                    this.menu = None;
                } else if this.confirm_disconnect.is_some() {
                    // Escape on a question means the answer that CHANGES
                    // NOTHING, which here is to keep waiting.
                    this.confirm_disconnect = None;
                } else if this.palette_open {
                    this.palette_open = false;
                } else if this.export_open {
                    this.export_open = false;
                } else if this.tabs.get(this.active).map_or(false, |x| x.detail.is_some()) {
                    if let Some(tab) = this.tabs.get_mut(this.active) {
                        tab.detail = None;
                    }
                } else {
                    // Nothing in front: take focus back from the text field, so
                    // the keys work again.
                    window.focus(&this.focus);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PaletteDown, _, cx| {
                this.palette_sel = this.palette_sel.saturating_add(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PaletteUp, _, cx| {
                this.palette_sel = this.palette_sel.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextCol, _, cx| this.step_col(1, cx)))
            .on_action(cx.listener(|this, _: &PrevCol, _, cx| this.step_col(-1, cx)))
            .on_action(cx.listener(|this, _: &CopyCell, _, cx| this.copy_cell(cx)))
            .on_action(cx.listener(|this, _: &CopyRowTsv, _, cx| {
                let what = this
                    .tabs
                    .get(this.active)
                    .and_then(|t| t.selected.or(t.detail).map(|r| (t.id, r)));
                match what {
                    Some((id, row)) => this.copy_row_tsv(id, row, cx),
                    None => this.toast("pick a row first".to_string(), true),
                }
            }))
            .on_action(cx.listener(|this, _: &CopyPageTsv, _, cx| this.copy_page_tsv(cx)))
            .on_action(cx.listener(|this, _: &NewTab, _, cx| this.new_draft_tab(cx)))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| {
                if let Some(t) = this.tabs.get(this.active) {
                    let id = t.id;
                    this.close_tab(id);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &LastTab, _, cx| {
                let alive: Vec<u64> = this.tabs.iter().map(|t| t.id).collect();
                let now = this.tabs.get(this.active).map(|t| t.id).unwrap_or(0);
                if let Some(to) = crate::workspace::mru_next(&this.mru, now, &alive) {
                    this.go_tab_id(to, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &TogglePanel, _, cx| {
                this.panel_hidden = !this.panel_hidden;
                this.remember();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.open_settings(cx)))
            .on_action(cx.listener(|this, _: &NewConnection, _, cx| this.open_form(None, cx)))
            .on_action(cx.listener(|this, _: &ToggleExport, _, cx| {
                this.export_open = !this.export_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevPage, _, cx| {
                this.page_by(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextPage, _, cx| {
                this.page_by(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowAudit, _, cx| this.go_rail(Rail::Audit, cx)))
            .on_action(cx.listener(|this, _: &ShowTasks, _, cx| this.go_rail(Rail::Tasks, cx)))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| {
                if let Some(t) = this.tabs.get(this.active) {
                    let id = t.id;
                    this.sync_tab_from_inputs(id, cx);
                    this.run_tab(id, cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &GoTab1, _, cx| this.go_tab_at(0, cx)))
            .on_action(cx.listener(|this, _: &GoTab2, _, cx| this.go_tab_at(1, cx)))
            .on_action(cx.listener(|this, _: &GoTab3, _, cx| this.go_tab_at(2, cx)))
            .on_action(cx.listener(|this, _: &GoTab4, _, cx| this.go_tab_at(3, cx)))
            .on_action(cx.listener(|this, _: &GoTab5, _, cx| this.go_tab_at(4, cx)))
            .on_action(cx.listener(|this, _: &GoTab6, _, cx| this.go_tab_at(5, cx)))
            .on_action(cx.listener(|this, _: &GoTab7, _, cx| this.go_tab_at(6, cx)))
            .on_action(cx.listener(|this, _: &GoTab8, _, cx| this.go_tab_at(7, cx)))
            .on_action(cx.listener(|this, _: &GoTab9, _, cx| this.go_tab_at(8, cx)))
            .on_action(cx.listener(|this, _: &PaletteAccept, _, cx| {
                let pick = this.palette_matches(cx).into_iter().nth(this.palette_sel);
                if let Some((_, it)) = pick {
                    this.palette_run(it.act, cx);
                }
                cx.notify();
            }))
            // A column being dragged is tracked at the root, because the mouse
            // leaves the narrow handle long before the drag is over.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if this.dragging.is_some() {
                    this.board_drag_to((f32::from(ev.position.x), f32::from(ev.position.y)));
                    cx.notify();
                }
                if let Some((from, was)) = this.panel_drag {
                    let w = was + (f32::from(ev.position.x) - from);
                    this.scale = this.scale.with_panel(Some(w));
                    cx.notify();
                }
                if let Some((tab_id, col, from, was)) = this.resizing {
                    let w = (was + (f32::from(ev.position.x) - from)).clamp(40.0, 900.0);
                    if let Some(tab) = this.tabs.iter_mut().find(|x| x.id == tab_id) {
                        tab.widths.insert(col, w);
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if let Some(tab) = this.dragging.take() {
                        if let Some(b) = this.board_mut(tab) {
                            b.drag = None;
                        }
                        cx.notify();
                    }
                    if this.resizing.take().is_some() || this.panel_drag.take().is_some() {
                        this.remember();
                        cx.notify();
                    }
                }),
            )
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
            .font_family(SharedString::from(self.font_family.clone()))
            .text_size(ms.heading());

        if let Some(fatal) = self.fatal.clone() {
            return root.child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(div().text_color(t.error()).child("No engine"))
                    .child(div().text_color(t.fg()).child(SharedString::from(fatal)))
                    .child(self.small(
                        "Build it with scripts/build and put loded on $PATH, or set $LODE_DAEMON.",
                        t.muted(),
                    )),
            );
        }

        root.child(
            div()
                .flex()
                .flex_1()
                .w_full()
                .overflow_hidden()
                .child(self.rail_el(cx))
                // FOLDED AWAY, with its grip, when the room is wanted for the
                // result. The rail stays: it is how the panel comes back.
                .children((!self.panel_hidden).then(|| self.panel_el(cx)))
                // The handle between the panel and the work. Its own strip, so
                // a drag never lands on whatever is beneath it; the move and
                // the release are on the ROOT, because the pointer leaves a
                // three-pixel strip immediately and a drag that ended there
                // would be a drag nobody could finish.
                .children((!self.panel_hidden).then(|| {
                    div()
                        .id("panel-grip")
                        .w(px(3.0))
                        .h_full()
                        .flex_none()
                        .bg(t.divider())
                        .hover(|d| d.bg(t.accent()))
                        .cursor_col_resize()
                        .hover(|d| d.bg(t.accent()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, ev: &gpui::MouseDownEvent, _, cx| {
                                // Two clicks puts it back to the width the type
                                // scale gives. A width somebody dragged is a
                                // decision, and this is how the decision is
                                // taken back without hunting for a reset.
                                if ev.click_count >= 2 {
                                    this.scale = this.scale.with_panel(None);
                                    this.panel_drag = None;
                                    this.remember();
                                } else {
                                    this.panel_drag =
                                        Some((f32::from(ev.position.x), this.scale.panel_w()));
                                }
                                cx.notify();
                            }),
                        )
                }))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .h_full()
                        .child(self.tabs_el(cx))
                        .child(div().flex_1().overflow_hidden().child(self.result_el(cx))),
                ),
        )
        .child(self.bottom_el(cx))
        .children(self.busy_el(cx))
        .children(self.toasts_el(cx))
        // A click ANYWHERE ELSE closes the menu. The menu itself `occlude`s, so
        // this never fires for a click on one of its own lines — without that,
        // choosing an item would close the menu before the item ran.
        .when(self.menu.is_some(), |d| {
            d.on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.menu = None;
                    cx.notify();
                }),
            )
        })
        .children(self.menu_el(cx))
        .children(self.palette_el(cx))
    }
}
