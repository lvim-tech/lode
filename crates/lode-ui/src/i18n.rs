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
    ("empty.connections",  "no connections yet"),
    ("empty.connections_hint", "press New connection to add one"),
    ("empty.tasks",        "nothing running"),
    ("empty.tasks_hint",   "a statement you run appears here while it works"),
    ("empty.instance",     "nothing read yet"),
    ("empty.instance_hint", "press now, or choose an interval"),
    ("empty.rows",         "no rows"),
    ("empty.rows_hint",    "the statement ran and returned nothing"),
    ("empty.history",      "nothing in the history yet"),
    ("empty.audit",        "nothing in the audit log yet"),
    ("empty.audit_hint",   "reads are not in here — only what changed, and what was stopped"),
    // The connection and tunnel windows.
    ("form.required",      "Fields marked * are required"),
    ("form.name",          "Name"),
    ("form.tab_main",      "Main"),
    ("form.tab_access",    "Access (SSH)"),
    ("form.tab_settings",  "Settings"),
    ("form.auth",          "Authentication"),
    ("form.provider",      "Provider"),
    ("form.token",         "Token"),
    ("form.password",      "Password"),
    ("form.passphrase",    "Passphrase"),
    ("form.ssl_mode",      "SSL mode"),
    ("form.ca_bundle",     "CA bundle"),
    ("form.certificate",   "Certificate"),
    ("form.client_cert",   "Client cert"),
    ("form.client_key",    "Client key"),
    ("form.key",           "Key"),
    ("form.key_file",      "Key file"),
    ("form.tunnel",        "Tunnel"),
    ("form.ssh",           "SSH"),
    ("form.ssh_tunnel",    "SSH tunnel"),
    ("form.ssh_host",      "SSH host"),
    ("form.ssh_port",      "SSH port"),
    ("form.ssh_user",      "SSH user"),
    ("form.goes_through",  "Goes through"),
    ("form.method",        "Method"),
    ("form.own",           "this connection's own"),
    ("form.browse",        "browse…"),
    ("form.test_conn",     "Test connection"),
    ("form.save",          "Save"),
    ("form.save_profile",  "Save as profile"),
    ("form.edit_tunnel",   "edit this tunnel"),
    ("form.policy_heading", "WHAT THIS CONNECTION MAY DO"),
    ("form.policy_read",   "Read only"),
    ("form.policy_read_why", "refuses anything that changes the database — an INSERT as firmly as a DROP"),
    ("form.policy_confirm", "Asks first"),
    ("form.policy_confirm_why", "writes are allowed; the dangerous ones have to be confirmed"),
    ("form.policy_open",   "Unrestricted"),
    ("form.policy_open_why", "everything runs — for a scratch database where the asking is the nuisance"),
    ("form.policy_note",   "Held by the engine, not by this window: a client that skips its own confirmation still cannot get past it."),
    ("form.plaintext_note", "the link will be PLAINTEXT — the engine warns on every connect"),
    ("form.passphrase_note", "Leave the passphrase empty for a key that has none."),
    ("form.no_tunnels",    "No saved tunnels yet. Fill these in and press \"save as profile\" to reuse them in another connection."),
    ("form.profile_note",  "These values live in the profile. Changing them there changes every connection that uses it."),
    ("form.save_over_note", "Saving over a name replaces it — one bastion, one entry, changed in one place."),
    ("form.no_aws",        "No AWS profiles were found — type a name, or check that ~/.aws/config is where the engine looks."),
    ("form.token_note",    "The token is what connects. The provider name is stored with the connection and is not resolved by the engine today."),
    ("form.source_note",   "Literal is stored as typed. Env, Command and Keyring are resolved by the engine at connect time, so the secret never reaches this window."),
    ("busy.title",         "{name} is still working on {n} statement(s)"),
    ("busy.cancel",        "Stop them and disconnect"),
    ("busy.wait",          "Keep waiting"),
    ("busy.anyway",        "Disconnect anyway"),
    ("busy.note",          "Disconnecting kills what is running: the engine drops the connection under it."),
    ("approve.title",      "Waiting for your answer"),
    ("approve.yes",        "Allow it"),
    ("approve.no",         "Refuse"),
    ("approve.note",       "An agent asked for this and cannot confirm it itself. A refusal stands — it does not mean \"not this second\"."),
    // The context menu.
    ("menu.connect",       "Connect"),
    ("menu.disconnect",    "Disconnect"),
    ("menu.refresh",       "Look at the schema again"),
    ("menu.edit",          "Settings\u{2026}"),
    ("menu.remove",        "Remove\u{2026}"),
    ("menu.copy_name",     "Copy the name"),
    ("menu.move_to",       "Move to a folder\u{2026}"),
    ("menu.open",          "Open"),
    ("menu.inside",        "Fields and indexes"),
    ("menu.insert_name",   "Put the name in the statement"),
    ("menu.copy_ddl",      "Copy the DDL"),
    ("menu.show_row",      "Show this row"),
    ("menu.copy_row",      "Copy the row"),
    ("menu.copy_select",   "Copy as SELECT"),
    ("menu.copy_insert",   "Copy as INSERT"),
    ("menu.edit_row",      "Edit this row"),
    ("menu.delete_row",    "Delete this row\u{2026}"),
    ("menu.close_tab",     "Close"),
    ("menu.close_others",  "Close the others"),
    ("menu.duplicate_tab", "Duplicate"),
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
    ("tip.close_tab", "Close tab"),
    ("tip.remove_tile", "Remove this tile"),
    ("tip.close_detail", "Close the detail"),
    ("tip.cancel_edit", "Cancel"),
    ("tip.new_board", "New dashboard"),
    ("tip.open_board", "Open dashboard"),
    ("tip.reveal", "Show the value"),
    ("tip.hide", "Hide the value"),
    ("tip.link_tls", "encrypted (TLS)"),
    ("tip.stop_task", "Stop this statement"),
    ("result.name_it", "name this"),
    ("name.folder",    "folder for"),
    ("tip.remove_conn", "Remove connection"),
    ("conn.remove_ask", "Remove \"{name}\"? {n} saved queries, and its charts, dashboards and scripts, go with it."),
    ("conn.remove_it", "remove it"),
    ("conn.keep_it", "keep it"),
    ("result.save", "save"),
    ("tip.link_tunnel", "encrypted (SSH tunnel)"),
    ("tip.link_plain", "PLAINTEXT — anyone on the wire can read this"),
    ("toast.plaintext", "plaintext connection: credentials and rows go over the wire unencrypted"),
    ("settings.filter_themes", "Filter themes…"),
    ("panel.rescan",      "Rescan theme folders"),
    ("settings.json_indent", "JSON indent"),
    ("settings.text_font",  "Text font"),
    ("settings.icon_font",  "Icon font"),
    ("settings.remembers",  "the theme, the sizes, the language, the open tabs and this window's panel are remembered in ~/.local/share/lode/session.json"),
    ("settings.font_env",   "$LODE_FONT and $LODE_ICON_FONT choose these; they are read at startup."),
    ("settings.language",   "Language"),
    ("settings.text",       "Text"),
    ("settings.reset",      "reset"),
    ("settings.no_themes",  "no theme files found — looked in:"),
    // The result area.
    ("result.empty_title", "Click a table or a collection to open it."),
    ("result.empty_hint",  "Its rows appear here — the statement comes from the driver, not from you."),
    ("result.run",         "Run"),
    ("result.find",        "Find"),
    ("result.cancel",      "Cancel"),
    ("result.export",      "Export"),
    ("export.loaded",      "Write the loaded rows"),
    ("export.all",         "Write ALL rows"),
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
    ("board.text_hint",    "select this tile and type a heading"),
    ("board.every",        "refresh every"),
    ("board.empty",        "an empty board — press Arrange, then add a chart you have saved"),
    // Tasks and the console.
    ("tasks.title",          "Background tasks"),
    ("tasks.clear_finished", "Clear finished"),
    ("tasks.failed",         "failed"),
    ("console.title",        "Command"),
    ("bottom.log",           "Log"),
    ("empty.log",            "nothing in the log"),
    ("empty.log_hint",       "warnings and failures this window could not show anywhere else land here"),
    ("bottom.running",       "running"),
    ("bottom.unread",        "unread"),
    ("console.hint",         "dangerous commands ask first"),
];

