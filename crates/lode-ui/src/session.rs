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
    /// Which panel the rail was showing — "connections" | "instance" |
    /// "history" | "audit" | "tasks" | "settings". Lower case, so the file
    /// reads as a person would write it.
    #[serde(default)]
    pub rail: String,
    /// The chosen language's tag, when somebody has chosen one. `None` means
    /// "whatever the system says", so a machine that changes locale follows it
    /// — a tag written out on first run would freeze the answer.
    #[serde(default)]
    pub lang: Option<String>,
    /// How many spaces the JSON view indents by. `None` is the default.
    #[serde(default)]
    pub json_indent: Option<u8>,
    /// The panel's width, when somebody has dragged one. `None` means derived
    /// from the text size, so it follows the scale until it is overridden.
    #[serde(default)]
    pub panel_w: Option<f32>,
    /// Which settings groups were left open.
    ///
    /// By NAME, not by index: a group inserted in the middle of the list would
    /// otherwise silently open somebody else's groups.
    #[serde(default)]
    pub settings_open: Option<Vec<String>>,
    /// The panel, folded away. `None` means it is open, which is the default
    /// and what every session written before this field says.
    #[serde(default)]
    pub panel_hidden: Option<bool>,
    /// The icon rail's size, when somebody has set one. Its OWN setting: the
    /// rail is navigation and the text is content, and enlarging one is not
    /// asking for the other.
    #[serde(default)]
    pub icon_size: Option<f32>,
    /// The base of the type scale, in pixels, when somebody has set one.
    ///
    /// `None` means "whatever the application's default is", so raising the
    /// default later reaches everyone who never touched it — which a number
    /// written out on first run would not.
    #[serde(default)]
    pub font_size: Option<f32>,
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

/// Write the session out.
///
/// To a TEMPORARY file first, then rename it over the real one. A rename within
/// a directory is atomic, so a reader sees either the old file or the new one
/// and never a half-written one — and this is written on every tab change, on
/// every theme change and on quit, which is exactly the shape that gets caught
/// by a crash or a power cut partway through. A truncated `session.json` does
/// not parse, and a session that does not parse is every open tab lost.
pub fn save(s: &Session) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let Ok(text) = serde_json::to_string_pretty(s) else { return };
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_err() {
        return;
    }
    // If the rename fails the temporary file is left behind rather than the
    // real one being removed: the old session is still better than none.
    if std::fs::rename(&tmp, &p).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `save` leaves on disk PARSES.
    ///
    /// The write goes to a temporary file and is renamed over the real one, so
    /// this also checks the rename happened: without it the real file would not
    /// exist at all and the read below would find nothing.
    #[test]
    fn what_save_writes_can_be_read_back() {
        let dir = std::env::temp_dir().join(format!("lode-session-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("LODE_DATA_DIR", &dir);

        let mut s = Session::default();
        s.theme = Some("LvimBase_dark".into());
        s.active = 2;
        save(&s);

        let p = path().expect("a path, with LODE_DATA_DIR set");
        let text = std::fs::read_to_string(&p).expect("the file is there");
        let back: Session = serde_json::from_str(&text).expect("and it parses");
        assert_eq!(back.theme.as_deref(), Some("LvimBase_dark"));
        assert_eq!(back.active, 2);
        // The temporary file does not outlive the write.
        assert!(!p.with_extension("json.tmp").exists(), "the .tmp was left behind");

        std::env::remove_var("LODE_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An old session's `settings_open` still PARSES. Nothing writes it since
    /// the settings panel became a window, and a field that stops being written
    /// must not stop being readable — a session file that fails to parse is
    /// every open tab lost.
    #[test]
    fn the_open_settings_groups_round_trip_by_name() {
        let mut s = Session::default();
        s.settings_open = Some(vec!["theme".into(), "language".into()]);
        let text = serde_json::to_string(&s).expect("writes");
        let back: Session = serde_json::from_str(&text).expect("reads");
        assert_eq!(
            back.settings_open.as_deref(),
            Some(&["theme".to_string(), "language".to_string()][..])
        );

        // A session written before groups existed opens the default one rather
        // than none: a settings panel with everything shut looks broken.
        let old: Session = serde_json::from_str("{}").expect("an empty session reads");
        assert_eq!(old.settings_open, None, "absent, so the caller picks the default");
    }

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
            font_size: Some(16.0),
            json_indent: Some(4),
            lang: Some("bg".into()),
            icon_size: Some(24.0),
            settings_open: Some(vec!["theme".into()]),
            panel_hidden: None,
            panel_w: Some(320.0),
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
        assert_eq!(back.font_size, Some(16.0), "a chosen text size comes back");
        assert_eq!(back.json_indent, Some(4));
        assert_eq!(back.lang.as_deref(), Some("bg"));
        assert_eq!(back.icon_size, Some(24.0));
        assert_eq!(back.settings_open.as_deref(), Some(&["theme".to_string()][..]));
        assert_eq!(back.panel_w, Some(320.0));

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
            font_size: None,
            json_indent: None,
            lang: None,
            icon_size: None,
            settings_open: None,
            panel_hidden: None,
            panel_w: None,
        };
        let round: Session = serde_json::from_str(&serde_json::to_string(&typed).unwrap()).unwrap();
        assert_eq!(round.tabs[0].draft, "SELECT 1 WHERE", "half a statement is still someone's work");
        // Unset stays unset, so raising the default later reaches everyone who
        // never chose a size — which a number written out on first run would not.
        assert!(round.font_size.is_none(), "a size nobody chose must not be written as a choice");
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
