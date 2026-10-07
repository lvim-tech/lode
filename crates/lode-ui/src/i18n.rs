//! The interface, in more than one language.
//!
//! WHERE THE LINE IS, because it is the only decision here that matters.
//!
//! The CLIENT translates its own chrome: the panels, the buttons, the headings,
//! the words this program chose. The ENGINE does not translate anything, and
//! that is deliberate rather than unfinished.
//!
//! An engine message is a RECORD. "this connection is read only" goes into the
//! audit log, into a bug report, into a shell test, and into whatever somebody
//! pastes into a message to somebody else. A log whose meaning depends on the
//! locale of whoever happened to run it is a log that cannot be read by the
//! person who did not. Refusals, driver errors and the audit log stay in
//! English for the same reason SQL is in English: they are the record, not the
//! interface.
//!
//! So a window in Bulgarian shows Bulgarian panels and an English refusal, and
//! the refusal is the thing that gets quoted.
//!
//! HOW A MISSING TRANSLATION IS CAUGHT. Not by falling back silently — that
//! gives a half-English window nobody notices. `every_language_has_every_key`
//! asserts that each catalogue has exactly the keys English has, so a key added
//! without a translation fails the build's tests rather than appearing as a
//! blank label three weeks later.

/// The languages there are.
///
/// A short list on purpose: each one is a promise to keep it complete, and the
/// test above enforces the promise. Adding one is adding a table and fixing the
/// test failure it causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    English,
    Bulgarian,
}

impl Lang {
    pub const ALL: &'static [Lang] = &[Lang::English, Lang::Bulgarian];

    /// What the settings panel shows. In the language ITSELF, not translated —
    /// somebody looking for their own language is looking for the word they
    /// would write, not for its English name.
    pub fn label(self) -> &'static str {
        match self {
            Lang::English => "English",
            Lang::Bulgarian => "Български",
        }
    }

    /// The tag stored in the session and read from `$LODE_LANG`.
    pub fn tag(self) -> &'static str {
        match self {
            Lang::English => "en",
            Lang::Bulgarian => "bg",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Lang> {
        Lang::ALL.iter().copied().find(|l| l.tag() == tag)
    }

    /// `$LODE_LANG`, then `$LC_ALL` / `$LANG`, then English.
    ///
    /// The system variables are read as a PREFIX (`bg_BG.UTF-8` is Bulgarian),
    /// because that is the shape they come in and demanding an exact tag would
    /// mean nobody's system setting ever matched.
    pub fn from_env() -> Option<Lang> {
        if let Ok(want) = std::env::var("LODE_LANG") {
            if let Some(l) = Lang::from_tag(want.trim()) {
                return Some(l);
            }
        }
        for var in ["LC_ALL", "LANG"] {
            if let Ok(v) = std::env::var(var) {
                let head = v.split(['_', '.', '@']).next().unwrap_or("");
                if let Some(l) = Lang::from_tag(head) {
                    return Some(l);
                }
            }
        }
        None
    }

    fn table(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Lang::English => EN,
            Lang::Bulgarian => BG,
        }
    }

    /// One phrase.
    ///
    /// A key with no entry comes back as the KEY, visibly wrong, rather than as
    /// an empty string. A blank label is a layout that looks finished; the key
    /// is a label that says what is missing. The test makes it unreachable, and
    /// this is what happens if it ever is reached anyway.
    pub fn tr(self, key: &str) -> &'static str {
        if let Some((_, v)) = self.table().iter().find(|(k, _)| *k == key) {
            return v;
        }
        if let Some((_, v)) = EN.iter().find(|(k, _)| *k == key) {
            return v;
        }
        // Leaked deliberately: a key is a short static string, this happens only
        // for a key that does not exist, and the alternative is an allocation
        // on every lookup to serve a case the tests make impossible.
        Box::leak(key.to_string().into_boxed_str())
    }
}

