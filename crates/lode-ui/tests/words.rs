//! Every word somebody reads comes out of the catalogue.
//!
//! Sixty-six did not. They were correct English and they stayed English in a
//! Bulgarian window, which reads as a translation abandoned halfway — and the
//! reason there were sixty-six is that nothing was counting. A sweep fixes the
//! ones that exist today; this is what stops the next one.
//!
//! It reads the places a literal becomes something a person reads: the label of
//! a button or a chip, a heading, a note, a row's name, a field's placeholder,
//! and the text of a `div`. What it cannot see is a literal built by `format!`
//! out of data, which is the right thing for it not to see.
//!
//! Where the line is: a WORD is in the catalogue; a NAME is not. A driver's own
//! label, a theme's filename, a font's family, a keystroke and a column's name
//! are names — they come from somewhere else and translating them would be
//! this client inventing a fact about something it did not name.

use std::path::PathBuf;

/// Where a literal becomes visible text: the call, and WHICH argument.
///
/// `btn` and `chip` take the theme, the metrics and an id before the label,
/// so the label is the fourth; everything else puts it first.
const SINKS: &[(&str, usize, &str)] = &[
    (".child", 0, "child"),
    ("btn", 3, "btn"),
    ("chip", 3, "chip"),
    ("self.small", 0, "small"),
    ("self.note", 0, "note"),
    ("self.section", 0, "section"),
    ("self.head", 0, "head"),
    ("TextInput::new", 1, "placeholder"),
    // A TOAST AND A STATUS LINE ARE READ TOO.
    //
    // They were not checked, and eighty of them had been written out in
    // English — "pick a row first", "cell copied", "nothing connected". The
    // window around them was translated, which made them worse than if
    // nothing had been: a sentence in the wrong language where everything
    // else is right reads as a bug in the program rather than as a gap in
    // the catalogue.
    ("self.toast", 0, "toast"),
    ("self.status =", 0, "status"),
];

/// The files that draw.
const DRAWING: [&str; 4] = ["app.rs", "form.rs", "settings.rs", "profile.rs"];

/// Literals that are deliberately not words.
///
/// Each one is here for a reason, and the reason is the same: it is an
/// EXAMPLE or a NAME, not a phrase. A placeholder showing `result.csv` is
/// showing a filename; one showing `ctrl-shift-k` is showing a keystroke; a
/// date in a date box is showing the shape of a date. Translating any of them
/// would be worse than leaving them.
const NOT_WORDS: &[&str] = &[
    "result.csv",
    "ctrl-shift-k",
    "2026-01-01",
    "2026-02-01",
    "bastion.example",
    "socks.example",
    "name this tunnel",
    "name this proxy",
    "user",
    "22",
    "1080",
    "~/.ssh/id_ed25519",
    "prod/db",
];

fn src_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p
}

/// Does this literal look like a phrase a person reads?
///
/// Two letters in a row and a letter at the start. That excludes ids
/// (`sc-out-{tab_id}`), glyphs, numbers and punctuation without excluding
/// anything short that is genuinely a word — `key` and `oid` are both three
/// characters and only one of them is a word, which is why the list above
/// exists rather than a cleverer rule.
fn looks_like_a_phrase(s: &str) -> bool {
    if s.len() < 3 || s.starts_with('\\') {
        return false;
    }
    let first = s.chars().next().unwrap_or(' ');
    if !first.is_alphabetic() {
        return false;
    }
    // An id is lower case with dashes and no spaces: `tab-confirm-run`.
    if s.contains('-') && !s.contains(' ') {
        return false;
    }
    // A format placeholder on its own is data.
    if s.starts_with('{') {
        return false;
    }
    s.chars().filter(|c| c.is_alphabetic()).count() >= 3
}

#[test]
fn every_word_somebody_reads_is_in_the_catalogue() {
    let mut stray: Vec<String> = Vec::new();
    for f in DRAWING {
        let Ok(text) = std::fs::read_to_string(src_dir().join(f)) else { continue };
        // The test module at the end is not a view.
        let code = text.split("\n#[cfg(test)]").next().unwrap_or(&text);
        // ONE CALL AT A TIME, not one line at a time.
        //
        // A call that rustfmt broke over three lines was invisible to this:
        // the sink was on one line and its literal on the next, so the whole
        // of `self.small(\n    "a chart needs …",\n    t.muted(),\n)` went
        // unseen. Comments go first, then every run of whitespace becomes one
        // space, and a line number is kept for each byte so a finding still
        // says where it is.
        let (flat, lines) = flatten(code);
        for (head, nth, kind) in SINKS {
            let mut from = 0usize;
            while let Some(at) = flat[from..].find(head) {
                let at = from + at;
                from = at + head.len();
                let Some(found) = literal_arg(&flat[at..], head, *nth) else { continue };
                if !looks_like_a_phrase(&found) || NOT_WORDS.contains(&found.as_str()) {
                    continue;
                }
                stray.push(format!("{f}:{}  {kind}  {found:?}", lines[at]));
            }
        }
    }
    assert!(
        stray.is_empty(),
        "{} word(s) written out instead of taken from the catalogue.\n\
         A window in Bulgarian showing English is a translation abandoned halfway.\n\
         Add a key to `i18n.rs` and use `tr`, or add it to NOT_WORDS with a reason:\n  {}",
        stray.len(),
        stray.join("\n  ")
    );
}

