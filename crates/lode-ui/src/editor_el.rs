//! The multi-line editor: the entity, the element, the keys.
//!
//! The MODEL is in `editor.rs` and is tested without a screen. This file is the
//! part that cannot be: a gpui `Element` that lays itself out, shapes the lines
//! that are visible, paints the selection, the caret and the text, and turns
//! key presses and clicks into calls on the model.
//!
//! It follows `input.rs`'s single-line element closely on purpose. That one is
//! gpui's own example, worked over for this client; every awkward thing in it —
//! the input handler installed during paint, offsets translated into the DRAWN
//! text, the caret painted only when focused — is awkward for a reason, and the
//! reasons do not change by there being more than one line.
//!
//! WHAT IT DOES NOT DO, said plainly rather than left to be discovered:
//!
//!   * It does not WRAP. A long line scrolls sideways. A SQL statement that
//!     rewraps at the window's edge stops lining up, and lining up is what
//!     reading an indented statement depends on.
//!   * Its height is DERIVED from the content between three and sixteen lines,
//!     not dragged. A box that is one line is useless and one that is half the
//!     window leaves no result; a box that follows what is in it needs neither
//!     decision from anybody.
//!   * Input-method composition is handled on the CURRENT LINE only. The handler
//!     is given that line's text and offsets within it, which is what every
//!     IME needs to place its candidate window; a composition spanning a line
//!     break is not a thing that happens.

use gpui::prelude::*;
use gpui::{
    actions, fill, point, px, relative, size, App, Bounds, ClipboardItem, Context, Element,
    ElementId, ElementInputHandler, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window,
};

use crate::editor::{sql_runs, Doc, Pos, SqlRole};
use crate::input::{InputColors, InputEvent};
use crate::metrics::Metrics;
use crate::theme::Theme;

actions!(
    lode_editor,
    [
        Up,
        Down,
        SelectUp,
        SelectDown,
        PageUp,
        PageDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DocStart,
        DocEnd,
        SelectHome,
        SelectEnd,
        Newline,
        Tab,
        Untab,
        Undo,
        Redo,
        DuplicateLine,
        ToggleComment,
        Run,
        RunNewTab,
    ]
);

/// Bind the editor's keys. Called once, at start.
///
/// CONTEXT `Editor`, so none of these reach the grid's single letters and the
/// grid's `j`/`k`/`r` do not reach a statement being typed. The shared ones —
/// left, right, backspace, home, copy — are `lode_input`'s actions, bound
/// already and handled here as well: one action type, two handlers, rather than
/// two names for one keystroke.
pub fn bind_editor_keys(cx: &mut App) {
    use gpui::KeyBinding;
    const IN: Option<&str> = Some("Editor");
    cx.bind_keys([
        KeyBinding::new("up", Up, IN),
        KeyBinding::new("down", Down, IN),
        KeyBinding::new("shift-up", SelectUp, IN),
        KeyBinding::new("shift-down", SelectDown, IN),
        KeyBinding::new("pageup", PageUp, IN),
        KeyBinding::new("pagedown", PageDown, IN),
        KeyBinding::new("ctrl-left", WordLeft, IN),
        KeyBinding::new("ctrl-right", WordRight, IN),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, IN),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, IN),
        KeyBinding::new("ctrl-home", DocStart, IN),
        KeyBinding::new("ctrl-end", DocEnd, IN),
        KeyBinding::new("shift-home", SelectHome, IN),
        KeyBinding::new("shift-end", SelectEnd, IN),
        KeyBinding::new("tab", Tab, IN),
        KeyBinding::new("shift-tab", Untab, IN),
        KeyBinding::new("ctrl-z", Undo, IN),
        KeyBinding::new("ctrl-shift-z", Redo, IN),
        KeyBinding::new("ctrl-y", Redo, IN),
        KeyBinding::new("ctrl-d", DuplicateLine, IN),
        KeyBinding::new("ctrl-/", ToggleComment, IN),
        // Enter makes a line; ctrl-enter RUNS. The other way round would mean a
        // half-typed statement reaching the database on a reflex.
        KeyBinding::new("enter", Newline, IN),
        KeyBinding::new("ctrl-enter", Run, IN),
        KeyBinding::new("ctrl-shift-enter", RunNewTab, IN),
    ]);
}

/// What the editor tells its owner.
pub enum EditorEvent {
    /// The text changed — the draft is kept in the tab, and the session keeps
    /// the draft.
    Changed,
    /// Ctrl-Enter: run what is in here.
    Run,
    /// Ctrl-Shift-Enter: run it in a tab of its own.
    RunNewTab,
}

