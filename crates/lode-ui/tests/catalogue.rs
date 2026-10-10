//! Does the catalogue match the windows?
//!
//! Two faults, and each has happened. A STRING WRITTEN AT THE CALL SITE never
//! reaches the catalogue, so a window in Bulgarian shows an English button and
//! nobody notices until somebody reads that panel; and a KEY NOBODY USES is a
//! line that two translations have to carry for ever, which is how a catalogue
//! grows a third of its weight in words no screen can show.
//!
//! `i18n.rs` already asserts that the languages agree with each other. This
//! asks the other question: do they agree with the SOURCE.
//!
//! WHERE THE LINE IS, because it decides what counts as a fault here. The
//! client translates its own chrome. The ENGINE's words — a refusal, a driver's
//! error, the audit log — stay in English deliberately: they are the record,
//! and a log whose meaning depends on the locale of whoever ran it cannot be
//! read by the person who did not. So a literal in the source is not
//! automatically wrong; a literal in a BUTTON is.

use std::path::{Path, PathBuf};

fn src(name: &str) -> String {
    let p: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name);
    std::fs::read_to_string(p).unwrap_or_default()
}

/// Every key in the English table, read out of the source rather than linked
/// against — this is an integration test and the crate is a binary.
fn keys() -> Vec<String> {
    let text = src("i18n.rs");
    let mut out = Vec::new();
    let mut in_en = false;
    for line in text.lines() {
        if line.contains("const EN:") {
            in_en = true;
            continue;
        }
        if in_en && line.starts_with("];") {
            break;
        }
        if !in_en {
            continue;
        }
        let t = line.trim();
        if !t.starts_with("(\"") {
            continue;
        }
        if let Some(rest) = t.strip_prefix("(\"") {
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_string());
            }
        }
    }
    out
}

/// The files that draw, plus `workspace.rs` — which draws nothing, but names
/// catalogue keys in the pure functions that DECIDE what a view shows (the
/// padlock's tooltip, for one). A key reached through a variable cannot be
/// grepped at the call site, so the place that names it is the place that
/// counts.
///
/// `i18n.rs` itself is not here: that is where a key is defined, and a mention
/// there is not a use.
const VIEWS: [&str; 8] = [
    "app.rs",
    "form.rs",
    "settings.rs",
    "profile.rs",
    "board.rs",
    "export.rs",
    "workspace.rs",
    // Same reason as `workspace.rs`: it draws nothing and names the keys in the
    // pure function that decides what a menu offers.
    "menu.rs",
];

/// A key defined and never asked for.
///
/// It is not pedantry. The settings panel became a window and took its fold
/// headings with it; the keys those headings used stayed behind, and two
/// languages went on carrying them.
#[test]
fn every_key_is_used_somewhere() {
    let all: String = VIEWS.iter().map(|f| src(f)).collect::<Vec<_>>().join("\n");
    let unused: Vec<String> = keys()
        .into_iter()
        .filter(|k| !all.contains(&format!("\"{k}\"")))
        .collect();
    assert!(
        unused.is_empty(),
        "{} catalogue key(s) that no window asks for. Use them or take them out — \
         every one is a line two languages maintain:\n  {}",
        unused.len(),
        unused.join("\n  ")
    );
}

/// A key asked for that is not in the catalogue.
///
/// `Lang::tr` leaks the key itself when it cannot find it, so the window shows
/// `panel.whatever` where a word should be — visible, but only to whoever opens
/// that panel in that language.
#[test]
fn every_key_asked_for_exists() {
    let defined = keys();
    let mut missing = Vec::new();
    for file in VIEWS {
        let text = src(file);
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            // `tr("…")` in any of its spellings: `self.tr`, `self.lang.tr`,
            // `w.lang.tr`.
            while let Some(at) = rest.find(".tr(\"") {
                let after = &rest[at + 5..];
                let Some(end) = after.find('"') else { break };
                let key = &after[..end];
                if !defined.iter().any(|d| d == key) {
                    missing.push(format!("{file}:{}  {key}", n + 1));
                }
                rest = &after[end..];
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} key(s) asked for and not in the catalogue:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

/// The reader itself, so a test that finds nothing is not mistaken for one that
/// works.
#[test]
fn the_catalogue_can_be_read_at_all() {
    let k = keys();
    assert!(k.len() > 80, "only {} keys found — the reader is broken", k.len());
    assert!(k.iter().any(|x| x == "panel.connections"), "{k:?}");
    assert!(k.iter().any(|x| x == "tip.edit_conn"));
    // No duplicates in what was read, or the counts above mean nothing.
    let mut sorted = k.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), k.len(), "the reader saw a key twice");
}
