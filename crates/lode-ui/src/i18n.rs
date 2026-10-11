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
    ("form.tab_hooks",         "Hooks"),
    ("hook.add",               "+  a hook"),
    ("hook.when",              "When"),
    ("hook.what",              "What"),
    ("hook.timeout",           "Timeout (ms)"),
    ("hook.none",              "Nothing runs when this connection opens or closes."),
    ("hook.blocking",          "A failure here STOPS the connection."),
    ("hook.not_blocking",      "A failure here is reported; it stops nothing."),
    ("hook.cmd_hint",          "a command line, run through a shell"),
    ("hook.script_hint",       "the name of a saved script"),
    ("hook.no_scripts",        "No saved scripts for this connection yet."),
    ("hook.output_goes",       "What a hook prints arrives in the log, as it runs."),
    ("hook.remove",            "remove"),
    ("form.tab_main",      "Main"),
    ("form.tab_access",    "Access (SSH)"),
    ("form.tab_settings",  "Settings"),
    ("form.auth",          "Authentication"),
    ("form.provider",      "Provider"),
    ("form.token",         "Token"),
    ("form.test_proxy",        "Test proxy"),
    ("form.no_proxies",        "No saved proxies \u{2014} Settings \u{203a} Proxies makes one"),
    ("form.proxy_written_out", "This connection carries a proxy written into its spec; choosing one above would replace it:"),
    ("form.proxy_kind",     "Protocol"),
    ("form.proxy_host",     "Proxy host"),
    ("form.proxy_port",     "Proxy port"),
    ("form.proxy_auth_opt", "A sign-in only if the proxy wants one \u{2014} leave both empty for none"),
    ("form.proxy",          "Proxy"),
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
    ("form.none",          "none"),
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
    ("menu.edit_doc",      "Edit the whole row"),
    ("menu.delete_row",    "Delete this row\u{2026}"),
    ("menu.close_tab",     "Close"),
    ("menu.close_others",  "Close the others"),
    ("menu.duplicate_tab", "Duplicate"),
    // The icon rail.
    ("rail.connections", "Connections"),
    ("rail.instance",    "Instance"),
    ("rail.history",     "History"),
    ("rail.audit",       "Audit"),
    ("rail.scripts",   "Scripts"),
    ("script.new",     "+  a script"),
    ("script.run",     "Run"),
    ("script.cancel",  "Cancel"),
    ("script.save",    "Save"),
    ("script.none",    "No saved scripts yet."),
    ("script.none_hint", "Press + to write one \u{2014} bash, python or lua, run by the engine."),
    ("script.global",  "every connection"),
    ("script.running", "running\u{2026}"),
    ("script.silent",  "It printed nothing."),
    ("ui.create", "CREATE"),
    ("ui.where", "Where"),
    ("ui.order", "Order"),
    ("ui.show", "Show"),
    ("ui.columns", "Columns"),
    ("ui.indexes", "Indexes"),
    ("ui.instance", "Instance"),
    ("ui.copy", "copy"),
    ("ui.copy_all", "copy all"),
    ("ui.cancel", "cancel"),
    ("ui.edit", "edit"),
    ("ui.remove", "remove"),
    ("ui.condition", "+ condition"),
    ("ui.just_first", "just the first"),
    ("ui.lay_out", "lay it out again"),
    ("ui.copy_refs", "copy the references"),
    ("ui.test", "test"),
    ("ui.import", "import"),
    ("ui.key", "key"),
    ("ui.unique", "unique"),
    ("ui.no_engine", "No engine"),
    ("ui.every_column", "every column"),
    ("ui.asking", "asking\u{2026}"),
    ("ui.read_every", "read every"),
    ("ui.reading", "reading\u{2026}"),
    ("ui.running", "running\u{2026}"),
    ("ui.reading_schema", "reading the schema\u{2026}"),
    ("ui.not_loaded", "not loaded"),
    ("ui.measure", "measure"),
    ("ui.takes_no_value", "takes no value"),
    ("ui.not_answered", "the engine has not answered yet"),
    ("ui.no_sql", "this driver does not speak SQL"),
    ("ui.nothing_matches", "nothing matches"),
    ("ui.no_icons", "no icons"),
    ("ui.capabilities", "CAPABILITIES"),
    ("ui.remove_it", "remove it"),
    ("ui.keep_it", "keep it"),
    ("ui.choose_file", "Choose a file\u{2026}"),
    ("ui.make_wallet", "Make a wallet"),
    ("ui.unlock", "Unlock"),
    ("ui.lock_now", "Lock it now"),
    ("ui.driver", "Driver"),
    ("ui.parameters", "Parameters"),
    ("ui.version", "Version"),
    ("ui.protocol", "Protocol"),
    ("ui.binary", "Binary"),
    ("ui.path", "Path"),
    ("ui.keeping", "Keeping"),
    ("ui.the_agent", "The agent"),
    ("ui.the_wallet", "The wallet"),
    ("ui.agent_proto", "Agent protocol"),
    ("ui.the_engine", "THE ENGINE"),
    ("ui.the_store", "THE STORE"),
    ("ui.older_store", "AN OLDER STORE"),
    ("ui.where_secrets", "WHERE SECRETS GO"),
    ("ui.browse", "browse\u{2026}"),
    ("ui.save", "Save"),
    ("ui.cancel_b", "Cancel"),
    ("ui.password", "Password"),
    ("ui.engine_restart", "Changes take effect when the engine restarts."),
    ("ui.nothing_waiting", "Nothing is waiting for an answer."),
    ("ui.decide_not_built", "Deciding one from this window is not built yet."),
    ("ph.filter_conns", "Filter connections and schema\u{2026}"),
    ("ph.search_history", "Search history\u{2026}"),
    ("ph.search_audit", "Search the audit log\u{2026}"),
    ("ph.name_dashboard", "name this dashboard"),
    ("ph.tile_heading", "heading for this tile"),
    ("ph.column", "column"),
    ("ph.find_command", "Type to find a command\u{2026}"),
    ("ph.name_enter", "name it, then press Enter"),
    ("ph.value", "value"),
    ("ph.search_settings", "Search settings\u{2026}"),
    ("ph.filter_fonts", "filter fonts\u{2026}"),
    ("ph.passphrase", "passphrase"),
    ("ph.console", "Type a command and press Enter \u{2014} it runs against the open connection"),
    ("ui.builtin_theme", "the built-in theme is the generated LvimBase_dark"),
    ("wallet.locked", "The keyring is shut. Open it and the secret goes straight in."),
    ("wallet.need_pass", "a passphrase is needed"),
    ("wallet.unlocking", "opening the keyring\u{2026}"),
    ("wallet.open", "the keyring is open"),
    ("edit.keys", "ctrl-enter saves \u{b7} escape leaves it"),
    ("result.row", "row"),
    ("json.bad", "that is not valid JSON"),
    ("json.not_object", "a row has to be an object of fields"),
    ("result.unchanged", "nothing changed"),
    ("ui.edit_row", "edit the whole row"),
    ("ddl.unknown", "the {driver} driver has no {op}"),
    ("ddl.write", "write the statement"),
    ("server.quiet", "the server reports nothing here just now"),
    ("chart.needs", "a chart needs one column of numbers and one to name the bars"),
    ("builder.needs_object", "the builder works on an object — open a table from the tree"),
    ("schema.not_object", "this tab is a statement, not an object — there is nothing to describe"),
    ("builder.above_runs", "the statement above is what runs — edit it and these stop rewriting it"),
    ("engine.back", "the engine is back"),
    ("engine.restarting", "the engine exited — starting it again"),
    ("toast.board_saved", "dashboard saved"),
    ("toast.ddl_copied", "DDL copied"),
    ("toast.no_ddl", "this driver answered no DDL"),
    ("toast.conn_saved", "connection saved"),
    ("toast.export_busy_result", "an export of this result is running — it has to finish first"),
    ("toast.nothing_to_save", "there is nothing to save yet"),
    ("toast.name_file", "name a file first"),
    ("toast.export_busy", "an export is already running"),
    ("toast.run_first", "run something first"),
    ("toast.nothing_to_draw", "nothing to draw — this connection has no tables"),
    ("toast.cell_copied", "cell copied"),
    ("toast.nothing_to_copy", "nothing to copy"),
    ("toast.row_copied", "row copied"),
    ("toast.no_charts", "there are no saved charts on this connection yet — save a chart first"),
    ("toast.time_range", "a time range needs a column, a start and an end"),
    ("toast.name_time_column", "name the time column first"),
    ("toast.board_needs_name", "the dashboard needs a name before it can be saved"),
    ("toast.no_refs", "there are no references to copy"),
    ("toast.pick_row", "pick a row first"),
    ("toast.nothing_connected", "nothing connected"),
    ("toast.copied", "copied"),
    ("engine.gone", "the engine exited and would not start again: {why}. Its drivers run in a separate process, so this window survived it."),
    ("toast.row_select", "row copied as SELECT"),
    ("multi.order", "They run in order, each in a tab of its own, and the run STOPS at the first failure."),
    ("console.policy", "the statement above is what runs — it goes through the connection’s policy like anything else"),
    ("engine.build_it", "Build it with scripts/build and put loded on $PATH, or set $LODE_DAEMON."),
    ("settings.faded_means", "Faded means the driver says it cannot. The engine refuses those rather than letting the window offer them."),
    ("settings.lang_record", "the engine's refusals and the audit log stay in English — they are the record, not the interface"),
    ("settings.profile_by_name", "A connection picks one by name in its Access tab. A connection whose profile is missing FAILS rather than falling back to a direct link."),
    ("settings.one_store", "The saved connections, queries, charts, dashboards, scripts, history and audit are all in that one file, and it is the engine's — two clients share it."),
    ("settings.keys_howto", "Press a row to change its key, then type the new one and press Enter. `escape` leaves it alone; an empty answer puts the default back."),
    ("settings.audit_what", "What is written: every statement that changed anything, every refusal, and every connect and disconnect. Reads are not in it. There is no method to delete from it."),
    ("settings.audit_panel", "The rail's Audit panel is the log itself — ctrl-shift-a."),
    ("settings.no_drivers", "No driver programs are registered. One is a `driver` profile holding a command line; the engine asks it to describe itself at startup and it then appears like any other driver."),
    ("settings.import_store", "A database written by the Neovim client can be read in: the connections, queries and history that are missing are copied, keyed by name, so running it twice imports nothing the second time. The source file is only read."),
    ("settings.wallet_agent", "lode starts the agent itself when it can find it: at $LVIM_KEYRING_DAEMON, on $PATH, or beside the engine. `scripts/build-wallet` builds it from the same source the Neovim plugin uses — it is one program, one vault file and one socket, so a secret saved in either client opens in the other."),
    ("settings.wallet_passphrase", "A passphrase makes one. It is the only thing that opens it — there is no copy anywhere and nothing can recover it, which is what makes the wallet worth having."),
    ("settings.wallet_idle", "It shuts itself after its own idle timeout, and whatever this window is holding is dropped with it."),
    ("settings.secret_as_typed", "A connection's password is stored as typed unless it points somewhere else. The key beside a secret field in the connection window moves it into this wallet and leaves a reference behind."),
    ("settings.no_aws", "Nothing found. The engine reads ~/.aws/config and ~/.aws/credentials."),
    ("settings.aws_whole_list", "The whole list is shown and the ones that will not work are marked, rather than offering eight of which three fail only when picked. An import stores the profile's NAME, never its key: the credential stays where the AWS tools rotate it."),
    ("settings.mcp_same_engine", "An agent speaking MCP runs against the SAME engine: same drivers, same store, same classify, same connection policy, same audit log. What differs only ever restricts — a role (`reader` by default, $LODE_MCP_ROLE), an unrestricted policy read as `confirm`, and an agent that CANNOT confirm. A statement classify names is not run: it is filed here for a person."),
    ("settings.hooks_what", "A hook is your own code run at one of four moments: before and after a connect, before and after a disconnect. A PreConnect hook that fails stops the connection, which is the only reason to have one before rather than after."),
    ("settings.hooks_env", "The environment a child gets carries CONTEXT and never a credential — a child's environment is readable from `ps` and `/proc`."),
    ("settings.hooks_per_conn", "They are set per connection, in its own window. Global hooks are not built."),
    ("write.delete_it", "Delete it"),
    ("palette.view_as", "View as"),
    ("sub.documents",  "Documents"),
    ("sub.schema",     "Schema"),
    ("sub.aggregate",  "Aggregate"),
    ("sub.builder",    "Builder"),
    ("view.tree",      "Tree"),
    ("view.table",     "Table"),
    ("view.json",      "JSON"),
    ("view.chart",     "Chart"),
    ("instance.no_metrics", "reports no numbers about its server \u{2014} what it is RUNNING is above"),
    ("panel.log",     "Log"),
    ("rail.tasks",       "Tasks"),
    ("rail.settings",    "Settings"),
    // Panels.
    ("panel.filter",         "Filter connections and schema…"),
    ("panel.new_connection", "New connection"),
    ("panel.new_dashboard",  "New dashboard"),
    ("panel.diagram",    "The schema as a picture"),
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
    ("tip.to_keyring",             "Put it in the keyring"),
    ("tip.to_keyring_needs_name",  "Name the connection first \u{2014} the secret is named after it"),
    ("tip.to_keyring_needs_value", "Nothing typed to store"),
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
    ("tip.link_tunnel", "encrypted (SSH tunnel)"),
    ("tip.link_plain", "PLAINTEXT — anyone on the wire can read this"),
    ("toast.plaintext", "plaintext connection: credentials and rows go over the wire unencrypted"),
    ("settings.close",        "Close"),
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
    ("form.tab_hooks",         "Закачки"),
    ("hook.add",               "+  закачка"),
    ("hook.when",              "Кога"),
    ("hook.what",              "Какво"),
    ("hook.timeout",           "Краен срок (ms)"),
    ("hook.none",              "Нищо не се изпълнява при отваряне или затваряне на тази връзка."),
    ("hook.blocking",          "Провал тук СПИРА връзката."),
    ("hook.not_blocking",      "Провал тук се съобщава; не спира нищо."),
    ("hook.cmd_hint",          "команден ред, през шел"),
    ("hook.script_hint",       "име на запазен скрипт"),
    ("hook.no_scripts",        "Още няма запазени скриптове за тази връзка."),
    ("hook.output_goes",       "Каквото закачка изпише, идва в дневника, докато тя работи."),
    ("hook.remove",            "махни"),
    ("form.tab_main",      "Основни"),
    ("form.tab_access",    "Достъп (SSH)"),
    ("form.tab_settings",  "Настройки"),
    ("form.auth",          "Удостоверяване"),
    ("form.provider",      "Доставчик"),
    ("form.token",         "Токен"),
    ("form.test_proxy",        "Провери проксито"),
    ("form.no_proxies",        "Няма запазени проксита \u{2014} Настройки \u{203a} Проксита прави едно"),
    ("form.proxy_written_out", "Тази връзка носи прокси, изписано в спецификацията си; избор отгоре би го заменил:"),
    ("form.proxy_kind",     "Протокол"),
    ("form.proxy_host",     "Хост на проксито"),
    ("form.proxy_port",     "Порт на проксито"),
    ("form.proxy_auth_opt", "Вписване само ако проксито иска \u{2014} остави и двете празни за никакво"),
    ("form.proxy",          "Прокси"),
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
    ("form.none",          "никакво"),
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
    ("menu.edit_doc",      "Редактирай целия ред"),
    ("menu.delete_row",    "Изтрий този ред\u{2026}"),
    ("menu.close_tab",     "Затвори"),
    ("menu.close_others",  "Затвори другите"),
    ("menu.duplicate_tab", "Дублирай"),
    ("rail.connections", "Връзки"),
    ("rail.instance",    "Сървър"),
    ("rail.history",     "История"),
    ("rail.audit",       "Одит"),
    ("rail.scripts",   "Скриптове"),
    ("script.new",     "+  скрипт"),
    ("script.run",     "Пусни"),
    ("script.cancel",  "Прекрати"),
    ("script.save",    "Запази"),
    ("script.none",    "Още няма запазени скриптове."),
    ("script.none_hint", "Натисни + и напиши един — bash, python или lua, изпълняван от двигателя."),
    ("script.global",  "всяка връзка"),
    ("script.running", "работи\u{2026}"),
    ("script.silent",  "Не изписа нищо."),
    ("ui.create", "CREATE"),
    ("ui.where", "Където"),
    ("ui.order", "Подредба"),
    ("ui.show", "Покажи"),
    ("ui.columns", "Колони"),
    ("ui.indexes", "Индекси"),
    ("ui.instance", "Сървър"),
    ("ui.copy", "копирай"),
    ("ui.copy_all", "копирай всичко"),
    ("ui.cancel", "Откажи"),
    ("ui.edit", "редакция"),
    ("ui.remove", "махни"),
    ("ui.condition", "+ условие"),
    ("ui.just_first", "само първото"),
    ("ui.lay_out", "подреди наново"),
    ("ui.copy_refs", "копирай препратките"),
    ("ui.test", "провери"),
    ("ui.import", "внеси"),
    ("ui.key", "ключ"),
    ("ui.unique", "уникален"),
    ("ui.no_engine", "Няма двигател"),
    ("ui.every_column", "всяка колона"),
    ("ui.asking", "пита се\u{2026}"),
    ("ui.read_every", "чети на всеки"),
    ("ui.reading", "чете се\u{2026}"),
    ("ui.running", "работи\u{2026}"),
    ("ui.reading_schema", "чете се схемата\u{2026}"),
    ("ui.not_loaded", "не е заредено"),
    ("ui.measure", "мярка"),
    ("ui.takes_no_value", "не иска стойност"),
    ("ui.not_answered", "двигателят още не е отговорил"),
    ("ui.no_sql", "този драйвер не говори SQL"),
    ("ui.nothing_matches", "нищо не съвпада"),
    ("ui.no_icons", "без икони"),
    ("ui.capabilities", "ВЪЗМОЖНОСТИ"),
    ("ui.remove_it", "махни го"),
    ("ui.keep_it", "запази го"),
    ("ui.choose_file", "Избери файл\u{2026}"),
    ("ui.make_wallet", "Направи трезор"),
    ("ui.unlock", "Отключи"),
    ("ui.lock_now", "Заключи сега"),
    ("ui.driver", "Драйвер"),
    ("ui.parameters", "Параметри"),
    ("ui.version", "Версия"),
    ("ui.protocol", "Протокол"),
    ("ui.binary", "Изпълним файл"),
    ("ui.path", "Път"),
    ("ui.keeping", "Пази"),
    ("ui.the_agent", "Агентът"),
    ("ui.the_wallet", "Трезорът"),
    ("ui.agent_proto", "Протокол на агента"),
    ("ui.the_engine", "ДВИГАТЕЛЯТ"),
    ("ui.the_store", "ХРАНИЛИЩЕТО"),
    ("ui.older_store", "ПО-СТАРО ХРАНИЛИЩЕ"),
    ("ui.where_secrets", "КЪДЕ ОТИВАТ ТАЙНИТЕ"),
    ("ui.browse", "прегледай\u{2026}"),
    ("ui.save", "Запази"),
    ("ui.cancel_b", "Откажи"),
    ("ui.password", "Парола"),
    ("ui.engine_restart", "Промените влизат в сила при рестарт на двигателя."),
    ("ui.nothing_waiting", "Нищо не чака отговор."),
    ("ui.decide_not_built", "Решаването им от този прозорец още не е направено."),
    ("ph.filter_conns", "Филтър по връзки и схема\u{2026}"),
    ("ph.search_history", "Търси в историята\u{2026}"),
    ("ph.search_audit", "Търси в одита\u{2026}"),
    ("ph.name_dashboard", "име на таблото"),
    ("ph.tile_heading", "заглавие на плочката"),
    ("ph.column", "колона"),
    ("ph.find_command", "Пиши, за да намериш команда\u{2026}"),
    ("ph.name_enter", "наименувай и натисни Enter"),
    ("ph.value", "стойност"),
    ("ph.search_settings", "Търси в настройките\u{2026}"),
    ("ph.filter_fonts", "филтър по шрифтове\u{2026}"),
    ("ph.passphrase", "парола за трезора"),
    ("ph.console", "Напиши команда и натисни Enter \u{2014} изпълнява се срещу отворената връзка"),
    ("ui.builtin_theme", "вградената тема е генерираната LvimBase_dark"),
    ("wallet.locked", "Ключодържателят е заключен. Отключи го и тайната влиза веднага."),
    ("wallet.need_pass", "нужна е парола"),
    ("wallet.unlocking", "отключване\u{2026}"),
    ("wallet.open", "ключодържателят е отворен"),
    ("edit.keys", "ctrl-enter записва \u{b7} escape отказва"),
    ("result.row", "ред"),
    ("json.bad", "това не е валиден JSON"),
    ("json.not_object", "редът трябва да е обект от полета"),
    ("result.unchanged", "нищо не е променено"),
    ("ui.edit_row", "редактирай целия ред"),
    ("ddl.unknown", "драйверът {driver} няма {op}"),
    ("ddl.write", "напиши заявката"),
    ("server.quiet", "сървърът не съобщава нищо тук в момента"),
    ("chart.needs", "диаграмата иска една колона с числа и една, която да именува стълбовете"),
    ("builder.needs_object", "конструкторът работи върху обект — отвори таблица от дървото"),
    ("schema.not_object", "този таб е заявка, не обект — няма какво да се опише"),
    ("builder.above_runs", "горната заявка е това, което се изпълнява — редактираш ли я, тези спират да я пренаписват"),
    ("engine.back", "двигателят се върна"),
    ("engine.restarting", "двигателят излезе — пускам го отново"),
    ("toast.board_saved", "таблото е запазено"),
    ("toast.ddl_copied", "DDL е копиран"),
    ("toast.no_ddl", "този драйвер не върна DDL"),
    ("toast.conn_saved", "връзката е запазена"),
    ("toast.export_busy_result", "изнасяне на този резултат върви — първо трябва да свърши"),
    ("toast.nothing_to_save", "още няма какво да се запази"),
    ("toast.name_file", "първо дай име на файл"),
    ("toast.export_busy", "вече върви изнасяне"),
    ("toast.run_first", "първо пусни нещо"),
    ("toast.nothing_to_draw", "няма какво да се нарисува — тази връзка няма таблици"),
    ("toast.cell_copied", "клетката е копирана"),
    ("toast.nothing_to_copy", "няма какво да се копира"),
    ("toast.row_copied", "редът е копиран"),
    ("toast.no_charts", "тази връзка още няма запазени диаграми — първо запази диаграма"),
    ("toast.time_range", "времевият обхват иска колона, начало и край"),
    ("toast.name_time_column", "първо назови времевата колона"),
    ("toast.board_needs_name", "таблото иска име, преди да може да се запази"),
    ("toast.no_refs", "няма препратки за копиране"),
    ("toast.pick_row", "първо избери ред"),
    ("toast.nothing_connected", "няма свързано"),
    ("toast.copied", "копирано"),
    ("engine.gone", "двигателят излезе и не тръгна отново: {why}. Драйверите му вървят в отделен процес, затова този прозорец го надживя."),
    ("toast.row_select", "редът е копиран като SELECT"),
    ("multi.order", "Изпълняват се по ред, всяка в свой таб, и изпълнението СПИРА при първата грешка."),
    ("console.policy", "горната заявка е това, което се изпълнява — минава през политиката на връзката като всичко друго"),
    ("engine.build_it", "Сглоби го със scripts/build и сложи loded на $PATH, или задай $LODE_DAEMON."),
    ("settings.faded_means", "Избледнялото значи, че драйверът казва, че не може. Двигателят ги отказва, вместо прозорецът да ги предлага."),
    ("settings.lang_record", "отказите на двигателя и одитният дневник остават на английски — те са записът, не интерфейсът"),
    ("settings.profile_by_name", "Връзката избира профил по име в таба Достъп. Връзка, чийто профил липсва, се ПРОВАЛЯ, вместо да падне до пряка връзка."),
    ("settings.one_store", "Запазените връзки, заявки, диаграми, табла, скриптове, история и одит са в този един файл, и той е на двигателя — два клиента го споделят."),
    ("settings.keys_howto", "Натисни ред, за да смениш клавиша му, после напиши новия и натисни Enter. `escape` го оставя; празен отговор връща подразбирания."),
    ("settings.audit_what", "Какво се записва: всяка заявка, която е променила нещо, всеки отказ, и всяко свързване и разкачане. Четенията не влизат. Няма метод, който да трие от него."),
    ("settings.audit_panel", "Панелът Одит в лентата е самият дневник — ctrl-shift-a."),
    ("settings.no_drivers", "Няма регистрирани програми-драйвери. Такава е профил `driver` с команден ред; двигателят я пита да се опише при пускане и тя после се появява като всеки друг драйвер."),
    ("settings.import_store", "База, писана от клиента за Neovim, може да се прочете: липсващите връзки, заявки и история се копират по име, така че второ пускане не внася нищо. Изходният файл само се чете."),
    ("settings.wallet_agent", "lode сам пуска агента, когато го намери: на $LVIM_KEYRING_DAEMON, на $PATH, или до двигателя. `scripts/build-wallet` го сглобява от същия код, който ползва приставката за Neovim — една програма, един файл-трезор и един сокет, така че тайна, запазена в единия клиент, се отваря в другия."),
    ("settings.wallet_passphrase", "Парола го създава. Тя е единственото, което го отваря — няма копие никъде и нищо не може да я възстанови, и точно това прави трезора ценен."),
    ("settings.wallet_idle", "Заключва се сам след собствения си срок на бездействие, и каквото този прозорец държи, пада заедно с него."),
    ("settings.secret_as_typed", "Паролата на връзката се пази както е въведена, освен ако не сочи другаде. Ключето до тайно поле в прозореца на връзката я премества в този трезор и оставя препратка."),
    ("settings.no_aws", "Нищо не е намерено. Двигателят чете ~/.aws/config и ~/.aws/credentials."),
    ("settings.aws_whole_list", "Показва се целият списък, а тези, които няма да работят, са отбелязани — вместо да се предлагат осем, от които три се провалят чак при избор. Внасянето пази ИМЕТО на профила, никога ключа му: удостоверението остава там, където инструментите на AWS го въртят."),
    ("settings.mcp_same_engine", "Агент, който говори MCP, работи срещу СЪЩИЯ двигател: същите драйвери, същото хранилище, същата класификация, същата политика на връзките, същия одитен дневник. Разликите само ограничават — роля (`reader` по подразбиране, $LODE_MCP_ROLE), неограничена политика, четена като `confirm`, и агент, който НЕ МОЖЕ да потвърждава. Заявка, която класификацията посочи, не се изпълнява: тя се завежда тук за човек."),
    ("settings.hooks_what", "Кука е твой собствен код, пуснат в един от четири момента: преди и след свързване, преди и след разкачане. Кука PreConnect, която се провали, спира връзката — и това е единствената причина да има такава преди, а не след."),
    ("settings.hooks_env", "Средата, която детето получава, носи КОНТЕКСТ и никога удостоверение — средата на дете се чете от `ps` и `/proc`."),
    ("settings.hooks_per_conn", "Задават се за всяка връзка в нейния прозорец. Глобални куки няма."),
    ("write.delete_it", "Изтрий"),
    ("palette.view_as", "Покажи като"),
    ("sub.documents",  "Документи"),
    ("sub.schema",     "Схема"),
    ("sub.aggregate",  "Агрегат"),
    ("sub.builder",    "Конструктор"),
    ("view.tree",      "Дърво"),
    ("view.table",     "Таблица"),
    ("view.json",      "JSON"),
    ("view.chart",     "Диаграма"),
    ("instance.no_metrics", "не отчита числа за сървъра си \u{2014} какво ИЗПЪЛНЯВА е отгоре"),
    ("panel.log",     "Дневник"),
    ("rail.tasks",       "Задачи"),
    ("rail.settings",    "Настройки"),

    ("panel.filter",         "Филтър за връзки и схема…"),
    ("panel.new_connection", "Нова връзка"),
    ("panel.new_dashboard",  "Ново табло"),
    ("panel.diagram",    "Схемата като картина"),
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
    ("tip.to_keyring",             "Сложи го в ключодържателя"),
    ("tip.to_keyring_needs_name",  "Първо наименувай връзката \u{2014} тайната се казва на нейно име"),
    ("tip.to_keyring_needs_value", "Няма какво да се запише"),
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
    ("tip.link_tunnel", "шифрована (SSH тунел)"),
    ("tip.link_plain", "НЕШИФРОВАНА — всеки по пътя може да я чете"),
    ("toast.plaintext", "нешифрована връзка: паролата и редовете минават по мрежата открити"),
    ("settings.close",        "Затвори"),
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
        // nobody writes. `JSON` is a format's name and the same four letters
        // for the same reason — "ДжейСън" is nothing anybody has ever typed.
        // `CREATE` is the SQL keyword, not the English verb: the heading
        // sits over a `CREATE TABLE` statement, and translating it would
        // label a statement with a word that is not in it.
        const SAME_ON_PURPOSE: [&str; 3] = ["form.ssh", "view.json", "ui.create"];
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