/// Which language the highlighter should read the text as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    Sql,
    /// A document store's command: MongoDB's filter, Redis's line. JSON-ish.
    Json,
    /// No highlighting at all — a driver whose language this client has never
    /// been told about. One ink is honest; a SQL palette over something that is
    /// not SQL is a lie that reads as a bug.
    Plain,
}

pub struct Editor {
    pub doc: Doc,
    pub focus_handle: FocusHandle,
    colors: InputColors,
    theme: Theme,
    scale: Metrics,
    flavour: Flavour,
    font: SharedString,
    /// The first visible line.
    scroll_top: usize,
    /// Set during paint, for hit-testing a click.
    last_lines: Vec<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    last_rows: usize,
    selecting: bool,
    /// The range an input method is composing, within the CURRENT LINE.
    marked: Option<std::ops::Range<usize>>,
}

/// How tall the box is, in lines.
///
/// Derived from the content and clamped. A statement box of one line is what
/// this replaces; one as tall as its longest statement would leave no room for
/// the result the statement is for.
pub const MIN_ROWS: usize = 3;
pub const MAX_ROWS: usize = 16;

/// Where the window has to scroll to so the cursor is in it.
///
/// A pure function because it is the thing that goes wrong: a cursor moved
/// off-screen and a window that does not follow it is an editor typing into
/// nowhere, and it is invisible in any screenshot taken with a short statement.
pub fn scroll_to_show(top: usize, cursor_line: usize, rows: usize, lines: usize) -> usize {
    if rows == 0 {
        return 0;
    }
    // Never past the last page: a window below the text shows blank where the
    // text is.
    let highest = lines.saturating_sub(rows);
    let mut top = top.min(highest);
    if cursor_line < top {
        top = cursor_line;
    } else if cursor_line >= top + rows {
        top = cursor_line + 1 - rows;
    }
    top.min(highest)
}

impl Editor {
    pub fn new(
        text: &str,
        flavour: Flavour,
        colors: InputColors,
        theme: Theme,
        scale: Metrics,
        font: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        Editor {
            doc: Doc::new(text),
            focus_handle: cx.focus_handle(),
            colors,
            theme,
            scale,
            flavour,
            font: font.into(),
            scroll_top: 0,
            last_lines: Vec::new(),
            last_bounds: None,
            last_rows: MIN_ROWS,
            selecting: false,
            marked: None,
        }
    }

