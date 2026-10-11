//! A multi-line text editor: the model.
//!
//! WHY THERE IS ONE AT ALL. The statement box was a single line. Every
//! statement longer than the box was typed blind, a formatted query pasted in
//! arrived as one line with its newlines turned into spaces, and nothing could
//! be read back. It is the largest gap between this client and dbflux, and the
//! one people hit first.
//!
//! WHY IT IS OURS. dbflux uses `gpui-component`'s `InputState::multi_line()`.
//! That crate depends on `gpui-pre`, a DIFFERENT crate from the `gpui` this
//! client is built on — not a different version, a different package, with its
//! own `Entity`, `Window` and `App`. Two of them in one binary cannot share an
//! element tree at all, so the component's editor could not be put on this
//! window's screen under any amount of glue. Checked rather than assumed:
//! `gpui-component 0.7.1` pins `gpui-pre =0.3.8`.
//!
//! WHAT IS IN THIS FILE: the MODEL, and nothing that draws. Every operation a
//! key can perform is a method here, with no `Window`, no `App` and no element,
//! so all of it is testable without a screen — which is the only way the
//! awkward parts (graphemes, selection direction, undo coalescing) get checked
//! at all. The element and the key bindings live beside it in `editor_el.rs`.

use unicode_segmentation::UnicodeSegmentation;

/// A place in the text: which line, and how many BYTES into it.
///
/// Bytes rather than characters, because that is what slicing a `String` wants
/// and what the text system's ranges are in. Every byte offset this type holds
/// is on a grapheme boundary — the moves are what guarantee it, and
/// `clamp_to_boundary` is what repairs it if something hands in an offset from
/// elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    pub line: usize,
    pub byte: usize,
}

impl Pos {
    pub fn new(line: usize, byte: usize) -> Self {
        Pos { line, byte }
    }
}

/// One step of history: the whole text and where the cursor was.
///
/// The WHOLE text, not a diff. A statement is a few hundred bytes and a session
/// holds a few dozen edits of one, so a snapshot costs nothing worth measuring
/// — and a diff-based undo is where undo bugs live. Said here so the next
/// person does not "optimise" it into one.
#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
}

/// How many edits of history to keep.
const HISTORY: usize = 200;