/// English, and the list of keys every other language must match.
#[rustfmt::skip]
const EN: &[(&str, &str)] = &[
    // The icon rail.
    ("rail.connections", "Connections"),
    ("rail.instance",    "Instance"),
    ("rail.history",     "History"),
    ("rail.audit",       "Audit"),
    ("rail.tasks",       "Tasks"),
    ("rail.settings",    "Settings"),
    // Panels.
    ("panel.filter",         "Filter connections and schema…"),
    ("panel.new_connection", "New connection"),
    ("panel.new_dashboard",  "New dashboard"),
    ("panel.dashboards",     "Dashboards"),
    ("panel.connections",    "Connections"),
    ("panel.connected",      "connected"),
    ("panel.idle",           "idle"),
    ("panel.saved_queries",  "Saved queries"),
    ("panel.saved_charts",   "Saved charts"),
    ("panel.schema",         "Schema"),
    ("panel.look_again",     "Look again"),
    ("panel.reload_history", "Reload history"),
    ("panel.reload_audit",   "Reload audit"),
    ("panel.search_history", "Search history…"),
    ("panel.search_audit",   "Search the audit log…"),
    ("panel.nothing_matches", "nothing matches"),
    ("panel.loading",        "loading…"),
    // Settings.
    ("settings.theme",      "Theme"),
    ("settings.text_size",  "Text size"),
    ("settings.icon_size",  "Icon size"),
    ("tip.edit_conn", "Edit connection"),
    ("tip.new_conn", "New connection"),
    ("tip.close_tab", "Close tab"),
    ("tip.remove_tile", "Remove this tile"),
    ("tip.close_detail", "Close the detail"),
    ("tip.cancel_edit", "Cancel"),
    ("tip.new_board", "New dashboard"),
    ("tip.open_board", "Open dashboard"),
    ("tip.reveal", "Show the value"),
    ("tip.instance", "Instance"),
    ("settings.json_indent", "JSON indent"),
    ("settings.language",   "Language"),
    ("settings.text",       "Text"),
    ("settings.reset",      "reset"),
    ("settings.no_themes",  "no theme files found — looked in:"),
    // The result area.
    ("result.empty_title", "Click a table or a collection to open it."),
    ("result.empty_hint",  "Its rows appear here — the statement comes from the driver, not from you."),
    ("result.run",         "Run"),
    ("result.cancel",      "Cancel"),
    ("result.export",      "Export"),
    ("result.save_query",  "Save query"),
    ("result.save_chart",  "Save chart"),
    ("result.refresh",     "Refresh"),
    ("result.rows",        "row(s)"),
    ("result.no_columns",  "no columns"),
    // Writing.
    ("write.run_it",       "Run it"),
    ("write.leave_it",     "Leave it"),
    ("write.delete_row",   "Delete row"),
    ("write.copy_select",  "Copy as SELECT"),
    ("write.copy_insert",  "Copy as INSERT"),
    // Dashboards.
    ("board.arrange",      "Arrange"),
    ("board.done",         "Done"),
    ("board.save",         "Save"),
    ("board.saved",        "Saved"),
    ("board.remove",       "Remove"),
    ("board.name_it",      "Name it"),
    ("board.apply",        "Apply"),
    ("board.clear",        "clear"),
    ("board.time",         "time"),
    ("board.from",         "from"),
    ("board.to",           "to"),
    ("board.empty",        "an empty board — press Arrange, then add a chart you have saved"),
    // Tasks and the console.
    ("tasks.title",          "Background tasks"),
    ("tasks.clear_finished", "Clear finished"),
    ("tasks.failed",         "failed"),
    ("console.title",        "Command"),
    ("console.hint",         "dangerous commands ask first"),
];