    pub fn text(&self) -> String {
        self.doc.text()
    }

    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.doc.set_text(text);
        self.scroll_top = 0;
        cx.notify();
    }

    pub fn set_colors(&mut self, colors: InputColors, theme: Theme, cx: &mut Context<Self>) {
        self.colors = colors;
        self.theme = theme;
        cx.notify();
    }

    pub fn set_scale(&mut self, scale: Metrics, cx: &mut Context<Self>) {
        self.scale = scale;
        cx.notify();
    }

    pub fn set_flavour(&mut self, flavour: Flavour, cx: &mut Context<Self>) {
        self.flavour = flavour;
        cx.notify();
    }

    /// What `run` should send: the SELECTION if there is one, the whole text
    /// otherwise.
    ///
    /// Running a selection is how a file of several statements is used at all,
    /// and it is the one feature of a query editor that cannot be worked around
    /// by retyping.
    pub fn to_run(&self) -> String {
        self.doc.selected_text().unwrap_or_else(|| self.doc.text())
    }

    pub fn rows(&self) -> usize {
        self.doc.line_count().clamp(MIN_ROWS, MAX_ROWS)
    }

    fn after_change(&mut self, cx: &mut Context<Self>) {
        self.follow_cursor();
        self.marked = None;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn after_move(&mut self, cx: &mut Context<Self>) {
        self.follow_cursor();
        cx.notify();
    }

    fn follow_cursor(&mut self) {
        self.scroll_top = scroll_to_show(
            self.scroll_top,
            self.doc.cursor().line,
            self.rows(),
            self.doc.line_count(),
        );
    }

    // ── the keys ────────────────────────────────────────────────────────────

    fn on_up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_up(false);
        self.after_move(cx);
    }
    fn on_down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_down(false);
        self.after_move(cx);
    }
    fn on_select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_up(true);
        self.after_move(cx);
    }
    fn on_select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_down(true);
        self.after_move(cx);
    }
    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows() as isize;
        self.doc.move_page(-rows, false);
        self.after_move(cx);
    }
    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let rows = self.rows() as isize;
        self.doc.move_page(rows, false);
        self.after_move(cx);
    }
    fn on_word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_word_left(false);
        self.after_move(cx);
    }
    fn on_word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_word_right(false);
        self.after_move(cx);
    }
    fn on_select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_word_left(true);
        self.after_move(cx);
    }
    fn on_select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_word_right(true);
        self.after_move(cx);
    }
    fn on_doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_doc_start(false);
        self.after_move(cx);
    }
    fn on_doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_doc_end(false);
        self.after_move(cx);
    }
    fn on_select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_home(true);
        self.after_move(cx);
    }
    fn on_select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_end(true);
        self.after_move(cx);
    }
    fn on_newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.newline();
        self.after_change(cx);
    }
    fn on_tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.tab();
        self.after_change(cx);
    }
    fn on_untab(&mut self, _: &Untab, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.untab();
        self.after_change(cx);
    }
    fn on_undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if self.doc.undo() {
            self.after_change(cx);
        }
    }
    fn on_redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if self.doc.redo() {
            self.after_change(cx);
        }
    }
    fn on_duplicate(&mut self, _: &DuplicateLine, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.duplicate_line();
        self.after_change(cx);
    }
    fn on_toggle_comment(&mut self, _: &ToggleComment, _: &mut Window, cx: &mut Context<Self>) {
        // The comment marker is the LANGUAGE's, not this file's opinion.
        let prefix = match self.flavour {
            Flavour::Sql => "--",
            Flavour::Json | Flavour::Plain => "//",
        };
        self.doc.toggle_comment(prefix);
        self.after_change(cx);
    }
    fn on_run(&mut self, _: &Run, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::Run);
    }
    fn on_run_new_tab(&mut self, _: &RunNewTab, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(EditorEvent::RunNewTab);
    }

    // The shared ones, by `lode_input`'s action types.
    fn on_left(&mut self, _: &crate::input::Left, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_left(false);
        self.after_move(cx);
    }
    fn on_right(&mut self, _: &crate::input::Right, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_right(false);
        self.after_move(cx);
    }
    fn on_select_left(&mut self, _: &crate::input::SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_left(true);
        self.after_move(cx);
    }
    fn on_select_right(&mut self, _: &crate::input::SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_right(true);
        self.after_move(cx);
    }
    fn on_home(&mut self, _: &crate::input::Home, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_home(false);
        self.after_move(cx);
    }
    fn on_end(&mut self, _: &crate::input::End, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.move_end(false);
        self.after_move(cx);
    }
    fn on_select_all(&mut self, _: &crate::input::SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.select_all();
        cx.notify();
    }
    fn on_backspace(&mut self, _: &crate::input::Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.backspace();
        self.after_change(cx);
    }
    fn on_delete(&mut self, _: &crate::input::Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.doc.delete();
        self.after_change(cx);
    }
    fn on_copy(&mut self, _: &crate::input::Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.doc.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
    fn on_cut(&mut self, _: &crate::input::Cut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.doc.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.doc.delete_selection();
            self.after_change(cx);
        }
    }
    fn on_paste(&mut self, _: &crate::input::Paste, _: &mut Window, cx: &mut Context<Self>) {
        // The WHOLE text, newlines and all. This is the box those newlines were
        // being thrown away in.
        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
            self.doc.insert(&text);
            self.after_change(cx);
        }
    }

    // ── the mouse ───────────────────────────────────────────────────────────

    /// Which position a point in the element is over.
    fn pos_for_point(&self, p: Point<Pixels>) -> Pos {
        let Some(bounds) = self.last_bounds else { return self.doc.cursor() };
        let line_h = f32::from(self.scale.line_h()).max(1.0);
        let dy = (f32::from(p.y) - f32::from(bounds.top())).max(0.0);
        let row = (dy / line_h) as usize;
        let line = (self.scroll_top + row).min(self.doc.line_count() - 1);
        let x = p.x - bounds.left() - px(self.gutter_w());
        let byte = match self.last_lines.get(row) {
            // `closest_index_for_x` rather than `index_for_x`: a click past the
            // end of a line has no index, and the answer somebody means by it
            // is the end of that line.
            Some(shaped) => shaped.closest_index_for_x(x),
            None => 0,
        };
        self.doc.clamp(Pos::new(line, byte))
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let p = self.pos_for_point(ev.position);
        // Shift-click extends, as it does everywhere.
        self.doc.place(p, ev.modifiers.shift);
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let p = self.pos_for_point(ev.position);
        self.doc.place(p, true);
        cx.notify();
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        cx.notify();
    }

    /// The width of the line-number column.
    ///
    /// Wide enough for the LAST line's number, so the text does not shift
    /// sideways as a statement grows past nine lines.
    fn gutter_w(&self) -> f32 {
        let digits = self.doc.line_count().to_string().len().max(2) as f32;
        (f32::from(self.scale.small()) * 0.62 * digits + self.scale.cell_pad() * 2.0).round()
    }

    fn runs_for(&self, line: &str) -> Vec<TextRun> {
        let font = gpui::Font {
            family: self.font.clone(),
            features: Default::default(),
            fallbacks: None,
            weight: Default::default(),
            style: Default::default(),
        };
        let one = |len: usize, color: gpui::Rgba| TextRun {
            len,
            font: font.clone(),
            color: color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        if line.is_empty() {
            return Vec::new();
        }
        match self.flavour {
            Flavour::Plain => vec![one(line.len(), self.colors.text)],
            Flavour::Sql => sql_runs(line)
                .into_iter()
                .map(|(len, role)| one(len, self.theme.sql_role(role)))
                .collect(),
            Flavour::Json => crate::workspace::json_runs(line)
                .into_iter()
                .map(|(len, role)| one(len, self.theme.json_role(role)))
                .collect(),
        }
    }
}

