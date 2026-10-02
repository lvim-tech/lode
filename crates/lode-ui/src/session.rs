//! What was open last time.
//!
//! A database client that forgets everything when it closes makes you rebuild
//! your context every morning, and the context is the expensive part: which
//! connections, which objects, which view you were reading them in.
//!
//! Deliberately NOT in the engine's store. That store is SHARED with the Neovim
//! client, and "which tabs were open" is a fact about one window on one
//! machine, not about the connections themselves. Putting it there would mean
//! the editor and the desktop fighting over each other's layout.
//!
//! Written on every change and read once at start. A missing, unreadable or
//! half-written file is not an error: the session is a convenience, and the
//! application has to open on a machine that has never run it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    /// Connections that were open, by name. Reconnected on start.
    #[serde(default)]
    pub connections: Vec<String>,
    /// Tabs that were open, in order.
    #[serde(default)]
    pub tabs: Vec<SessionTab>,
    #[serde(default)]
    pub active: usize,
    /// The theme that was chosen, so it survives a restart without an env var.
    #[serde(default)]
    pub theme: Option<String>,
    /// Which panel the rail was showing — "connections" | "history" | "tasks" |
    /// "settings". Lower case, so the file reads as a person would write it.
    #[serde(default)]
    pub rail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    pub conn: String,
    /// The object this tab was opened on. Empty for a tab that was typed rather
    /// than clicked — those are kept too, because an unsent statement someone
    /// was in the middle of writing is the LAST thing a restart should throw
    /// away.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub kind: String,
    /// "tree" | "table" | "json" — lower case, so the file is readable by a
    /// person and stable if the enum is reordered.
    #[serde(default)]
    pub view: String,
    /// What the tab is called, for a typed tab that has no object to name it.
    #[serde(default)]
    pub title: String,
    /// The statement in the box, run or not.
    #[serde(default)]
    pub draft: String,
    /// Which sub-tab was showing — "documents" | "schema" | …
    #[serde(default)]
    pub sub: String,
    /// The row that was open in the detail panel. Coming back to a table and
    /// having to find your row again is the kind of small loss that makes a
    /// restart feel expensive.
    #[serde(default)]
    pub detail: Option<usize>,
}

/// Where the session lives: beside the engine's data, but its own file.
pub fn path() -> Option<PathBuf> {
    let dir = if let Some(d) = std::env::var_os("LODE_DATA_DIR") {
        PathBuf::from(d)
    } else if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(d).join("lode")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/share/lode")
    };
    Some(dir.join("session.json"))
}

pub fn load() -> Session {
    let Some(p) = path() else { return Session::default() };
    let Ok(text) = std::fs::read_to_string(p) else { return Session::default() };
    // A half-written file is a session not worth having, not a crash.
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(s: &Session) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    if let Ok(text) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(p, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_broken_session_is_simply_empty() {
        // The application must open on a machine that has never run it, and on
        // one where the file was truncated by a crash mid-write.
        let empty: Session = serde_json::from_str("{}").unwrap();
        assert!(empty.connections.is_empty());
        assert!(empty.tabs.is_empty());
        let broken: Session = serde_json::from_str("{ this is not json").unwrap_or_default();
        assert!(broken.tabs.is_empty());
    }

    #[test]
    fn a_session_round_trips() {
        let s = Session {
            connections: vec!["demo".into()],
            tabs: vec![SessionTab {
                conn: "demo".into(),
                name: "people".into(),
                schema: Some("main".into()),
                kind: "table".into(),
                view: "table".into(),
                title: String::new(),
                draft: String::new(),
                sub: "schema".into(),
                detail: Some(2),
            }],
            active: 0,
            theme: Some("LvimBase_dark".into()),
            rail: "history".into(),
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&text).unwrap();
        assert_eq!(back.connections, vec!["demo".to_string()]);
        assert_eq!(back.tabs.len(), 1);
        assert_eq!(back.tabs[0].name, "people");
        assert_eq!(back.tabs[0].schema.as_deref(), Some("main"));
        assert_eq!(back.theme.as_deref(), Some("LvimBase_dark"));
        assert_eq!(back.rail, "history");
        assert_eq!(back.tabs[0].sub, "schema");
        assert_eq!(back.tabs[0].detail, Some(2));

        // A tab with no object is a typed one, and its words come back.
        let typed = Session {
            connections: vec!["demo".into()],
            tabs: vec![SessionTab {
                conn: "demo".into(),
                name: String::new(),
                schema: None,
                kind: String::new(),
                view: "table".into(),
                title: "Draft".into(),
                draft: "SELECT 1 WHERE".into(),
                sub: String::new(),
                detail: None,
            }],
            active: 0,
            theme: None,
            rail: String::new(),
        };
        let round: Session = serde_json::from_str(&serde_json::to_string(&typed).unwrap()).unwrap();
        assert_eq!(round.tabs[0].draft, "SELECT 1 WHERE", "half a statement is still someone's work");
        assert!(round.tabs[0].name.is_empty());
    }

    /// A tab from a version that did not record the view must still load.
    #[test]
    fn an_older_session_file_still_loads() {
        let s: Session =
            serde_json::from_str(r#"{"connections":["c"],"tabs":[{"conn":"c","name":"t"}]}"#).unwrap();
        assert_eq!(s.tabs.len(), 1);
        assert!(s.tabs[0].schema.is_none());
        assert!(s.tabs[0].view.is_empty());
    }
}