/// The code with its comments gone and its whitespace squeezed, plus the
/// line each byte came from.
///
/// Strings are left alone — a literal with two spaces in it is a literal with
/// two spaces in it — which also means a `//` inside a string is not a
/// comment, and that is the one thing a naive version of this gets wrong.
fn flatten(code: &str) -> (String, Vec<usize>) {
    let (mut out, mut lines) = (String::with_capacity(code.len()), Vec::new());
    let mut line = 1usize;
    let mut chars = code.char_indices().peekable();
    let (mut in_str, mut in_comment, mut escaped, mut last_space) = (false, false, false, false);
    while let Some((_, c)) = chars.next() {
        if c == '\n' {
            line += 1;
            in_comment = false;
            if !in_str {
                if !last_space && !out.is_empty() {
                    out.push(' ');
                    lines.push(line);
                    last_space = true;
                }
                continue;
            }
        }
        if in_comment {
            continue;
        }
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            out.push(c);
            lines.push(line);
            last_space = false;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            lines.push(line);
            last_space = false;
            continue;
        }
        if c == '/' && matches!(chars.peek(), Some((_, '/'))) {
            in_comment = true;
            continue;
        }
        if c.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
                lines.push(line);
                last_space = true;
            }
            continue;
        }
        out.push(c);
        lines.push(line);
        last_space = false;
    }
    (out, lines)
}

/// The literal at argument `nth` of a call to `head`, when it is one.
///
/// Written out rather than pulled in: one test is not worth a regex crate in
/// the build, and what is needed is narrow — find a call, walk its arguments
/// respecting nesting, and look at one of them.
///
/// It is TESTED below, because a matcher that finds nothing makes the
/// guardrail above pass by saying nothing — which is what the first version
/// of it did.
fn literal_arg(line: &str, head: &str, nth: usize) -> Option<String> {
    let at = line.find(head)?;
    let rest = &line[at + head.len()..];
    let rest = rest.strip_prefix('(')?;
    let mut depth = 0i32;
    let mut arg = 0usize;
    let mut from = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            // A comma inside a nested call belongs to that call.
            ',' if depth == 0 => {
                if arg == nth {
                    break;
                }
                arg += 1;
                from = i + 1;
            }
            _ => {}
        }
    }
    if arg != nth {
        return None;
    }
    let tail = rest[from..].trim_start();
    let quoted = tail.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_string())
}

/// The matcher reads what is there.
#[test]
fn the_matcher_can_tell_a_word_from_an_id() {
    assert!(looks_like_a_phrase("Save the connection"));
    assert!(looks_like_a_phrase("key"));
    assert!(!looks_like_a_phrase("tab-confirm-run"), "an id is not a word");
    assert!(!looks_like_a_phrase("{name}"), "a placeholder is data");
    assert!(!looks_like_a_phrase("12"), "a number is not a word");
    assert!(!looks_like_a_phrase("\\u{f084}"), "a glyph is not a word");
    assert!(!looks_like_a_phrase("ok"), "two letters is not enough to be sure");

    // The argument it is pointed at, and nothing else.
    assert_eq!(
        literal_arg(r#"out.child(self.small("not loaded", t.muted()))"#, "self.small", 0),
        Some("not loaded".to_string())
    );
    // A call that already goes through the catalogue has no literal there.
    assert_eq!(
        literal_arg(r#"out.child(self.small(self.tr("ui.x"), t.muted()))"#, "self.small", 0),
        None
    );
    // The FOURTH argument of a button, past an id that is itself a literal —
    // the thing that makes a naive match report every id as a word.
    assert_eq!(
        literal_arg(r#"btn(t, ms, "vault-unlock", "Unlock", Btn::Primary)"#, "btn", 3),
        Some("Unlock".to_string())
    );
    // And a nested call's commas do not count as arguments.
    assert_eq!(
        literal_arg(r#"btn(t, ms, format!("f-{a}-{b}"), "Save", Btn::Primary)"#, "btn", 3),
        Some("Save".to_string())
    );
}