impl gpui::EventEmitter<EditorEvent> for Editor {}
impl gpui::EventEmitter<InputEvent> for Editor {}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui::div()
            .flex()
            .key_context("Editor")
            .track_focus(&self.focus_handle(cx))
            .cursor(gpui::CursorStyle::IBeam)
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_select_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_word_left))
            .on_action(cx.listener(Self::on_word_right))
            .on_action(cx.listener(Self::on_select_word_left))
            .on_action(cx.listener(Self::on_select_word_right))
            .on_action(cx.listener(Self::on_doc_start))
            .on_action(cx.listener(Self::on_doc_end))
            .on_action(cx.listener(Self::on_select_home))
            .on_action(cx.listener(Self::on_select_end))
            .on_action(cx.listener(Self::on_newline))
            .on_action(cx.listener(Self::on_tab))
            .on_action(cx.listener(Self::on_untab))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_duplicate))
            .on_action(cx.listener(Self::on_toggle_comment))
            .on_action(cx.listener(Self::on_run))
            .on_action(cx.listener(Self::on_run_new_tab))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_select_left))
            .on_action(cx.listener(Self::on_select_right))
            .on_action(cx.listener(Self::on_home))
            .on_action(cx.listener(Self::on_end))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_paste))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .size_full()
            .child(EditorElement { editor: cx.entity() })
    }
}

/// The element: the only part that cannot be tested without a screen.
struct EditorElement {
    editor: gpui::Entity<Editor>,
}

struct Prepaint {
    lines: Vec<ShapedLine>,
    numbers: Vec<ShapedLine>,
    gutter: f32,
    quads: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let ed = self.editor.read(cx);
        let rows = ed.rows();
        let line_h = f32::from(ed.scale.line_h());
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = px(line_h * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let ed = self.editor.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_h = px(f32::from(ed.scale.line_h()));
        let rows = ed.rows();
        let top = ed.scroll_top;
        let gutter = ed.gutter_w();
        let cursor = ed.doc.cursor();
        let selection = ed.doc.selection();
        let number_font = gpui::Font {
            family: ed.font.clone(),
            features: Default::default(),
            fallbacks: None,
            weight: Default::default(),
            style: Default::default(),
        };

        let mut lines = Vec::new();
        let mut numbers = Vec::new();
        let mut quads = Vec::new();
        let mut caret = None;

        for row in 0..rows {
            let n = top + row;
            let Some(text) = ed.doc.lines().get(n) else { break };
            let runs = ed.runs_for(text);
            let shaped = window.text_system().shape_line(
                SharedString::from(text.clone()),
                font_size,
                &runs,
                None,
            );
            let y = bounds.top() + line_h * row as f32;

            // The SELECTION, per line: the part of this line that is inside it.
            if let Some((from, to)) = selection {
                if n >= from.line && n <= to.line {
                    let start = if n == from.line { from.byte } else { 0 };
                    let end = if n == to.line { to.byte } else { text.len() };
                    let x0 = bounds.left() + px(gutter) + shaped.x_for_index(start);
                    let x1 = bounds.left() + px(gutter) + shaped.x_for_index(end);
                    // A line selected whole but EMPTY still shows: a zero-width
                    // mark says nothing, and the line break is part of what was
                    // selected.
                    let x1 = if end == start && n != to.line { x0 + font_size * 0.5 } else { x1 };
                    quads.push(fill(
                        Bounds::from_corners(point(x0, y), point(x1, y + line_h)),
                        ed.colors.selection,
                    ));
                }
            }

            if n == cursor.line {
                let x = bounds.left() + px(gutter) + shaped.x_for_index(cursor.byte);
                caret = Some(fill(
                    Bounds::new(point(x, y), size(px(1.5), line_h)),
                    ed.colors.cursor,
                ));
            }

            // The number, in its own shaped line so it can be painted in the
            // gutter at the same baseline as its text.
            let label = SharedString::from((n + 1).to_string());
            let num_run = TextRun {
                len: label.len(),
                font: number_font.clone(),
                color: ed.theme.row_number().into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            numbers.push(window.text_system().shape_line(
                label,
                font_size,
                &[num_run],
                None,
            ));
            lines.push(shaped);
        }

        Prepaint { lines, numbers, gutter, quads, cursor: caret }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.editor.read(cx).focus_handle.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.editor.clone()), cx);

