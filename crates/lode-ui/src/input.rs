//! A text field.
//!
//! GPUI ships none, so this is one. It is adapted from the framework's own
//! `examples/input.rs`, which is the reference implementation and is 746 lines
//! for a single line of text — an input handler so the compositor's IME can
//! compose into it, a custom `Element` that shapes the text and paints the
//! cursor and the selection itself, and an action per key. That is the price of
//! the framework, and it is paid once here rather than worked around at every
//! call site.
//!
//! What this adds over the example:
//!
//!   * **Colours from the theme.** The example paints white, black and blue. A
//!     field in this client is drawn in the palette the rest of it is drawn in.
//!   * **An event when it changes and when Enter is pressed**, so a caller can
//!     filter a list as it is typed, or run a query on Enter, without polling.
//!   * **A label and a placeholder**, because every field in a connection form
//!     needs both and the engine supplies the placeholder per driver.
//!
//! SINGLE LINE, deliberately and for now. Multi-line needs its own layout pass
//! over wrapped lines rather than one `shape_line`, and everything blocked on
//! typing — the sidebar filter, the structured query bar, the console, every
//! field of the connection form — is single-line. The SQL editor is the one
//! caller that wants more, and it is the next step rather than this one.

use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, App, Bounds, ClipboardItem, Context,
    CursorStyle, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, Rgba, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, Window,
};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    lode_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
        Submit,
        /// Tab and shift-tab, for moving between the fields of a window.
        ///
        /// NOT handled by the field: focusing another one needs a `Window`, and
        /// a subscription has none. The action bubbles out of the focused field
        /// to the window's root, which has both the window and the list of
        /// fields in their order — see `ConnectionForm`.
        FocusNext,
        FocusPrev,
    ]
);

/// Bind the keys a field needs. Called once, at start.
pub fn bind_keys(cx: &mut App) {
    use gpui::KeyBinding;
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, None),
        KeyBinding::new("delete", Delete, None),
        KeyBinding::new("left", Left, None),
        KeyBinding::new("right", Right, None),
        KeyBinding::new("shift-left", SelectLeft, None),
        KeyBinding::new("shift-right", SelectRight, None),
        KeyBinding::new("ctrl-a", SelectAll, None),
        KeyBinding::new("home", Home, None),
        KeyBinding::new("end", End, None),
        KeyBinding::new("ctrl-v", Paste, None),
        KeyBinding::new("ctrl-x", Cut, None),
        KeyBinding::new("ctrl-c", Copy, None),
        KeyBinding::new("enter", Submit, None),
        // Only inside a field: `tab` in the EDITOR is an indent, and in the
        // main window it is nothing at all.
        KeyBinding::new("tab", FocusNext, Some("TextInput")),
        KeyBinding::new("shift-tab", FocusPrev, Some("TextInput")),
    ]);
}

/// What a field tells its owner.
pub enum InputEvent {
    /// The text changed — filter as you type.
    Changed,
    /// Enter was pressed — run it.
    Submit,
}

/// The colours a field is drawn in. Set from the theme; never guessed.
#[derive(Clone, Copy)]
pub struct InputColors {
    pub text: Rgba,
    pub placeholder: Rgba,
    pub cursor: Rgba,
    pub selection: Rgba,
    pub background: Rgba,
    pub border: Rgba,
}

pub struct TextInput {
    pub focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    colors: InputColors,
    /// Draw this value as dots.
    ///
    /// DRAWING, not a different value: `content` is untouched, and every offset
    /// that crosses into the drawn line is translated. The cursor, the
    /// selection, click-to-place and the input method all keep working on the
    /// real text, because they are working on the real text.
    mask: bool,
    /// Showing it anyway, for now.
    ///
    /// Deliberately not persisted and not on the session: a field that
    /// remembers it was revealed is a field that is one day revealed in front of
    /// somebody. It dies with the window.
    reveal: bool,
}

/// One dot per character. Per CHARACTER, so the count of dots is the length of
/// the value and nothing more is told.
const MASK_CH: char = '\u{2022}';

