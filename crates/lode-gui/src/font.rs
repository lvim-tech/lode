//! Fonts, and the glyphs that depend on them.
//!
//! egui ships a perfectly good built-in font and the application looked like a
//! demo of egui because of it. A database client is read for hours: the grid and
//! the editor want the monospace the person already chose for their terminal,
//! and the chrome wants a proportional face so a label is not competing with the
//! data for the same rhythm.
//!
//! Nothing here is hardcoded to one machine. Each face is looked for in order:
//! an explicit `$LODE_FONT_MONO` / `$LODE_FONT_UI`, then well-known file names
//! under the standard font roots, then a bounded search of those roots. If none
//! of it lands, egui's built-in font is used and the application is plain but
//! correct — a missing font is never a reason to fail to start.
//!
//! **Icons are gated on finding a Nerd Font**, because a glyph from a font that
//! is not there is an empty box, and an empty box is worse than the word it
//! replaced. `Icons::for_fonts` hands back the glyph set or the word set, and
//! every call site uses whichever it got.

use std::path::{Path, PathBuf};

/// What was actually installed.
pub struct Fonts {
    /// A Nerd Font was found for monospace, so icon glyphs will render.
    pub nerd: bool,
    pub mono: Option<PathBuf>,
    pub ui: Option<PathBuf>,
}

/// The directories a font may be found under, most specific first.
fn roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        out.push(PathBuf::from(d).join("fonts"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        out.push(PathBuf::from(&h).join(".local/share/fonts"));
        out.push(PathBuf::from(&h).join(".fonts"));
    }
    out.push(PathBuf::from("/usr/local/share/fonts"));
    out.push(PathBuf::from("/usr/share/fonts"));
    out
}

fn is_font(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("ttf") | Some("otf")
    )
}

/// Walk `dir` to `depth` levels, calling `pick` on each font file; the first
/// `true` wins. Bounded because a font root can hold thousands of files and
/// startup is not the place to read all of them.
fn walk(dir: &Path, depth: usize, pick: &mut impl FnMut(&Path) -> bool) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut dirs = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            dirs.push(p);
        } else if is_font(&p) && pick(&p) {
            return Some(p);
        }
    }
    if depth == 0 {
        return None;
    }
    for d in dirs {
        if let Some(found) = walk(&d, depth - 1, pick) {
            return Some(found);
        }
    }
    None
}

/// The first readable path among the candidates.
fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.is_file()).cloned()
}