        let line_h = px(f32::from(self.editor.read(cx).scale.line_h()));
        for quad in prepaint.quads.drain(..) {
            window.paint_quad(quad);
        }
        let gutter = px(prepaint.gutter);
        for (row, line) in prepaint.lines.iter().enumerate() {
            let y = bounds.top() + line_h * row as f32;
            let _ = line.paint(point(bounds.left() + gutter, y), line_h, window, cx);
        }
        for (row, num) in prepaint.numbers.iter().enumerate() {
            let y = bounds.top() + line_h * row as f32;
            // Right-aligned in the gutter, so the digits line up as the numbers
            // grow a place.
            let x = bounds.left() + gutter - num.width - px(4.0);
            let _ = num.paint(point(x, y), line_h, window, cx);
        }
        // The caret only where the focus is, or two editors would appear to be
        // taking input at once.
        if focus.is_focused(window) {
            if let Some(caret) = prepaint.cursor.take() {
                window.paint_quad(caret);
            }
        }

        let lines = std::mem::take(&mut prepaint.lines);
        let rows = lines.len();
        self.editor.update(cx, |ed, _| {
            ed.last_lines = lines;
            ed.last_bounds = Some(bounds);
            ed.last_rows = rows;
        });
    }
}

/// Input-method support, over the CURRENT LINE.
///
/// The same shape as `input.rs`'s, with one difference that matters: the offsets
/// an IME works in are relative to the text it was given, and the text given
/// here is the line the cursor is on. A composition that spanned a line break
/// is not a thing that happens, and pretending to support one would mean
/// translating every offset through the whole document for no gain.
impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let line = self.doc.lines().get(self.doc.cursor().line)?.clone();
        let start = utf16_to_byte(&line, range.start);
        let end = utf16_to_byte(&line, range.end);
        if start != range.start || end != range.end {
            *adjusted = Some(byte_to_utf16(&line, start)..byte_to_utf16(&line, end));
        }
        Some(line[start..end].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let line = self.doc.lines().get(self.doc.cursor().line)?;
        let here = byte_to_utf16(line, self.doc.cursor().byte);
        let (start, end) = match self.doc.selection() {
            Some((a, b)) if a.line == b.line && a.line == self.doc.cursor().line => {
                (byte_to_utf16(line, a.byte), byte_to_utf16(line, b.byte))
            }
            _ => (here, here),
        };
        Some(UTF16Selection { range: start..end, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<std::ops::Range<usize>> {
        let line = self.doc.lines().get(self.doc.cursor().line)?;
        let m = self.marked.clone()?;
        Some(byte_to_utf16(line, m.start)..byte_to_utf16(line, m.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(r) = range.clone() {
            // The IME is replacing a range of the CURRENT LINE: select it and
            // let the model's insert do the work, so there is one code path
            // that edits text.
            let line = self.doc.lines()[self.doc.cursor().line].clone();
            let from = utf16_to_byte(&line, r.start);
            let to = utf16_to_byte(&line, r.end);
            let l = self.doc.cursor().line;
            self.doc.place(Pos::new(l, from), false);
            self.doc.place(Pos::new(l, to), true);
        }
        self.doc.insert(text);
        self.marked = None;
        self.follow_cursor();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        new_selected: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let l = self.doc.cursor().line;
        let line = self.doc.lines()[l].clone();
        let (from, to) = match range {
            Some(r) => (utf16_to_byte(&line, r.start), utf16_to_byte(&line, r.end)),
            None => match self.marked.clone() {
                Some(m) => (m.start, m.end),
                None => (self.doc.cursor().byte, self.doc.cursor().byte),
            },
        };
        self.doc.place(Pos::new(l, from), false);
        if to > from {
            self.doc.place(Pos::new(l, to), true);
        }
        self.doc.insert(text);
        self.marked = (!text.is_empty()).then(|| from..from + text.len());
        // The selection the IME asks for is relative to the text it just put
        // in — the same `+ range.start` the single-line field had wrong.
        if let (Some(sel), Some(m)) = (new_selected, self.marked.clone()) {
            let line = self.doc.lines()[l].clone();
            let s = utf16_to_byte(&line, sel.start) + m.start;
            self.doc.place(Pos::new(l, s.min(line.len())), false);
        }
        self.follow_cursor();
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: std::ops::Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Where the candidate window goes: at the start of the composition, on
        // the cursor's line.
        let row = self.doc.cursor().line.checked_sub(self.scroll_top)?;
        let shaped = self.last_lines.get(row)?;
        let line = self.doc.lines().get(self.doc.cursor().line)?;
        let start = utf16_to_byte(line, range_utf16.start);
        let line_h = px(f32::from(self.scale.line_h()));
        let x = bounds.left() + px(self.gutter_w()) + shaped.x_for_index(start);
        let y = bounds.top() + line_h * row as f32;
        Some(Bounds::new(point(x, y), size(px(1.0), line_h)))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let p = self.pos_for_point(point);
        let line = self.doc.lines().get(p.line)?;
        Some(byte_to_utf16(line, p.byte))
    }
}

fn byte_to_utf16(s: &str, byte: usize) -> usize {
    let byte = byte.min(s.len());
    s[..byte].chars().map(char::len_utf16).sum()
}

fn utf16_to_byte(s: &str, utf16: usize) -> usize {
    let mut at = 0usize;
    for (i, ch) in s.char_indices() {
        if at >= utf16 {
            return i;
        }
        at += ch.len_utf16();
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window follows the cursor, and never shows blank below the text.
    #[test]
    fn the_window_follows_the_cursor() {
        // Cursor below the window: the window moves down just far enough.
        assert_eq!(scroll_to_show(0, 5, 4, 20), 2);
        // Cursor above it: up to the cursor.
        assert_eq!(scroll_to_show(10, 3, 4, 20), 3);
        // Already inside: nothing moves.
        assert_eq!(scroll_to_show(4, 5, 4, 20), 4);
        // Never past the last page.
        assert_eq!(scroll_to_show(50, 19, 4, 20), 16);
        // Fewer lines than rows: always the top.
        assert_eq!(scroll_to_show(3, 1, 10, 4), 0);
        // Degenerate rather than panicking.
        assert_eq!(scroll_to_show(3, 1, 0, 4), 0);
    }

    /// Offsets cross between bytes and UTF-16 the way the platform's input
    /// methods count, which is not the way Rust counts.
    #[test]
    fn offsets_cross_both_ways() {
        let s = "aпри";
        assert_eq!(byte_to_utf16(s, 0), 0);
        assert_eq!(byte_to_utf16(s, 1), 1);
        assert_eq!(byte_to_utf16(s, 3), 2, "one Cyrillic letter is 2 bytes, 1 unit");
        assert_eq!(utf16_to_byte(s, 2), 3);
        assert_eq!(utf16_to_byte(s, 99), s.len(), "past the end is the end");
        // And an emoji, which is two UTF-16 units.
        let e = "a🙂b";
        assert_eq!(byte_to_utf16(e, 1), 1);
        assert_eq!(byte_to_utf16(e, 5), 3);
        assert_eq!(utf16_to_byte(e, 3), 5);
    }

    /// The box is between three and sixteen lines, whatever is in it.
    #[test]
    fn the_box_has_a_floor_and_a_ceiling() {
        let d = Doc::new("");
        assert_eq!(d.line_count().clamp(MIN_ROWS, MAX_ROWS), MIN_ROWS);
        let many = "x\n".repeat(100);
        assert_eq!(Doc::new(&many).line_count().clamp(MIN_ROWS, MAX_ROWS), MAX_ROWS);
        assert_eq!(Doc::new("a\nb\nc\nd\ne").line_count().clamp(MIN_ROWS, MAX_ROWS), 5);
    }
}