/// A byte offset in `content` → the same byte offset in a line of dots.
///
/// One dot per CHARACTER, and a dot is three bytes while a character may be
/// one, two, three or four — so the two strings do not share offsets and every
/// offset that crosses between them has to be converted. Getting this wrong
/// does not look like a bug in a password field: it looks like the cursor
/// jumping, or like a panic on a character boundary the moment somebody types
/// a non-ASCII passphrase.
fn to_display_in(content: &str, off: usize) -> usize {
    let off = off.min(content.len());
    // Snap to a boundary rather than panicking on a stray offset.
    let mut end = off;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    content[..end].chars().count() * MASK_CH.len_utf8()
}

/// A byte offset in a line of dots → the same place in `content`.
fn from_display_in(content: &str, off: usize) -> usize {
    let nth = off / MASK_CH.len_utf8();
    content.char_indices().nth(nth).map(|(i, _)| i).unwrap_or(content.len())
}

impl EventEmitter<InputEvent> for TextInput {}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>, colors: InputColors) -> Self {
        TextInput {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            colors,
            mask: false,
            reveal: false,
        }
    }

    /// Draw this field's value as dots.
    pub fn set_mask(&mut self, mask: bool, cx: &mut Context<Self>) {
        if self.mask != mask {
            self.mask = mask;
            // Changing what a field IS also puts away any showing of it: a field
            // that becomes secret becomes secret now, not after the next click.
            self.reveal = false;
            cx.notify();
        }
    }

    pub fn revealed(&self) -> bool {
        self.reveal
    }

    pub fn toggle_reveal(&mut self, cx: &mut Context<Self>) {
        self.reveal = !self.reveal;
        cx.notify();
    }

    /// Is the drawn line different from the stored one right now?
    fn masking(&self) -> bool {
        self.mask && !self.reveal && !self.content.is_empty()
    }

    /// The line as it is DRAWN.
    fn display_text(&self) -> SharedString {
        if self.masking() {
            SharedString::from(MASK_CH.to_string().repeat(self.content.chars().count()))
        } else {
            self.content.clone()
        }
    }

    /// A byte offset in `content` → the same place in the drawn line.
    fn to_display(&self, off: usize) -> usize {
        if !self.masking() {
            return off;
        }
        to_display_in(&self.content, off)
    }

    /// A byte offset in the drawn line → the same place in `content`.
    fn from_display(&self, off: usize) -> usize {
        if !self.masking() {
            return off;
        }
        from_display_in(&self.content, off)
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.marked_range = None;
        cx.notify();
    }

    pub fn set_placeholder(&mut self, p: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = p.into();
        cx.notify();
    }

    /// Repaint in a new palette. A theme change must reach every open field.
    pub fn set_colors(&mut self, colors: InputColors, cx: &mut Context<Self>) {
        self.colors = colors;
        cx.notify();
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submit);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            // One line, so a pasted newline becomes a space rather than
            // silently truncating what was pasted. BOTH characters: text
            // copied from a Windows editor or a browser arrives as CRLF, and
            // replacing only the `\n` left the `\r` in the field — invisible,
            // and sent to the engine as part of a host name or a statement.
            self.replace_text_in_range(None, &text.replace(['\n', '\r'], " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.from_display(line.closest_index_for_x(position.x - bounds.left()))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    /// Grapheme boundaries, not bytes: one press of Left must step over a
    /// whole character, including the ones that are several bytes.
    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.marked_range = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            // BOTH ends are offsets from the start of the replaced range.
            // This said `+ range.end` for the end, which is the line as it
            // stands in gpui's own example and is wrong whenever the replaced
            // range is not empty: committing an IME composition over a
            // selection left the caret `range.len()` characters too far right,
            // so the next keystroke landed outside the text just inserted.
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(self.to_display(range.start)),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(self.to_display(range.end)),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = self.from_display(last_layout.index_for_x(point.x - line_point.x)?);
        Some(self.offset_to_utf16(utf8_index))
    }
}