/// Bulgarian.
///
/// The person this is for works in Bulgarian, which is why it is the second
/// one: a second language that nobody reads proves the mechanism and not the
/// translation.
#[rustfmt::skip]
const BG: &[(&str, &str)] = &[
    ("rail.connections", "Връзки"),
    ("rail.instance",    "Сървър"),
    ("rail.history",     "История"),
    ("rail.audit",       "Одит"),
    ("rail.tasks",       "Задачи"),
    ("rail.settings",    "Настройки"),

    ("panel.filter",         "Филтър за връзки и схема…"),
    ("panel.new_connection", "Нова връзка"),
    ("panel.new_dashboard",  "Ново табло"),
    ("panel.dashboards",     "Табла"),
    ("panel.connections",    "Връзки"),
    ("panel.connected",      "свързани"),
    ("panel.idle",           "в покой"),
    ("panel.saved_queries",  "Запазени заявки"),
    ("panel.saved_charts",   "Запазени диаграми"),
    ("panel.schema",         "Схема"),
    ("panel.look_again",     "Потърси пак"),
    ("panel.reload_history", "Презареди историята"),
    ("panel.reload_audit",   "Презареди одита"),
    ("panel.search_history", "Търсене в историята…"),
    ("panel.search_audit",   "Търсене в одитния дневник…"),
    ("panel.nothing_matches", "нищо не съвпада"),
    ("panel.loading",        "зарежда се…"),

    ("settings.theme",      "Тема"),
    ("settings.text_size",  "Размер на текста"),
    ("settings.icon_size",  "Размер на иконите"),
    ("tip.edit_conn", "Редактирай връзката"),
    ("tip.new_conn", "Нова връзка"),
    ("tip.close_tab", "Затвори таба"),
    ("tip.remove_tile", "Премахни плочката"),
    ("tip.close_detail", "Затвори подробностите"),
    ("tip.cancel_edit", "Откажи"),
    ("tip.new_board", "Ново табло"),
    ("tip.open_board", "Отвори табло"),
    ("tip.reveal", "Покажи стойността"),
    ("tip.instance", "Инстанция"),
    ("settings.json_indent", "Отстъп в JSON"),
    ("settings.language",   "Език"),
    ("settings.text",       "Текст"),
    ("settings.reset",      "по подразбиране"),
    ("settings.no_themes",  "няма намерени файлове с теми — търсено в:"),

    ("result.empty_title", "Щракни върху таблица или колекция, за да я отвориш."),
    ("result.empty_hint",  "Редовете ѝ се появяват тук — заявката идва от драйвера, не от теб."),
    ("result.run",         "Изпълни"),
    ("result.cancel",      "Отказ"),
    ("result.export",      "Износ"),
    ("result.save_query",  "Запази заявката"),
    ("result.save_chart",  "Запази диаграмата"),
    ("result.refresh",     "Опресни"),
    ("result.rows",        "ред(а)"),
    ("result.no_columns",  "няма колони"),

    ("write.run_it",       "Изпълни го"),
    ("write.leave_it",     "Остави го"),
    ("write.delete_row",   "Изтрий реда"),
    ("write.copy_select",  "Копирай като SELECT"),
    ("write.copy_insert",  "Копирай като INSERT"),

    ("board.arrange",      "Подреди"),
    ("board.done",         "Готово"),
    ("board.save",         "Запази"),
    ("board.saved",        "Запазено"),
    ("board.remove",       "Премахни"),
    ("board.name_it",      "Наименувай"),
    ("board.apply",        "Приложи"),
    ("board.clear",        "изчисти"),
    ("board.time",         "време"),
    ("board.from",         "от"),
    ("board.to",           "до"),
    ("board.empty",        "празно табло — натисни Подреди и добави запазена диаграма"),

    ("tasks.title",          "Задачи на заден план"),
    ("tasks.clear_finished", "Изчисти завършилите"),
    ("tasks.failed",         "провалени"),
    ("console.title",        "Команда"),
    ("console.hint",         "опасните команди питат първо"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The promise each language makes. A key added without a translation fails
    /// HERE, rather than appearing as a blank label three weeks later in
    /// somebody's window.
    #[test]
    fn every_language_has_every_key() {
        let en: BTreeSet<&str> = EN.iter().map(|(k, _)| *k).collect();
        for lang in Lang::ALL {
            if *lang == Lang::English {
                continue;
            }
            let got: BTreeSet<&str> = lang.table().iter().map(|(k, _)| *k).collect();
            let missing: Vec<&&str> = en.difference(&got).collect();
            let extra: Vec<&&str> = got.difference(&en).collect();
            assert!(missing.is_empty(), "{} is missing: {missing:?}", lang.tag());
            // Extra keys are as wrong as missing ones: a translation for a key
            // nothing uses is a line somebody will maintain for ever.
            assert!(extra.is_empty(), "{} has keys English does not: {extra:?}", lang.tag());
        }
    }

    #[test]
    fn no_key_is_defined_twice() {
        for lang in Lang::ALL {
            let t = lang.table();
            let uniq: BTreeSet<&str> = t.iter().map(|(k, _)| *k).collect();
            assert_eq!(uniq.len(), t.len(), "{} defines a key twice", lang.tag());
        }
    }

    /// A translation that is still the English word is usually a line somebody
    /// forgot. Not an error — "Instance" and some proper nouns legitimately
    /// stay — but it must be a short, deliberate list rather than a drift.
    #[test]
    fn bulgarian_is_actually_translated() {
        let same: Vec<&str> = BG
            .iter()
            .filter(|(k, v)| EN.iter().any(|(ek, ev)| ek == k && ev == v))
            .map(|(k, _)| *k)
            .collect();
        assert!(same.is_empty(), "left in English: {same:?}");
    }

    #[test]
    fn a_language_is_found_by_its_tag_and_by_a_locale() {
        assert_eq!(Lang::from_tag("bg"), Some(Lang::Bulgarian));
        assert_eq!(Lang::from_tag("de"), None);
        // BOTH system variables, because the fallback reads both and this
        // machine's own `LANG` answered for the one the test had cleared — the
        // first version asserted `None` and got `English`, which was the code
        // being right and the test being incomplete.
        let (lang, lc) = (std::env::var("LANG").ok(), std::env::var("LC_ALL").ok());
        std::env::remove_var("LODE_LANG");
        std::env::remove_var("LANG");
        std::env::set_var("LC_ALL", "bg_BG.UTF-8");
        assert_eq!(Lang::from_env(), Some(Lang::Bulgarian));
        // A locale nobody has a catalogue for is not a language this has.
        std::env::set_var("LC_ALL", "fr_FR.UTF-8");
        assert_eq!(Lang::from_env(), None);
        // `LANG` is read when `LC_ALL` says nothing useful.
        std::env::set_var("LANG", "bg_BG.UTF-8");
        assert_eq!(Lang::from_env(), Some(Lang::Bulgarian));
        // The explicit variable wins over both.
        std::env::set_var("LODE_LANG", "en");
        assert_eq!(Lang::from_env(), Some(Lang::English));
        std::env::remove_var("LODE_LANG");
        std::env::remove_var("LC_ALL");
        std::env::remove_var("LANG");
        if let Some(v) = lang {
            std::env::set_var("LANG", v);
        }
        if let Some(v) = lc {
            std::env::set_var("LC_ALL", v);
        }
    }

    /// A key with no entry comes back visibly wrong rather than blank: an empty
    /// label is a layout that looks finished.
    #[test]
    fn an_unknown_key_shows_itself() {
        assert_eq!(Lang::English.tr("no.such.key"), "no.such.key");
        assert_eq!(Lang::Bulgarian.tr("no.such.key"), "no.such.key");
    }

    /// The language's own name, not its English one: somebody looking for their
    /// language looks for the word they would write.
    #[test]
    fn a_language_is_named_in_itself() {
        assert_eq!(Lang::Bulgarian.label(), "Български");
    }
}