/// Bulgarian.
///
/// The person this is for works in Bulgarian, which is why it is the second
/// one: a second language that nobody reads proves the mechanism and not the
/// translation.
#[rustfmt::skip]
const BG: &[(&str, &str)] = &[
    ("empty.connections",  "още няма връзки"),
    ("empty.connections_hint", "натисни „Нова връзка“, за да добавиш"),
    ("empty.tasks",        "нищо не върви"),
    ("empty.tasks_hint",   "заявка, която пуснеш, се появява тук, докато работи"),
    ("empty.instance",     "още нищо не е прочетено"),
    ("empty.instance_hint", "натисни „сега“ или избери интервал"),
    ("empty.rows",         "няма редове"),
    ("empty.rows_hint",    "заявката се изпълни и не върна нищо"),
    ("empty.history",      "историята е празна"),
    ("empty.audit",        "одитният дневник е празен"),
    ("empty.audit_hint",   "четенията не влизат тук — само това, което е променило нещо, и това, което е било спряно"),
    ("form.required",      "Полетата с * са задължителни"),
    ("form.name",          "Име"),
    ("form.tab_main",      "Основни"),
    ("form.tab_access",    "Достъп (SSH)"),
    ("form.tab_settings",  "Настройки"),
    ("form.auth",          "Удостоверяване"),
    ("form.provider",      "Доставчик"),
    ("form.token",         "Токен"),
    ("form.password",      "Парола"),
    ("form.passphrase",    "Парола на ключа"),
    ("form.ssl_mode",      "SSL режим"),
    ("form.ca_bundle",     "CA пакет"),
    ("form.certificate",   "Сертификат"),
    ("form.client_cert",   "Клиентски сертификат"),
    ("form.client_key",    "Клиентски ключ"),
    ("form.key",           "Ключ"),
    ("form.key_file",      "Файл с ключ"),
    ("form.tunnel",        "Тунел"),
    ("form.ssh",           "SSH"),
    ("form.ssh_tunnel",    "SSH тунел"),
    ("form.ssh_host",      "SSH хост"),
    ("form.ssh_port",      "SSH порт"),
    ("form.ssh_user",      "SSH потребител"),
    ("form.goes_through",  "Минава през"),
    ("form.method",        "Метод"),
    ("form.own",           "собствен на тази връзка"),
    ("form.browse",        "избери…"),
    ("form.test_conn",     "Провери връзката"),
    ("form.save",          "Запази"),
    ("form.save_profile",  "Запази като профил"),
    ("form.edit_tunnel",   "редактирай този тунел"),
    ("form.policy_heading", "КАКВО Е ПОЗВОЛЕНО НА ТАЗИ ВРЪЗКА"),
    ("form.policy_read",   "Само четене"),
    ("form.policy_read_why", "отказва всичко, което променя базата — INSERT толкова категорично, колкото DROP"),
    ("form.policy_confirm", "Пита първо"),
    ("form.policy_confirm_why", "писането е позволено; опасните неща се потвърждават"),
    ("form.policy_open",   "Без ограничения"),
    ("form.policy_open_why", "всичко минава — за черновова база, където питането е пречка"),
    ("form.policy_note",   "Държи го двигателят, не този прозорец: клиент, който пропусне своето потвърждение, пак не може да го заобиколи."),
    ("form.plaintext_note", "връзката ще е НЕШИФРОВАНА — двигателят предупреждава при всяко свързване"),
    ("form.passphrase_note", "Остави паролата на ключа празна, ако ключът няма такава."),
    ("form.no_tunnels",    "Още няма запазени тунели. Попълни тези полета и натисни „запази като профил“, за да ги ползваш и в друга връзка."),
    ("form.profile_note",  "Тези стойности живеят в профила. Промяната им там променя всяка връзка, която го ползва."),
    ("form.save_over_note", "Запазване върху същото име го подменя — един бастион, един запис, сменя се на едно място."),
    ("form.no_aws",        "Не са намерени AWS профили — напиши име или провери дали ~/.aws/config е там, където двигателят гледа."),
    ("form.token_note",    "Токенът е това, което свързва. Името на доставчика се пази с връзката и днес двигателят не го разрешава."),
    ("form.source_note",   "„Literal“ се пази както е въведено. „Env“, „Command“ и „Keyring“ се разрешават от двигателя при свързване, тоест тайната никога не стига до този прозорец."),
    ("approve.title",      "Чака твоя отговор"),
    ("approve.yes",        "Позволи"),
    ("approve.no",         "Откажи"),
    ("approve.note",       "Агент е поискал това и не може да го потвърди сам. Отказът ОСТАВА — не значи „не точно сега“."),
    ("busy.title",         "{name} още работи по {n} заявки"),
    ("busy.cancel",        "Спри ги и се разкачи"),
    ("busy.wait",          "Продължи да чакаш"),
    ("busy.anyway",        "Разкачи се все пак"),
    ("busy.note",          "Разкачането убива това, което върви: двигателят пуска връзката под него."),
    ("menu.connect",       "Свържи се"),
    ("menu.disconnect",    "Разкачи се"),
    ("menu.refresh",       "Погледни схемата отново"),
    ("menu.edit",          "Настройки\u{2026}"),
    ("menu.remove",        "Премахни\u{2026}"),
    ("menu.copy_name",     "Копирай името"),
    ("menu.move_to",       "Премести в папка\u{2026}"),
    ("menu.open",          "Отвори"),
    ("menu.inside",        "Полета и индекси"),
    ("menu.insert_name",   "Сложи името в заявката"),
    ("menu.copy_ddl",      "Копирай DDL-а"),
    ("menu.show_row",      "Покажи този ред"),
    ("menu.copy_row",      "Копирай реда"),
    ("menu.copy_select",   "Копирай като SELECT"),
    ("menu.copy_insert",   "Копирай като INSERT"),
    ("menu.edit_row",      "Редактирай този ред"),
    ("menu.delete_row",    "Изтрий този ред\u{2026}"),
    ("menu.close_tab",     "Затвори"),
    ("menu.close_others",  "Затвори другите"),
    ("menu.duplicate_tab", "Дублирай"),
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
    ("tip.close_tab", "Затвори таба"),
    ("tip.remove_tile", "Премахни плочката"),
    ("tip.close_detail", "Затвори подробностите"),
    ("tip.cancel_edit", "Откажи"),
    ("tip.new_board", "Ново табло"),
    ("tip.open_board", "Отвори табло"),
    ("tip.reveal", "Покажи стойността"),
    ("tip.hide", "Скрий стойността"),
    ("tip.link_tls", "шифрована (TLS)"),
    ("tip.stop_task", "Спри тази заявка"),
    ("result.name_it", "име на"),
    ("name.folder",    "папка за"),
    ("tip.remove_conn", "Премахни връзката"),
    ("conn.remove_ask", "Да премахна ли \"{name}\"? С нея си отиват {n} запазени заявки, както и графиките, таблата и скриптовете ѝ."),
    ("conn.remove_it", "премахни я"),
    ("conn.keep_it", "остави я"),
    ("result.save", "запази"),
    ("tip.link_tunnel", "шифрована (SSH тунел)"),
    ("tip.link_plain", "НЕШИФРОВАНА — всеки по пътя може да я чете"),
    ("toast.plaintext", "нешифрована връзка: паролата и редовете минават по мрежата открити"),
    ("settings.filter_themes", "Филтър по теми…"),
    ("panel.rescan",      "Опресни списъка с теми"),
    ("settings.json_indent", "Отстъп в JSON"),
    ("settings.text_font",  "Шрифт за текста"),
    ("settings.icon_font",  "Шрифт за иконите"),
    ("settings.remembers",  "темата, размерите, езикът, отворените табове и панелът на този прозорец се помнят в ~/.local/share/lode/session.json"),
    ("settings.font_env",   "$LODE_FONT и $LODE_ICON_FONT ги избират; четат се при стартиране."),
    ("settings.language",   "Език"),
    ("settings.text",       "Текст"),
    ("settings.reset",      "по подразбиране"),
    ("settings.no_themes",  "няма намерени файлове с теми — търсено в:"),

    ("result.empty_title", "Щракни върху таблица или колекция, за да я отвориш."),
    ("result.empty_hint",  "Редовете ѝ се появяват тук — заявката идва от драйвера, не от теб."),
    ("result.run",         "Изпълни"),
    ("result.find",        "Намери"),
    ("result.cancel",      "Отказ"),
    ("result.export",      "Износ"),
    ("export.loaded",      "Запиши заредените редове"),
    ("export.all",         "Запиши ВСИЧКИ редове"),
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
    ("board.text_hint",    "избери тази плочка и напиши заглавие"),
    ("board.every",        "опресняване на"),
    ("board.empty",        "празно табло — натисни Подреди и добави запазена диаграма"),

    ("tasks.title",          "Задачи на заден план"),
    ("tasks.clear_finished", "Изчисти завършилите"),
    ("tasks.failed",         "провалени"),
    ("console.title",        "Команда"),
    ("bottom.log",           "Дневник"),
    ("empty.log",            "дневникът е празен"),
    ("empty.log_hint",       "предупрежденията и провалите, които прозорецът няма къде друго да каже, идват тук"),
    ("bottom.running",       "вървят"),
    ("bottom.unread",        "непрочетени"),
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
    /// forgot. Not an error — a proper noun legitimately stays — but it must be
    /// a short, DELIBERATE list rather than a drift.
    #[test]
    fn bulgarian_is_actually_translated() {
        // `SSH` is a protocol's name. It is the same four letters in Bulgarian
        // prose, and transliterating it would make the Access tab say something
        // nobody writes.
        const SAME_ON_PURPOSE: [&str; 1] = ["form.ssh"];
        let same: Vec<&str> = BG
            .iter()
            .filter(|(k, v)| EN.iter().any(|(ek, ev)| ek == k && ev == v))
            .map(|(k, _)| *k)
            .filter(|k| !SAME_ON_PURPOSE.contains(k))
            .collect();
        assert!(same.is_empty(), "left in English: {same:?}");
        // And the list stays honest: a key on it that HAS been translated is a
        // line nobody took off again.
        for k in SAME_ON_PURPOSE {
            let en = EN.iter().find(|(ek, _)| *ek == k).map(|(_, v)| *v);
            let bg = BG.iter().find(|(bk, _)| *bk == k).map(|(_, v)| *v);
            assert_eq!(en, bg, "{k} is translated now — take it off the list");
        }
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
