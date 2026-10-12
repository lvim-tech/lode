//! A window the workspace pushes into must not push back.
//!
//! The workspace hands an engine reply to another window by updating that
//! window's entity from INSIDE its own update:
//!
//!     let _ = form.update(cx, |f, cx| f.wallet_opened(field, cx));
//!
//! If the method on the other end then asks the workspace for something —
//! `self.workspace.update(…)` — gpui refuses, and it refuses by PANICKING:
//! "cannot update lode::app::Workspace while it is already being updated".
//! The whole client dies.
//!
//! That is not a theory. `wallet_opened` called `to_keyring`, which asks the
//! workspace to store the secret, so unlocking the wallet from the connection
//! window killed the program on the one keystroke that was meant to make it
//! work. The `let _ =` in front of every such call hides a Result, not a
//! panic, so nothing warned.
//!
//! The way back to the workspace is `cx.defer` — after the update, when the
//! workspace has been handed back to the app. So this reads the source: every
//! method the workspace pushes into, and whether anything it calls (directly,
//! or one call further down) reaches back.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

fn src(name: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("src");
    p.push(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Whitespace squeezed to single spaces, so a call split over four lines by
/// rustfmt reads as one call.
fn flat(code: &str) -> String {
    code.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The methods the workspace calls on ANOTHER entity, from inside its own
/// update: the `f.wallet_opened` of `form.update(cx, |f, cx| f.wallet_opened(…))`.
fn pushed_into(code: &str) -> BTreeSet<String> {
    const OPEN: &str = ".update(cx, |";
    let flat = flat(code);
    let mut rest = flat.as_str();
    let mut out = BTreeSet::new();
    while let Some(i) = rest.find(OPEN) {
        rest = &rest[i + OPEN.len()..];
        // The closure's parameters end at the next `|`; the call is what follows.
        let Some(bar) = rest.find('|') else { break };
        let after = rest[bar + 1..].trim_start();
        if let Some(dot) = after.find('.') {
            let name: String = after[dot + 1..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                out.insert(name);
            }
        }
    }
    out
}

/// Every `fn name` in a file with its body, by counting braces — which is the
/// thing a search cannot do, because a body has braces of its own.
fn bodies(code: &str) -> BTreeMap<String, String> {
    let bytes = code.as_bytes();
    let mut out = BTreeMap::new();
    let mut at = 0usize;
    while let Some(i) = code[at..].find("fn ") {
        let start = at + i;
        at = start + 3;
        // `fn` has to be a word of its own.
        if start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            continue;
        }
        let name: String = code[at..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        let Some(brace) = code[at..].find('{') else { break };
        let mut depth = 0i32;
        let mut end = at + brace;
        for (k, ch) in code[at + brace..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = at + brace + k;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.insert(name, code[at + brace..=end.min(code.len() - 1)].to_string());
        at = end;
    }
    out
}

/// A body with every `cx.defer(…)` cut out of it: what is deferred runs AFTER
/// the update, which is exactly the way back to the workspace.
fn without_deferred(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find("cx.defer") {
        out.push_str(&rest[..i]);
        let Some(open) = rest[i..].find('(') else { break };
        let mut depth = 0i32;
        let mut end = None;
        for (k, ch) in rest[i + open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + open + k);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => rest = &rest[e..],
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// The methods in `file` that reach the workspace — directly, or through
/// another method of the same file, however many steps down.
fn reach_the_workspace(code: &str) -> BTreeSet<String> {
    let bodies = bodies(code);
    let open: BTreeMap<String, String> = bodies
        .iter()
        .map(|(k, v)| (k.clone(), flat(&without_deferred(v))))
        .collect();
    let mut reaching: BTreeSet<String> = open
        .iter()
        .filter(|(_, body)| body.contains("workspace.update("))
        .map(|(name, _)| name.clone())
        .collect();
    loop {
        let mut grew = false;
        for (name, body) in &open {
            if reaching.contains(name) {
                continue;
            }
            if reaching.iter().any(|r| body.contains(&format!(".{r}("))) {
                reaching.insert(name.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    reaching
}

#[test]
fn nothing_the_workspace_pushes_into_pushes_back() {
    let pushed = pushed_into(&src("app.rs"));
    assert!(
        pushed.contains("wallet_opened"),
        "the scan found no pushed methods at all, so it is reading the wrong thing"
    );
    let mut sins = Vec::new();
    for window in ["form.rs", "settings.rs", "profile.rs", "board.rs", "metrics.rs"] {
        let code = src(window);
        let reaching = reach_the_workspace(&code);
        for name in pushed.intersection(&reaching) {
            sins.push(format!("{window}: {name} asks the workspace back"));
        }
    }
    assert!(
        sins.is_empty(),
        "the workspace calls these from inside its own update, and gpui PANICS \
         when they call it back — defer the way back with cx.defer:\n  {}",
        sins.join("\n  ")
    );
}

#[test]
fn the_deferred_way_back_is_not_counted() {
    // The shape `wallet_opened` has now: the call to the workspace happens
    // after the update, so it is not a re-entry.
    let deferred = r#"{ let me = cx.entity().downgrade();
        cx.defer(move |cx| { let _ = me.update(cx, |f, cx| f.to_keyring(&k, cx)); }); }"#;
    assert!(
        !without_deferred(deferred).contains("to_keyring"),
        "a deferred call must not be read as a call made during the update"
    );
}

#[test]
fn a_call_one_step_down_is_still_a_call() {
    // What was missed: `wallet_opened` did not touch the workspace itself.
    let code = r#"
        impl ConnectionForm {
            fn to_keyring(&mut self, key: &str, cx: &mut Context<Self>) {
                let _ = self.workspace.update(cx, |w, _| w.vault_put(key));
            }
            pub fn wallet_opened(&mut self, cx: &mut Context<Self>) {
                self.to_keyring("auth-password", cx);
            }
        }
    "#;
    let reaching = reach_the_workspace(code);
    assert!(reaching.contains("to_keyring"), "the direct call is the obvious one");
    assert!(
        reaching.contains("wallet_opened"),
        "and the one that goes through it is the one that killed the program"
    );
}