/// Find the monospace face: the person's terminal font if it is there, else any
/// Nerd Font, else a decent mono, else nothing.
fn find_mono() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LODE_FONT_MONO") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let named = [
        // The lvim-tech terminal font, which is the point of this ordering.
        "CodeNerdFont/CodeNerdFont-Light.ttf",
        "CodeNerdFont/CodeNerdFont-Regular.ttf",
        "Ubuntu/UbuntuNerdFont-Regular.ttf",
        "texlive-cascadia-code/CascadiaCode-Light.otf",
        "texlive-cascadia-code/CascadiaCode-Regular.otf",
        "texlive-fira/FiraMono-Regular.otf",
    ];
    for root in roots() {
        if let Some(p) = first_existing(&named.iter().map(|n| root.join(n)).collect::<Vec<_>>()) {
            return Some(p);
        }
    }
    // Nothing known by name: take any Nerd Font that is not an italic.
    for root in roots() {
        let found = walk(&root, 3, &mut |p| {
            let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase();
            n.contains("nerd") && !n.contains("italic") && !n.contains("propo")
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Find the proportional face for the chrome.
fn find_ui() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LODE_FONT_UI") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let named = [
        "texlive-fira/FiraSans-Regular.otf",
        "texlive-fira/FiraSans-Book.otf",
        "truetype/Roboto-Regular.ttf",
        "Inter/Inter-Regular.ttf",
    ];
    for root in roots() {
        if let Some(p) = first_existing(&named.iter().map(|n| root.join(n)).collect::<Vec<_>>()) {
            return Some(p);
        }
    }
    None
}

/// Install what was found into `ctx`, leaving egui's own fonts as the fallback
/// behind them — so a glyph neither face has still renders.
pub fn install(ctx: &egui::Context) -> Fonts {
    let mono = find_mono();
    let ui = find_ui();
    let mut defs = egui::FontDefinitions::default();

    if let Some(path) = &mono {
        if let Ok(bytes) = std::fs::read(path) {
            defs.font_data
                .insert("lode-mono".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            defs.families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "lode-mono".into());
            // Also BEHIND the proportional face: the icon glyphs live in the
            // Nerd Font, and the toolbar is proportional.
            defs.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("lode-mono".into());
        }
    }
    if let Some(path) = &ui {
        if let Ok(bytes) = std::fs::read(path) {
            defs.font_data
                .insert("lode-ui".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
            defs.families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "lode-ui".into());
        }
    }
    ctx.set_fonts(defs);

    let nerd = mono
        .as_ref()
        .and_then(|p| p.file_name().and_then(|s| s.to_str()))
        .map(|n| n.to_ascii_lowercase().contains("nerd"))
        .unwrap_or(false);
    Fonts { nerd, mono, ui }
}

/// The label set. Two instances: glyphs when a Nerd Font is installed, words
/// when it is not.
#[derive(Clone, Copy)]
pub struct Icons {
    pub run: &'static str,
    pub cancel: &'static str,
    pub new_tab: &'static str,
    pub save: &'static str,
    pub export: &'static str,
    pub refresh: &'static str,
    pub add: &'static str,
    pub edit: &'static str,
    pub delete: &'static str,
    pub connect: &'static str,
    pub disconnect: &'static str,
    pub connected: &'static str,
    pub offline: &'static str,
    pub locked: &'static str,
    pub unlocked: &'static str,
    pub database: &'static str,
    pub schema: &'static str,
    pub table: &'static str,
    pub view: &'static str,
    pub column: &'static str,
    pub query: &'static str,
    pub close: &'static str,
    pub test: &'static str,
    pub ok: &'static str,
    pub fail: &'static str,
}

/// Nerd Font glyphs, all from the Font Awesome block every Nerd Font patch
/// carries — not from a vendor-specific range that only some patches have.
pub const NERD: Icons = Icons {
    run: "\u{f04b}",         // play
    cancel: "\u{f04d}",      // stop
    new_tab: "\u{f067}",     // plus
    save: "\u{f0c7}",        // floppy
    export: "\u{f019}",      // download
    refresh: "\u{f021}",     // refresh
    add: "\u{f067}",         // plus
    edit: "\u{f044}",        // pencil-square
    delete: "\u{f00d}",      // times
    connect: "\u{f0c1}",     // chain
    disconnect: "\u{f127}",  // chain-broken
    connected: "\u{f111}",   // filled circle
    offline: "\u{f10c}",     // hollow circle
    locked: "\u{f023}",      // lock
    unlocked: "\u{f09c}",    // unlock
    database: "\u{f1c0}",    // database
    schema: "\u{f07b}",      // folder
    table: "\u{f0ce}",       // table
    view: "\u{f06e}",        // eye
    column: "\u{f0db}",      // columns
    query: "\u{f15c}",       // file-text
    close: "\u{f00d}",       // times
    test: "\u{f0e7}",        // bolt
    ok: "\u{f00c}",          // check
    fail: "\u{f06a}",        // exclamation-circle
};

/// The same set as words, for a machine with no Nerd Font.
pub const PLAIN: Icons = Icons {
    run: "▶",
    cancel: "■",
    new_tab: "+",
    save: "save",
    export: "export",
    refresh: "↻",
    add: "+",
    edit: "edit",
    delete: "×",
    connect: "connect",
    disconnect: "close",
    connected: "●",
    offline: "○",
    locked: "[enc]",
    unlocked: "[!]",
    database: "db",
    schema: "+",
    table: "·",
    view: "·",
    column: "·",
    query: "·",
    close: "×",
    test: "test",
    ok: "ok",
    fail: "!",
};

impl Icons {
    pub fn for_fonts(f: &Fonts) -> Icons {
        if f.nerd {
            NERD
        } else {
            PLAIN
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_icon_sets_are_both_filled_in() {
        // A glyph set with an empty entry would render as nothing at all, which
        // reads as a broken button rather than as a missing icon.
        for (name, set) in [("nerd", NERD), ("plain", PLAIN)] {
            for s in [
                set.run, set.cancel, set.new_tab, set.save, set.export, set.refresh, set.add,
                set.edit, set.delete, set.connect, set.disconnect, set.connected, set.offline,
                set.locked, set.unlocked, set.database, set.schema, set.table, set.view,
                set.column, set.query, set.close, set.test, set.ok, set.fail,
            ] {
                assert!(!s.is_empty(), "{name}: an icon is empty");
            }
        }
    }

    #[test]
    fn nerd_glyphs_are_in_the_private_use_area() {
        // Everything in NERD must be a patched glyph, not an ordinary character
        // that happens to look right in one font.
        for s in [NERD.run, NERD.database, NERD.table, NERD.locked, NERD.ok] {
            let c = s.chars().next().unwrap();
            assert!(
                ('\u{e000}'..='\u{f8ff}').contains(&c),
                "{c:?} is not in the private use area"
            );
        }
    }

    #[test]
    fn a_missing_font_path_is_not_fatal() {
        assert!(!is_font(Path::new("/nope")));
        assert!(is_font(Path::new("/a/b.ttf")));
        assert!(is_font(Path::new("/a/b.OTF")));
        // find_* must tolerate roots that do not exist.
        assert!(walk(Path::new("/definitely/not/here"), 2, &mut |_| true).is_none());
    }
}