/// The element that actually draws the text, the cursor and the selection.
/// GPUI has no widget for this, so the field owns its own painting.
struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

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
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
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
        let input = self.input.read(cx);
        let content = input.content.clone();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let colors = input.colors;
        let style = window.text_style();

        let masking = input.masking();
        let (display_text, text_color) = if content.is_empty() {
            (input.placeholder.clone(), colors.placeholder)
        } else {
            // Dots when this is a secret and nobody asked to see it. The
            // placeholder is never masked: it is not a value.
            (input.display_text(), colors.text)
        };
        // Offsets are byte offsets into `content`; the shaped line is the DRAWN
        // text. When those differ, every offset crossing over is translated.
        let selected_range = input.to_display(selected_range.start)..input.to_display(selected_range.end);
        let cursor = input.to_display(cursor);

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        // Text being composed by an input method is underlined, so it is visibly
        // not yet committed. Not while masked: the underline would mark out
        // which of the dots are new.
        let runs = if let Some(marked) = input.marked_range.as_ref().filter(|_| !masking) {
            vec![
                TextRun { len: marked.start, ..run.clone() },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun { len: display_text.len() - marked.end, ..run },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(1.5), bounds.bottom() - bounds.top()),
                    ),
                    colors.cursor,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left() + line.x_for_index(selected_range.start), bounds.top()),
                        point(bounds.left() + line.x_for_index(selected_range.end), bounds.bottom()),
                    ),
                    colors.selection,
                )),
                None,
            )
        };
        PrepaintState { line: Some(line), cursor, selection }
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
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let line = prepaint.line.take().unwrap();
        let _ = line.paint(bounds.origin, window.line_height(), window, cx);

        // The cursor only where the focus is, or several fields would appear to
        // be taking input at once.
        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // WHICH FIELD TAKES THE KEYBOARD. Every field drew the same border
        // whether it had focus or not, so a window with four of them gave no
        // sign at all of where typing would land — and the caret, which is the
        // other answer, is only drawn while there is text to put it in.
        //
        // The caret's own colour, not a new token: it is the one colour in the
        // palette that already means "the keyboard is here".
        let focused = self.focus_handle.is_focused(window);
        div()
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .px_1()
            .bg(self.colors.background)
            .border_1()
            .border_color(if focused { self.colors.cursor } else { self.colors.border })
            .child(TextElement { input: cx.entity() })
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dots are DRAWING. Every offset that crosses between the stored value
    /// and the drawn line survives the trip — including through a passphrase
    /// that is not ASCII, which is where a naive one-byte-per-character mask
    /// panics on a character boundary.
    #[test]
    fn an_offset_survives_the_round_trip_through_the_dots() {
        for content in [
            "hunter2",
            "correct-horse!battery#staple$42",
            "паролна-фраза-с-кирилица",
            "mixed-Фраза-42-ü-ß",
            "\u{1f511}-key-emoji",
            "",
        ] {
            // Every character boundary, and the end.
            let mut offsets: Vec<usize> = content.char_indices().map(|(i, _)| i).collect();
            offsets.push(content.len());
            for off in offsets {
                let there = to_display_in(content, off);
                let back = from_display_in(content, there);
                assert_eq!(back, off, "content {content:?}, offset {off}");
            }
        }
    }

    /// The line of dots is as long as the value is, in characters — and says
    /// nothing else about it.
    #[test]
    fn the_dots_count_characters_not_bytes() {
        let content = "паролна-фраза";
        let dots = MASK_CH.to_string().repeat(content.chars().count());
        assert_eq!(dots.chars().count(), content.chars().count());
        assert_ne!(dots.len(), content.len(), "the byte lengths differ, which is the point");
        // The far end of the value is the far end of the dots.
        assert_eq!(to_display_in(content, content.len()), dots.len());
    }

    /// An offset landing inside a character does not panic and does not run
    /// past the end — both of which are reachable from a stray mouse position.
    #[test]
    fn a_stray_offset_is_survivable() {
        let content = "фраза";
        assert_eq!(to_display_in(content, 1), 0, "inside the first character");
        assert_eq!(to_display_in(content, 9_999), to_display_in(content, content.len()));
        assert_eq!(from_display_in(content, 9_999), content.len());
    }
}
