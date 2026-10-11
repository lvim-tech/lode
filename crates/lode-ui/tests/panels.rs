//! Every side panel draws its OWN list and stops.
//!
//! This has been got wrong twice. Settings once fell through to the
//! connections path and ended up with two scrolling lists sharing one height;
//! Scripts did the same thing the day it was written, and the whole connection
//! tree — filter, "New connection", the schema, the dashboards — was drawn
//! underneath a list of scripts.
//!
//! There WAS a diagnostic for it: `lode --check-panels` prints "panels that
//! return their own list: …". It did not catch either one, because it prints a
//! sentence a person has to compare against the code by eye, and the sentence
//! was written by the same hand that forgot the `return`. A list somebody
//! maintains is not a check.
//!
//! So this reads the match itself.

use std::path::PathBuf;

fn app_rs() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src/app.rs");
    p
}

/// The arms of `match self.rail` inside the panel, as `(name, body)`.
///
/// Brace counting, which is the thing a regular expression cannot do: an arm's
/// body has braces of its own, and the last one before the next `Rail::` is
/// what has to be looked at.
///
/// It counts only braces that are CODE. `app.rs` is full of `"{qualified}"`,
/// `format!("{a}")` and `'}'`, and a counter that believed those found one arm
/// and declared victory — which is how a walker comes to agree with whatever
/// it is pointed at.
fn rail_arms(text: &str) -> Vec<(String, String)> {
    // The match inside `panel_el`, not the one in the session writer, which
    // maps the same enum to words and has no braces to count. Searching for
    // the first `match self.rail` found that one and answered zero arms —
    // which would have passed as "nothing to check" if the count were not
    // asserted.
    let from = text.find("fn panel_el").unwrap_or(0);
    let Some(start) = text[from..].find("match self.rail {").map(|i| i + from) else {
        return Vec::new();
    };
    let body: Vec<char> = text[start + "match self.rail {".len()..].chars().collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut depth: i32 = 0;
    let mut arm: Option<(String, usize)> = None;
    let mut i = 0usize;
    while i < body.len() {
        let c = body[i];
        // ── what is not code ────────────────────────────────────────────
        if c == '/' && body.get(i + 1) == Some(&'/') {
            while i < body.len() && body[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && body.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < body.len() && !(body[i] == '*' && body[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c == '"' {
            i += 1;
            while i < body.len() {
                if body[i] == '\\' {
                    i += 2;
                    continue;
                }
                if body[i] == '"' {
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        // A char literal, which in this file is only ever `'x'` or `'\n'`.
        // A LIFETIME (`&'static`) looks the same at the start and is not one,
        // so it is only treated as a literal when it closes within three.
        if c == '\'' {
            let close = (1..=4).find(|k| body.get(i + k) == Some(&'\''));
            if let Some(k) = close {
                i += k + 1;
                continue;
            }
        }
        // ── the arms ────────────────────────────────────────────────────
        if depth == 0 && arm.is_none() && starts_with(&body, i, "Rail::") {
            let name: String =
                body[i + 6..].iter().take_while(|c| c.is_alphanumeric()).collect();
            // The `{` that opens this arm.
            let mut j = i + 6 + name.len();
            while j < body.len() && body[j] != '{' && body[j] != ',' {
                j += 1;
            }
            if body.get(j) == Some(&'{') {
                arm = Some((name, j + 1));
                depth = 1;
                i = j + 1;
                continue;
            }
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    match arm.take() {
                        Some((name, from)) => {
                            out.push((name, body[from..i].iter().collect()));
                        }
                        // The match itself has closed.
                        None => break,
                    }
                } else if depth < 0 {
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn starts_with(chars: &[char], at: usize, needle: &str) -> bool {
    needle.chars().enumerate().all(|(k, c)| chars.get(at + k) == Some(&c))
}

/// A panel that does not `return` falls through into the one written after it.
#[test]
fn every_panel_returns_its_own_list() {
    let text = std::fs::read_to_string(app_rs()).expect("app.rs is where the panel lives");
    let arms = rail_arms(&text);
    assert!(
        arms.len() >= 5,
        "only {} rail arm(s) found — the walker has lost the match it reads",
        arms.len()
    );
    let mut fell: Vec<String> = Vec::new();
    let mut empty = 0;
    for (name, body) in &arms {
        let lines: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .collect();
        // An EMPTY arm is the one that means "this is the default panel, which
        // is the code after the match". It draws nothing, so it cannot draw
        // half of something — `Rail::Connections => {}` is deliberate and is
        // the only shape allowed to fall through.
        if lines.is_empty() {
            empty += 1;
            continue;
        }
        // Every other arm's last act must be to hand its own panel back.
        let last = lines.last().copied().unwrap_or("");
        if !last.starts_with("return ") {
            fell.push(format!("Rail::{name} ends with `{last}`"));
        }
    }
    assert!(
        empty <= 1,
        "{empty} empty rail arms — only ONE panel can be the default that the \
         code after the match draws"
    );
    assert!(
        fell.is_empty(),
        "{} panel(s) fall through into the one written after them, and the \
         connection tree is drawn underneath:\n  {}",
        fell.len(),
        fell.join("\n  ")
    );
}

/// The walker reads what is there, not what it hopes is there.
#[test]
fn the_walker_can_find_an_arm_and_its_end() {
    let fake = "fn x() { match self.rail { \
        Rail::A => { let v = f(|x| { x }); return p.child(v); } \
        Rail::B => { p.child(q) } } }";
    let arms = rail_arms(fake);
    assert_eq!(arms.len(), 2, "{arms:?}");
    assert_eq!(arms[0].0, "A");
    assert!(arms[0].1.contains("return p.child(v);"), "{:?}", arms[0].1);
    // The nested closure's braces did not end the arm.
    assert!(arms[0].1.contains("f(|x| { x })"), "{:?}", arms[0].1);
    assert_eq!(arms[1].0, "B");
    assert!(!arms[1].1.contains("return"), "{:?}", arms[1].1);
}