/// The editor's text, cursor and selection.
#[derive(Debug, Clone)]
pub struct Doc {
    lines: Vec<String>,
    cursor: Pos,
    /// Where a selection started, if there is one. The cursor is its other end,
    /// so a selection has a DIRECTION and shift-arrow can shrink it again.
    anchor: Option<Pos>,
    /// The column a vertical move is trying to keep, in bytes.
    ///
    /// Up and down through a short line must not lose the column: moving from
    /// byte 30 through a 4-byte line and on should arrive back at 30, not at 4.
    goal: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl Default for Doc {
    fn default() -> Self {
        Doc {
            lines: vec![String::new()],
            cursor: Pos::default(),
            anchor: None,
            goal: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

impl Doc {
    pub fn new(text: &str) -> Self {
        let mut d = Doc::default();
        d.set_text(text);
        d
    }

    /// The whole text, with `\n` between lines.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Replace everything. Clears the history: this is a new document, not an
    /// edit of the old one, and offering to undo back into somebody else's
    /// statement is worse than offering nothing.
    pub fn set_text(&mut self, text: &str) {
        // `\r\n` and a lone `\r` both become one line break. Text pasted from a
        // Windows editor arrives as CRLF, and a `\r` left inside a line is
        // invisible here and goes to the engine as part of the statement.
        let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
        self.lines = normalised.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor = self.end();
        self.anchor = None;
        self.goal = None;
        self.undo.clear();
        self.redo.clear();
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn cursor(&self) -> Pos {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// The end of the text.
    pub fn end(&self) -> Pos {
        let line = self.lines.len() - 1;
        Pos::new(line, self.lines[line].len())
    }

    fn line(&self, n: usize) -> &str {
        self.lines.get(n).map(String::as_str).unwrap_or("")
    }

    /// Bring a position inside the text and onto a grapheme boundary.
    pub fn clamp(&self, p: Pos) -> Pos {
        let line = p.line.min(self.lines.len() - 1);
        let s = self.line(line);
        let mut byte = p.byte.min(s.len());
        while byte > 0 && !s.is_char_boundary(byte) {
            byte -= 1;
        }
        // And onto a GRAPHEME boundary, which is not the same thing: a
        // combining mark is its own char and half of one grapheme.
        if byte > 0 && byte < s.len() {
            let ok = s.grapheme_indices(true).any(|(i, _)| i == byte);
            if !ok {
                byte = s
                    .grapheme_indices(true)
                    .take_while(|(i, _)| *i < byte)
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
        }
        Pos::new(line, byte)
    }

    // ── selection ───────────────────────────────────────────────────────────

    /// The selection as an ordered pair, if there is one.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        if a == self.cursor {
            return None;
        }
        Some(if a <= self.cursor { (a, self.cursor) } else { (self.cursor, a) })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (from, to) = self.selection()?;
        Some(self.slice(from, to))
    }

    fn slice(&self, from: Pos, to: Pos) -> String {
        if from.line == to.line {
            let s = self.line(from.line);
            return s[from.byte.min(s.len())..to.byte.min(s.len())].to_string();
        }
        let mut out = String::new();
        let first = self.line(from.line);
        out.push_str(&first[from.byte.min(first.len())..]);
        for n in (from.line + 1)..to.line {
            out.push('\n');
            out.push_str(self.line(n));
        }
        out.push('\n');
        let last = self.line(to.line);
        out.push_str(&last[..to.byte.min(last.len())]);
        out
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(Pos::default());
        self.cursor = self.end();
        self.goal = None;
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Start or extend a selection, then move. The caller says whether the
    /// shift key was down; this is the only place that decides what that means.
    fn moving(&mut self, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
    }

    // ── moving ──────────────────────────────────────────────────────────────

    pub fn move_left(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        if self.cursor.byte > 0 {
            let s = self.line(self.cursor.line);
            let to = s
                .grapheme_indices(true)
                .take_while(|(i, _)| *i < self.cursor.byte)
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor.byte = to;
        } else if self.cursor.line > 0 {
            // Over the line break, to the end of the line above.
            self.cursor.line -= 1;
            self.cursor.byte = self.line(self.cursor.line).len();
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        let len = self.line(self.cursor.line).len();
        if self.cursor.byte < len {
            let s = self.line(self.cursor.line);
            let to = s
                .grapheme_indices(true)
                .find_map(|(i, _)| (i > self.cursor.byte).then_some(i))
                .unwrap_or(len);
            self.cursor.byte = to;
        } else if self.cursor.line + 1 < self.lines.len() {
            self.cursor.line += 1;
            self.cursor.byte = 0;
        }
    }

    pub fn move_up(&mut self, extend: bool) {
        self.moving(extend);
        let goal = *self.goal.get_or_insert(self.cursor.byte);
        if self.cursor.line == 0 {
            self.cursor.byte = 0;
            return;
        }
        self.cursor.line -= 1;
        self.cursor = self.clamp(Pos::new(self.cursor.line, goal));
        // The goal SURVIVES the clamp: moving up through a short line and on
        // must come back to the column it started in.
        self.goal = Some(goal);
    }

    pub fn move_down(&mut self, extend: bool) {
        self.moving(extend);
        let goal = *self.goal.get_or_insert(self.cursor.byte);
        if self.cursor.line + 1 >= self.lines.len() {
            self.cursor.byte = self.line(self.cursor.line).len();
            return;
        }
        self.cursor.line += 1;
        self.cursor = self.clamp(Pos::new(self.cursor.line, goal));
        self.goal = Some(goal);
    }

    /// Home: to the first non-blank, or to column 0 if already there.
    ///
    /// Two presses rather than one, because an indented statement's useful
    /// start is its first word, and the margin is where you go to type an
    /// indent.
    pub fn move_home(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        let s = self.line(self.cursor.line);
        let indent = s.len() - s.trim_start().len();
        self.cursor.byte = if self.cursor.byte == indent { 0 } else { indent };
    }

    pub fn move_end(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        self.cursor.byte = self.line(self.cursor.line).len();
    }

    pub fn move_doc_start(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        self.cursor = Pos::default();
    }

    pub fn move_doc_end(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        self.cursor = self.end();
    }

    pub fn move_page(&mut self, lines: isize, extend: bool) {
        self.moving(extend);
        let goal = *self.goal.get_or_insert(self.cursor.byte);
        let target = (self.cursor.line as isize + lines).clamp(0, self.lines.len() as isize - 1);
        self.cursor = self.clamp(Pos::new(target as usize, goal));
        self.goal = Some(goal);
    }

    /// One word left. A WORD here is a run of word characters; the gap before
    /// it is crossed first, so pressing it in whitespace lands on a word rather
    /// than nowhere.
    pub fn move_word_left(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        if self.cursor.byte == 0 {
            self.move_left(extend);
            return;
        }
        let s = self.line(self.cursor.line).to_string();
        let starts: Vec<usize> = word_starts(&s);
        let to = starts.into_iter().take_while(|i| *i < self.cursor.byte).last().unwrap_or(0);
        self.cursor.byte = to;
    }

    pub fn move_word_right(&mut self, extend: bool) {
        self.moving(extend);
        self.goal = None;
        let s = self.line(self.cursor.line).to_string();
        if self.cursor.byte >= s.len() {
            self.move_right(extend);
            return;
        }
        let to = word_starts(&s)
            .into_iter()
            .find(|i| *i > self.cursor.byte)
            .unwrap_or(s.len());
        self.cursor.byte = to;
    }

    // ── editing ─────────────────────────────────────────────────────────────

    fn snapshot(&mut self) {
        self.undo.push(Snapshot { lines: self.lines.clone(), cursor: self.cursor });
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        // A new edit makes the redo stack meaningless: it described a future
        // that no longer follows from here.
        self.redo.clear();
    }

    /// Put text in, replacing the selection if there is one.
    pub fn insert(&mut self, text: &str) {
        self.snapshot();
        self.delete_selection_inner();
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let p = self.clamp(self.cursor);
        let rest = self.lines[p.line].split_off(p.byte);
        let mut parts = text.split('\n');
        let first = parts.next().unwrap_or("");
        self.lines[p.line].push_str(first);
        let mut at = p.line;
        for part in parts {
            at += 1;
            self.lines.insert(at, part.to_string());
        }
        self.cursor = Pos::new(at, self.lines[at].len());
        self.lines[at].push_str(&rest);
        self.anchor = None;
        self.goal = None;
    }

    /// Enter: a new line, carrying the current line's indent.
    ///
    /// An editor that drops to column 0 inside an indented block makes somebody
    /// retype the indent on every line, which is why every editor does this.
    pub fn newline(&mut self) {
        let s = self.line(self.cursor.line);
        let indent: String = s.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        // Only up to the cursor: pressing Enter in the middle of an indent must
        // not copy more indent than there is behind it.
        let keep = indent.len().min(self.cursor.byte);
        let indent = indent[..keep].to_string();
        self.insert(&format!("\n{indent}"));
    }

    /// Tab: two spaces, or one level of indent on every selected line.
    pub fn tab(&mut self) {
        match self.selection() {
            None => self.insert("  "),
            Some((from, to)) => {
                self.snapshot();
                for n in from.line..=to.line.min(self.lines.len() - 1) {
                    self.lines[n].insert_str(0, "  ");
                }
                self.anchor = Some(Pos::new(from.line, from.byte + 2));
                self.cursor = Pos::new(to.line, to.byte + 2);
            }
        }
    }

    /// Shift-Tab: take one level of indent off, where there is one.
    pub fn untab(&mut self) {
        self.snapshot();
        let (from, to) = self.selection().unwrap_or((self.cursor, self.cursor));
        for n in from.line..=to.line.min(self.lines.len() - 1) {
            let taken = if self.lines[n].starts_with("  ") {
                self.lines[n].replace_range(0..2, "");
                2
            } else if self.lines[n].starts_with(' ') || self.lines[n].starts_with('\t') {
                self.lines[n].replace_range(0..1, "");
                1
            } else {
                0
            };
            if n == self.cursor.line {
                self.cursor.byte = self.cursor.byte.saturating_sub(taken);
            }
            if let Some(a) = self.anchor.as_mut() {
                if a.line == n {
                    a.byte = a.byte.saturating_sub(taken);
                }
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.selection().is_some() {
            self.delete_selection();
            return;
        }
        self.snapshot();
        if self.cursor.byte > 0 {
            let s = &self.lines[self.cursor.line];
            let from = s
                .grapheme_indices(true)
                .take_while(|(i, _)| *i < self.cursor.byte)
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.lines[self.cursor.line].replace_range(from..self.cursor.byte, "");
            self.cursor.byte = from;
        } else if self.cursor.line > 0 {
            // Join with the line above.
            let gone = self.lines.remove(self.cursor.line);
            self.cursor.line -= 1;
            self.cursor.byte = self.lines[self.cursor.line].len();
            self.lines[self.cursor.line].push_str(&gone);
        } else {
            // Nothing to delete: take the snapshot back off, or Ctrl-Z would
            // undo a keystroke that changed nothing.
            self.undo.pop();
        }
        self.goal = None;
    }

    pub fn delete(&mut self) {
        if self.selection().is_some() {
            self.delete_selection();
            return;
        }
        self.snapshot();
        let len = self.lines[self.cursor.line].len();
        if self.cursor.byte < len {
            let s = &self.lines[self.cursor.line];
            let to = s
                .grapheme_indices(true)
                .find_map(|(i, _)| (i > self.cursor.byte).then_some(i))
                .unwrap_or(len);
            self.lines[self.cursor.line].replace_range(self.cursor.byte..to, "");
        } else if self.cursor.line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor.line + 1);
            self.lines[self.cursor.line].push_str(&next);
        } else {
            self.undo.pop();
        }
        self.goal = None;
    }

    pub fn delete_selection(&mut self) {
        if self.selection().is_none() {
            return;
        }
        self.snapshot();
        self.delete_selection_inner();
    }

    fn delete_selection_inner(&mut self) {
        let Some((from, to)) = self.selection() else { return };
        if from.line == to.line {
            let len = self.lines[from.line].len();
            self.lines[from.line].replace_range(from.byte.min(len)..to.byte.min(len), "");
        } else {
            let tail = self.lines[to.line][to.byte.min(self.lines[to.line].len())..].to_string();
            let keep = from.byte.min(self.lines[from.line].len());
            self.lines[from.line].truncate(keep);
            self.lines[from.line].push_str(&tail);
            self.lines.drain((from.line + 1)..=to.line);
        }
        self.cursor = from;
        self.anchor = None;
        self.goal = None;
    }

    /// Duplicate the line the cursor is on, below it.
    pub fn duplicate_line(&mut self) {
        self.snapshot();
        let copy = self.lines[self.cursor.line].clone();
        self.lines.insert(self.cursor.line + 1, copy);
        self.cursor.line += 1;
        self.anchor = None;
    }

    /// Comment or uncomment the selected lines, with `--`.
    ///
    /// ONE decision for the whole selection, taken from the FIRST line: a
    /// per-line decision turns a half-commented block inside out, which is
    /// never what the keystroke meant.
    pub fn toggle_comment(&mut self, prefix: &str) {
        self.snapshot();
        let (from, to) = self.selection().unwrap_or((self.cursor, self.cursor));
        let last = to.line.min(self.lines.len() - 1);
        let commenting = !self.lines[from.line].trim_start().starts_with(prefix);
        for n in from.line..=last {
            if commenting {
                let indent = self.lines[n].len() - self.lines[n].trim_start().len();
                self.lines[n].insert_str(indent, &format!("{prefix} "));
            } else {
                let s = self.lines[n].clone();
                let indent = s.len() - s.trim_start().len();
                let rest = &s[indent..];
                if let Some(stripped) = rest.strip_prefix(prefix) {
                    let stripped = stripped.strip_prefix(' ').unwrap_or(stripped);
                    self.lines[n] = format!("{}{}", &s[..indent], stripped);
                }
            }
        }
        self.cursor = self.clamp(self.cursor);
        if let Some(a) = self.anchor {
            self.anchor = Some(self.clamp(a));
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(Snapshot { lines: self.lines.clone(), cursor: self.cursor });
        self.lines = prev.lines;
        self.cursor = self.clamp(prev.cursor);
        self.anchor = None;
        self.goal = None;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(Snapshot { lines: self.lines.clone(), cursor: self.cursor });
        self.lines = next.lines;
        self.cursor = self.clamp(next.cursor);
        self.anchor = None;
        self.goal = None;
        true
    }

    /// The word the cursor is at the end of, and where it starts.
    ///
    /// A WORD here includes dots, because `users.na` is one thing to complete
    /// and two words to a lexer. It stops at whitespace, at a bracket, at a
    /// comma — anywhere a name cannot continue.
    ///
    /// `None` when the cursor is not at the end of one: completing while the
    /// cursor sits in the middle of a name would replace the half in front of
    /// it, which is never what the keystroke meant.
    pub fn word_before_cursor(&self) -> Option<(String, usize)> {
        let p = self.clamp(self.cursor);
        let line = self.line(p.line);
        // Not at the end of a word: the character to the RIGHT is one a name
        // can continue with.
        if let Some(next) = line[p.byte..].chars().next() {
            if next.is_alphanumeric() || next == '_' || next == '.' {
                return None;
            }
        }
        let head = &line[..p.byte];
        let start = head
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_' || *c == '.')
            .last()
            .map(|(i, _)| i)?;
        let word = head[start..].to_string();
        if word.is_empty() {
            return None;
        }
        Some((word, start))
    }

    /// Replace the word the cursor is at the end of.
    pub fn replace_word_before_cursor(&mut self, with: &str) {
        let Some((word, start)) = self.word_before_cursor() else {
            self.insert(with);
            return;
        };
        let line = self.cursor.line;
        self.place(Pos::new(line, start), false);
        self.place(Pos::new(line, start + word.len()), true);
        self.insert(with);
    }

    /// Everything to the left of the cursor, for deciding whether a separator
    /// is wanted before what is about to be inserted.
    pub fn text_before_cursor(&self) -> String {
        let p = self.clamp(self.cursor);
        let mut out = String::new();
        for n in 0..p.line {
            out.push_str(self.line(n));
            out.push('\n');
        }
        out.push_str(&self.line(p.line)[..p.byte]);
        out
    }

    /// Put the cursor somewhere, e.g. where a click landed.
    pub fn place(&mut self, p: Pos, extend: bool) {
        self.moving(extend);
        self.goal = None;
        self.cursor = self.clamp(p);
    }
}

/// What a run of statement text IS, for colouring it.
///
/// Seven roles and no more, because the 48 themes carry exactly these: the
/// generator reads the editor's `syntax.*` groups, and inventing an eighth role
/// here would mean inventing a colour for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlRole {
    Keyword,
    Str,
    Num,
    Comment,
    /// A quoted identifier: `"col"`, `` `col` ``, `[col]`.
    Ident,
    Operator,
    Plain,
}

/// The words every SQL dialect spells the same way.
///
/// A LIST, not a grammar. Highlighting is a reading aid: a word in the wrong
/// colour costs a glance, and a parser that is wrong about a dialect costs a
/// rewrite. The engine is the only thing that decides what a statement MEANS —
/// see `query.classify` — and this is deliberately too dumb to disagree with it
/// about anything that matters.
pub const KEYWORDS: &[&str] = &[
    "add", "all", "alter", "and", "any", "as", "asc", "begin", "between", "by", "case", "cast",
    "check", "collate", "column", "commit", "constraint", "create", "cross", "current", "database",
    "default", "delete", "desc", "distinct", "drop", "else", "end", "escape", "except", "exists",
    "explain", "false", "for", "foreign", "from", "full", "function", "grant", "group", "having",
    "if", "ilike", "in", "index", "inner", "insert", "intersect", "into", "is", "join", "key",
    "left", "like", "limit", "not", "null", "offset", "on", "or", "order", "outer", "primary",
    "references", "rename", "replace", "returning", "revoke", "right", "rollback", "schema",
    "select", "set", "show", "table", "then", "to", "transaction", "true", "truncate", "union",
    "unique", "update", "using", "vacuum", "values", "view", "when", "where", "window", "with",
];

/// Split statement text into coloured runs.
///
/// Byte lengths that COVER THE TEXT, exactly as `workspace::json_runs` does and
/// for the same reason: a highlight range off a character boundary is a panic
/// in the text system, not a wrong colour. The tests push rubbish through it —
/// an unterminated string, a half-written comment, Cyrillic — because a
/// half-typed statement is the normal state of an editor.
pub fn sql_runs(text: &str) -> Vec<(usize, SqlRole)> {
    let b = text.as_bytes();
    let mut out: Vec<(usize, SqlRole)> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        let role = match b[i] {
            // A line comment, to the end of the line.
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                SqlRole::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                SqlRole::Comment
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                SqlRole::Comment
            }
            // A string. `''` inside one is an escaped quote in SQL.
            b'\'' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                SqlRole::Str
            }
            // A quoted IDENTIFIER, in any of the three spellings.
            b'"' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                SqlRole::Ident
            }
            b'[' => {
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                i = (i + 1).min(b.len());
                SqlRole::Ident
            }
            c if c.is_ascii_digit() => {
                while i < b.len()
                    && (b[i].is_ascii_digit() || b[i] == b'.' || b[i] == b'e' || b[i] == b'E')
                {
                    i += 1;
                }
                SqlRole::Num
            }
            c if c == b'_' || c.is_ascii_alphabetic() => {
                while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                let word = &text[start..i];
                if KEYWORDS.contains(&word.to_ascii_lowercase().as_str()) {
                    SqlRole::Keyword
                } else {
                    SqlRole::Plain
                }
            }
            c if b"+-*/%<>=!|&^~(),;:.".contains(&c) => {
                i += 1;
                SqlRole::Operator
            }
            _ => {
                // Whitespace, and any character this lexer has no opinion
                // about — including every multi-byte one, which is stepped as a
                // whole so a run never ends inside a character.
                i += 1;
                while i < b.len() && !text.is_char_boundary(i) {
                    i += 1;
                }
                SqlRole::Plain
            }
        };
        let len = i - start;
        match out.last_mut() {
            Some((l, r)) if *r == role => *l += len,
            _ => out.push((len, role)),
        }
    }
    out
}

/// Where each word on a line begins.
fn word_starts(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut was_word = false;
    for (i, g) in s.grapheme_indices(true) {
        let is_word = g.chars().next().map_or(false, |c| c.is_alphanumeric() || c == '_');
        if is_word && !was_word {
            out.push(i);
        }
        was_word = is_word;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runs cover the text exactly, and name the words apart.
    #[test]
    fn sql_colours_cover_every_byte() {
        use super::SqlRole::*;
        let text = "SELECT count(*) -- how many\nFROM \"t\" WHERE n > 12 AND s = 'it''s'";
        let runs = sql_runs(text);
        assert_eq!(
            runs.iter().map(|(l, _)| *l).sum::<usize>(),
            text.len(),
            "{runs:?}"
        );
        let mut at = 0usize;
        for (l, _) in &runs {
            at += l;
            assert!(text.is_char_boundary(at));
        }
        let roles: Vec<super::SqlRole> = runs.iter().map(|(_, r)| *r).collect();
        for want in [Keyword, Str, Num, Comment, Ident, Operator] {
            assert!(roles.contains(&want), "{want:?} missing from {roles:?}");
        }
    }

    /// `''` inside a string does not end it, and a keyword inside one is not a
    /// keyword — which is the whole reason a lexer beats a word search.
    #[test]
    fn a_keyword_inside_a_string_stays_a_string() {
        use super::SqlRole::*;
        let text = "where s = 'select from'";
        let runs = sql_runs(text);
        assert_eq!(runs.iter().map(|(l, _)| *l).sum::<usize>(), text.len());
        // One string run, holding the whole quoted part.
        let strs: Vec<usize> = runs.iter().filter(|(_, r)| *r == Str).map(|(l, _)| *l).collect();
        assert_eq!(strs, vec!["'select from'".len()], "{runs:?}");
        // And the escaped-quote form is one run too.
        let text = "'it''s'";
        let runs = sql_runs(text);
        assert_eq!(runs, vec![(text.len(), Str)]);
    }

    /// Half-written text is the NORMAL state of an editor.
    #[test]
    fn a_half_typed_statement_is_still_covered() {
        for text in [
            "",
            "sel",
            "select 'unterminated",
            "/* unclosed",
            "-- just a comment",
            "where име = 'стойност'",
            "\"unclosed ident",
            "12.",
            "::",
        ] {
            let runs = sql_runs(text);
            assert_eq!(
                runs.iter().map(|(l, _)| *l).sum::<usize>(),
                text.len(),
                "{text:?} -> {runs:?}"
            );
            let mut at = 0usize;
            for (l, _) in &runs {
                at += l;
                assert!(text.is_char_boundary(at), "{text:?} broke at {at}");
            }
        }
    }

    #[test]
    fn text_goes_in_and_comes_back() {
        let d = Doc::new("select 1\nfrom t");
        assert_eq!(d.line_count(), 2);
        assert_eq!(d.text(), "select 1\nfrom t");
        // The cursor lands at the END, which is where somebody opening a
        // statement to edit it wants to be.
        assert_eq!(d.cursor(), Pos::new(1, 6));
    }

    /// CRLF becomes one line break. A query pasted from a Windows editor
    /// otherwise carries an invisible `\r` into the statement the engine runs.
    #[test]
    fn crlf_is_one_break() {
        let d = Doc::new("a\r\nb\rc\nd");
        assert_eq!(d.lines(), ["a", "b", "c", "d"]);
        assert!(!d.text().contains('\r'));
    }

    /// What is behind the cursor, which is how an inserted name decides
    /// whether it needs a space in front of it.
    #[test]
    fn the_text_behind_the_cursor_is_readable() {
        let mut d = Doc::new("select \nfrom t");
        d.place(Pos::new(0, 7), false);
        assert_eq!(d.text_before_cursor(), "select ");
        d.place(Pos::new(1, 4), false);
        assert_eq!(d.text_before_cursor(), "select \nfrom");
        d.move_doc_start(false);
        assert_eq!(d.text_before_cursor(), "");
    }

    /// The word under the cursor, and what counts as one.
    #[test]
    fn the_word_at_the_cursor_is_the_one_being_typed() {
        let mut d = Doc::new("select us");
        d.move_end(false);
        assert_eq!(d.word_before_cursor(), Some(("us".into(), 7)));

        // A dot is part of it: `users.na` is ONE thing to complete.
        let mut d = Doc::new("where users.na");
        d.move_end(false);
        assert_eq!(d.word_before_cursor().map(|(w, _)| w), Some("users.na".into()));

        // Not at the END of a word: completing here would replace the half in
        // front of the cursor.
        let mut d = Doc::new("select name");
        d.place(Pos::new(0, 9), false);
        assert_eq!(d.word_before_cursor(), None);

        // After a space, a bracket or a comma there is no word.
        for text in ["select ", "count(", "a, "] {
            let mut d = Doc::new(text);
            d.move_end(false);
            assert_eq!(d.word_before_cursor(), None, "{text:?}");
        }
        // An empty document has none either.
        assert_eq!(Doc::new("").word_before_cursor(), None);
    }

    #[test]
    fn accepting_a_completion_replaces_the_word() {
        let mut d = Doc::new("select * from us");
        d.move_end(false);
        d.replace_word_before_cursor("users");
        assert_eq!(d.text(), "select * from users");
        assert_eq!(d.cursor(), Pos::new(0, 19));

        // A qualified one replaces the whole thing, dot and all.
        let mut d = Doc::new("where users.na");
        d.move_end(false);
        d.replace_word_before_cursor("users.name");
        assert_eq!(d.text(), "where users.name");

        // And it is ONE undo step, not two.
        assert!(d.undo());
        assert_eq!(d.text(), "where users.na");
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut d = Doc::new("");
        d.insert("select");
        d.insert(" 1");
        assert_eq!(d.text(), "select 1");
        d.move_home(false);
        d.insert("-- ");
        assert_eq!(d.text(), "-- select 1");
        assert_eq!(d.cursor(), Pos::new(0, 3));
    }

    #[test]
    fn a_pasted_newline_makes_a_line() {
        let mut d = Doc::new("ab");
        d.move_home(false);
        d.move_right(false);
        d.insert("X\nY");
        assert_eq!(d.text(), "aX\nYb");
        assert_eq!(d.cursor(), Pos::new(1, 1));
    }

    /// Up and down keep the COLUMN across a short line. Losing it is the
    /// classic editor bug: three presses of down through a short line and the
    /// cursor has crept to that line's end for good.
    #[test]
    fn vertical_moves_keep_their_column() {
        let mut d = Doc::new("aaaaaaaaaa\nbb\ncccccccccc");
        d.place(Pos::new(0, 8), false);
        d.move_down(false);
        assert_eq!(d.cursor(), Pos::new(1, 2), "clamped to the short line");
        d.move_down(false);
        assert_eq!(d.cursor(), Pos::new(2, 8), "and back to the column it had");
        d.move_up(false);
        d.move_up(false);
        assert_eq!(d.cursor(), Pos::new(0, 8));
    }

    #[test]
    fn home_goes_to_the_first_word_then_to_the_margin() {
        let mut d = Doc::new("    select 1");
        d.move_end(false);
        d.move_home(false);
        assert_eq!(d.cursor(), Pos::new(0, 4), "the first non-blank");
        d.move_home(false);
        assert_eq!(d.cursor(), Pos::new(0, 0), "then the margin");
        d.move_home(false);
        assert_eq!(d.cursor(), Pos::new(0, 4), "and back again");
    }

    #[test]
    fn a_selection_has_a_direction_and_can_shrink() {
        let mut d = Doc::new("abcdef");
        d.move_home(false);
        d.move_right(true);
        d.move_right(true);
        assert_eq!(d.selected_text().as_deref(), Some("ab"));
        d.move_left(true);
        assert_eq!(d.selected_text().as_deref(), Some("a"), "shift-left shrinks it");
        d.move_left(true);
        assert_eq!(d.selection(), None, "back to where it started is no selection");
    }

    #[test]
    fn a_selection_spans_lines() {
        let mut d = Doc::new("one\ntwo\nthree");
        d.place(Pos::new(0, 1), false);
        d.place(Pos::new(2, 2), true);
        assert_eq!(d.selected_text().as_deref(), Some("ne\ntwo\nth"));
        d.delete_selection();
        assert_eq!(d.text(), "oree");
        assert_eq!(d.cursor(), Pos::new(0, 1));
    }

    #[test]
    fn select_all_then_type_replaces_everything() {
        let mut d = Doc::new("old\nstatement");
        d.select_all();
        d.insert("new");
        assert_eq!(d.text(), "new");
    }

    #[test]
    fn backspace_joins_lines_and_steps_graphemes() {
        let mut d = Doc::new("ab\ncd");
        d.place(Pos::new(1, 0), false);
        d.backspace();
        assert_eq!(d.text(), "abcd");
        assert_eq!(d.cursor(), Pos::new(0, 2));

        // A multi-byte grapheme goes in ONE press, not four.
        let mut d = Doc::new("прив");
        d.move_end(false);
        d.backspace();
        assert_eq!(d.text(), "при");
        d.backspace();
        assert_eq!(d.text(), "пр");
    }

    #[test]
    fn delete_joins_the_line_below() {
        let mut d = Doc::new("ab\ncd");
        d.place(Pos::new(0, 2), false);
        d.delete();
        assert_eq!(d.text(), "abcd");
        d.place(Pos::new(0, 0), false);
        d.delete();
        assert_eq!(d.text(), "bcd");
    }

    #[test]
    fn enter_carries_the_indent() {
        let mut d = Doc::new("    select 1");
        d.move_end(false);
        d.newline();
        assert_eq!(d.text(), "    select 1\n    ");
        assert_eq!(d.cursor(), Pos::new(1, 4));

        // Pressing Enter INSIDE the indent copies only what is BEHIND it, so
        // the text is split and nothing is added to it: two spaces stay above,
        // the other two go down with the `x` and two more are carried — which
        // leaves the line that holds the `x` indented exactly as it was.
        let mut d = Doc::new("    x");
        d.place(Pos::new(0, 2), false);
        d.newline();
        assert_eq!(d.text(), "  \n    x");
        assert_eq!(d.cursor(), Pos::new(1, 2));
    }

    #[test]
    fn tab_indents_a_block_and_shift_tab_takes_it_back() {
        let mut d = Doc::new("a\nb\nc");
        d.place(Pos::new(0, 0), false);
        d.place(Pos::new(2, 1), true);
        d.tab();
        assert_eq!(d.text(), "  a\n  b\n  c");
        d.untab();
        assert_eq!(d.text(), "a\nb\nc");
        // With no selection it is two spaces at the cursor.
        let mut d = Doc::new("x");
        d.move_home(false);
        d.tab();
        assert_eq!(d.text(), "  x");
    }

    #[test]
    fn a_comment_goes_on_and_comes_off_as_one_decision() {
        let mut d = Doc::new("select 1\n  from t\nwhere x");
        d.place(Pos::new(0, 0), false);
        d.place(Pos::new(2, 0), true);
        d.toggle_comment("--");
        assert_eq!(d.text(), "-- select 1\n  -- from t\n-- where x");
        d.toggle_comment("--");
        assert_eq!(d.text(), "select 1\n  from t\nwhere x");

        // A half-commented block takes the FIRST line's answer, so it does not
        // turn inside out.
        let mut d = Doc::new("-- a\nb");
        d.place(Pos::new(0, 0), false);
        d.place(Pos::new(1, 1), true);
        d.toggle_comment("--");
        assert_eq!(d.text(), "a\nb", "uncommenting, because the first line was");
    }

    #[test]
    fn duplicate_puts_the_copy_below_and_follows_it() {
        let mut d = Doc::new("one\ntwo");
        d.place(Pos::new(0, 1), false);
        d.duplicate_line();
        assert_eq!(d.text(), "one\none\ntwo");
        assert_eq!(d.cursor().line, 1);
    }

    #[test]
    fn undo_and_redo_walk_the_edits() {
        let mut d = Doc::new("a");
        d.move_end(false);
        d.insert("b");
        d.insert("c");
        assert_eq!(d.text(), "abc");
        assert!(d.undo());
        assert_eq!(d.text(), "ab");
        assert!(d.undo());
        assert_eq!(d.text(), "a");
        assert!(!d.undo(), "nothing left to undo");
        assert!(d.redo());
        assert_eq!(d.text(), "ab");
        assert!(d.redo());
        assert_eq!(d.text(), "abc");
        assert!(!d.redo());

        // An EDIT after an undo clears the redo: it described a future that no
        // longer follows from here.
        d.undo();
        d.insert("Z");
        assert!(!d.redo(), "the redo stack was cleared");
    }

    /// Backspace at the very start changes nothing, and must not leave an undo
    /// step behind — ctrl-z would then appear to do nothing.
    #[test]
    fn a_keystroke_that_changes_nothing_leaves_no_history() {
        let mut d = Doc::new("abc");
        d.place(Pos::new(0, 0), false);
        d.backspace();
        assert_eq!(d.text(), "abc");
        assert!(!d.undo(), "no history for a no-op");

        let mut d = Doc::new("abc");
        d.move_end(false);
        d.delete();
        assert_eq!(d.text(), "abc");
        assert!(!d.undo());
    }

    #[test]
    fn set_text_is_a_new_document() {
        let mut d = Doc::new("a");
        d.insert("b");
        d.set_text("something else");
        assert!(!d.undo(), "undo must not reach back into the old statement");
        assert_eq!(d.text(), "something else");
    }

    #[test]
    fn words_are_crossed_one_at_a_time() {
        let mut d = Doc::new("select count(*) from t");
        d.move_end(false);
        d.move_word_left(false);
        assert_eq!(&d.text()[d.cursor().byte..], "t");
        d.move_word_left(false);
        assert_eq!(&d.text()[d.cursor().byte..], "from t");
        d.move_word_left(false);
        assert_eq!(&d.text()[d.cursor().byte..], "count(*) from t");
        d.move_home(false);
        d.move_word_right(false);
        assert_eq!(&d.text()[d.cursor().byte..], "count(*) from t");
    }

    /// A position from somewhere else — a click, a restored session — is
    /// brought inside the text and onto a grapheme boundary.
    #[test]
    fn a_stray_position_is_repaired() {
        let d = Doc::new("при\nx");
        assert_eq!(d.clamp(Pos::new(99, 99)), Pos::new(1, 1));
        // Byte 1 is inside `п`: it comes back to 0.
        assert_eq!(d.clamp(Pos::new(0, 1)), Pos::new(0, 0));
        assert_eq!(d.clamp(Pos::new(0, 2)), Pos::new(0, 2));
        assert_eq!(d.clamp(Pos::new(0, 7)), Pos::new(0, 6), "past the end of the line");
    }

    /// Paging moves by lines and stops at the ends rather than wrapping.
    #[test]
    fn paging_stops_at_the_ends() {
        let mut d = Doc::new("1\n2\n3\n4\n5");
        d.place(Pos::new(2, 0), false);
        d.move_page(-10, false);
        assert_eq!(d.cursor().line, 0);
        d.move_page(10, false);
        assert_eq!(d.cursor().line, 4);
    }
}
