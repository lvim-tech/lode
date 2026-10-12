//! The connection form, in a window of its own.
//!
//! Not a layer painted over the workspace: a real window, with a real title, that
//! the window manager owns and can tile, move and tab like any other. In GPUI
//! that is not a feature to build — `cx.open_window` is the only way to show
//! anything and there is no modal-layer concept — so it is simply what a dialog
//! is here.
//!
//! Three tabs, because the previous client's one long scrolling column was the
//! complaint: **Main** is the server and how to prove who you are, **Access** is
//! how the connection gets there (TLS, SSH tunnel), **Settings** is the rest.
//!
//! Every field that can hold a credential has a SOURCE in front of it. That is
//! the part that matters: the engine already resolves `{{ env … }}`,
//! `{{ cmd … }}` and `{{ vault … }}` (the lvim-keyring wallet), and until now
//! there was no way in the client to say "this password comes from the wallet".
//! The list offers only what the engine actually resolves, so it cannot promise
//! a source that will fail at connect time.

use std::collections::HashMap;

use gpui::{
    div, prelude::*, px, Context, Entity, SharedString, WeakEntity, Window,
};
use lode_proto::DriverMeta;
use serde_json::{json, Value};

use crate::app::Workspace;
use crate::i18n::Lang;
use crate::input::{InputColors, InputEvent, TextInput};
use crate::metrics::Metrics;
use crate::theme::Theme;
use crate::widgets::{btn, chip, Btn};

/// Where a value comes from. The engine's `secret.rs` resolves exactly these,
/// which is why the list is this and not a wish.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Literal,
    Env,
    Command,
    Keyring,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Literal => "Literal",
            Source::Env => "Env",
            Source::Command => "Command",
            Source::Keyring => "Keyring",
        }
    }

    pub fn all() -> [Source; 4] {
        [Source::Literal, Source::Env, Source::Command, Source::Keyring]
    }

    /// What the field holds in this mode.
    ///
    /// Four different things wearing one box. Under `Env` it is the NAME of a
    /// variable, under `Command` a command line, under `Keyring` a path in the
    /// keyring, and only under `Literal` is it the credential itself. The field
    /// looked identical in all four, so nothing said which was expected.
    pub fn field_label(self) -> &'static str {
        match self {
            Source::Literal => "Value",
            Source::Env => "Variable",
            Source::Command => "Command",
            Source::Keyring => "Keyring path",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Source::Literal => "the value itself",
            Source::Env => "PGPASSWORD",
            Source::Command => "aws rds generate-db-auth-token …",
            Source::Keyring => "prod/db",
        }
    }
}

/// One hook as this window edits it.
///
/// The engine's `Hook` is the shape that goes out; this is the same thing with
/// live fields in it. Kept apart because a `TextInput` cannot be serialised and
/// because the three kinds share one `source` box — switching kind must not
/// carry a command line into a script name, which is the same mistake the
/// secret sources make and the same cure: one stash per kind.
pub struct HookRow {
    /// `pre_connect`, `post_connect`, `pre_disconnect`, `post_disconnect`.
    pub on: String,
    /// `command`, `script` or `inline`.
    pub kind: String,
    /// A command line or a script's name. The INLINE body lives in `body`,
    /// because one line is not where somebody writes a script.
    pub source: Entity<TextInput>,
    pub body: Entity<crate::editor_el::Editor>,
    /// For an inline hook: `bash`, `python` or `lua`.
    pub lang: String,
    pub timeout: Entity<TextInput>,
    /// What was typed under the OTHER kinds, so switching back and forth does
    /// not lose it. See `Stash`.
    pub stash: std::collections::HashMap<String, String>,
}

/// The four moments, in the order they happen.
pub const HOOK_EVENTS: &[&str] =
    &["pre_connect", "post_connect", "pre_disconnect", "post_disconnect"];

/// Does a failure at this moment stop what was about to happen?
///
/// Only before the dial. The other three run after the thing they are named
/// for, so there is nothing left to stop — and refusing to disconnect because
/// a hook failed would leave open a connection somebody asked to close. The
/// engine decides this; the window only has to say it, and saying the wrong
/// thing is worse than saying nothing.
pub fn hook_blocks(on: &str) -> bool {
    on == "pre_connect"
}

/// The hooks a spec carries, as rows, and what was in the spec that this window
/// cannot show.
///
/// A hook whose `on` or `kind` the engine does not know is NOT dropped: it is
/// returned as a leftover and written back untouched. A client that silently
/// deleted what it failed to parse would turn "lode is older than this spec"
/// into "the spec lost half its hooks".
pub fn read_hooks(spec: &Value) -> (Vec<Value>, Vec<Value>) {
    let all = spec.get("hooks").and_then(Value::as_array).cloned().unwrap_or_default();
    let known = |h: &Value| {
        let on = h.get("on").and_then(Value::as_str).unwrap_or_default();
        let kind = h.get("kind").and_then(Value::as_str).unwrap_or("command");
        HOOK_EVENTS.contains(&on) && ["command", "script", "inline"].contains(&kind)
    };
    all.iter().cloned().partition(known)
}

/// What a secret is called in the wallet.
///
/// `db/<connection>` for the password, which is the name lvim-db uses — so the
/// Neovim client finds the same secret rather than a second copy of it. Every
/// other secret a connection can have hangs below that, because one name for
/// four secrets is one secret overwritten three times.
pub fn vault_name(conn: &str, field: &str) -> String {
    let conn = conn.trim();
    match field {
        "auth-password" | "password" => format!("db/{conn}"),
        // `auth-` says only where the field lives in this window; it is noise
        // in a name somebody has to read in the wallet.
        other => format!("db/{conn}/{}", other.trim_start_matches("auth-")),
    }
}

/// One value, kept per source.
///
/// Switching `Literal → Env → Literal` used to show the variable name where the
/// password had been and then save it as the password. Each mode keeps its own
/// text, so moving between them loses nothing and shows nothing of the others.
#[derive(Default)]
pub struct Stash {
    by_source: HashMap<Source, String>,
}

impl Field {
    /// A secret field with an empty stash. The stash fills as somebody moves
    /// between sources; it starts empty because nothing has been typed yet.
    pub fn secret(input: Entity<TextInput>, source: Source) -> Field {
        Field { input, source, stash: Stash::default(), secret: true }
    }
}

impl Stash {
    fn put(&mut self, s: Source, v: String) {
        self.by_source.insert(s, v);
    }

    fn take(&self, s: Source) -> String {
        self.by_source.get(&s).cloned().unwrap_or_default()
    }
}

impl Source {
    /// Wrap what was typed in the template the engine understands. A literal is
    /// stored as typed, which is also what makes a literal distinguishable from
    /// a template that happens to look like one.
    pub fn wrap(self, raw: &str) -> String {
        let raw = raw.trim();
        if raw.is_empty() {
            return String::new();
        }
        match self {
            Source::Literal => raw.to_string(),
            Source::Env => format!("{{{{ env \"{raw}\" }}}}"),
            Source::Command => format!("{{{{ cmd \"{raw}\" }}}}"),
            Source::Keyring => format!("{{{{ vault \"{raw}\" }}}}"),
        }
    }

    /// Read a stored value back into a source and its bare argument, so editing
    /// a saved connection shows `Keyring` + `prod/db` rather than the raw
    /// template.
    pub fn unwrap(stored: &str) -> (Source, String) {
        let s = stored.trim();
        for (src, kw) in [
            (Source::Env, "env"),
            (Source::Command, "cmd"),
            (Source::Keyring, "vault"),
        ] {
            let prefix = format!("{{{{ {kw} \"");
            if let Some(rest) = s.strip_prefix(&prefix) {
                if let Some(arg) = rest.strip_suffix("\" }}") {
                    return (src, arg.to_string());
                }
            }
        }
        (Source::Literal, s.to_string())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FormTab {
    Main,
    Access,
    /// The person's own code, run when this connection opens or closes.
    Hooks,
    Settings,
}

impl FormTab {
    /// The catalogue KEY, not the word: the tab strip is this window's own
    /// chrome and a window in Bulgarian showing three English tabs is what the
    /// i18n note in `i18n.rs` is about.
    fn key(self) -> &'static str {
        match self {
            FormTab::Main => "form.tab_main",
            FormTab::Access => "form.tab_access",
            FormTab::Hooks => "form.tab_hooks",
            FormTab::Settings => "form.tab_settings",
        }
    }

    /// A stable id for the element, which must NOT follow the language — an id
    /// that changes with the locale is a different element to GPUI.
    fn id(self) -> &'static str {
        match self {
            FormTab::Main => "main",
            FormTab::Access => "access",
            FormTab::Hooks => "hooks",
            FormTab::Settings => "settings",
        }
    }
}

/// One editable value: its own field, and where it comes from.
pub struct Field {
    pub input: Entity<TextInput>,
    pub source: Source,
    /// What was typed under the OTHER sources, so switching back and forth
    /// keeps it. Without this, a glance at `Env` and back costs the password.
    pub stash: Stash,
    /// Whether a source may be chosen at all. A host is a host; a password is
    /// the kind of thing that comes from a wallet.
    secret: bool,
}

pub struct ConnectionForm {
    workspace: WeakEntity<Workspace>,
    /// What the engine speaks, for the one button that needs a new engine.
    proto: u32,
    /// Whether this window refuses to save a password typed as itself.
    secrets_from_a_source: bool,
    /// The field whose secret is waiting on a locked wallet, if any. The row
    /// to unlock it appears while this is set.
    unlocking: Option<String>,
    /// The passphrase box, masked from the moment it is built.
    pass_input: Entity<TextInput>,
    theme: Theme,
    /// The same type scale as the workspace, handed over when the window opens:
    /// a dialog in its own OS window is still the same application, and a form
    /// at a different size from the panel behind it reads as a different one.
    scale: Metrics,
    /// The two fonts the main window resolved. Handed over for the same reason
    /// the scale is: one application, one face. The root used to name a font
    /// outright, which is a second decision about something already decided
    /// once, by `$LODE_FONT`.
    font_family: String,
    icon_family: String,
    /// The interface's language. Handed over like the theme and the fonts, and
    /// updated by `set_lang` when it changes behind this window — a dialog in
    /// its own OS window is still the same application, and a form in English
    /// beside a panel in Bulgarian reads as a different one.
    lang: Lang,
    tab: FormTab,
    editing: Option<String>,
    driver: String,
    drivers: Vec<DriverMeta>,

    name: Entity<TextInput>,
    /// Driver parameters, by key.
    params: HashMap<String, Field>,
    auth_kind: String,
    /// Shared on purpose: every kind that has a user calls it `user` and means
    /// the same thing by it. Shared where the ENGINE shares it, nowhere else.
    auth_user: Entity<TextInput>,
    /// `password` — and only `password`.
    auth_password: Field,
    /// `client_cert`: a certificate, a key and the key's password. `cert` and
    /// `key` used to be written out as hardcoded empty strings, which made a
    /// client-certificate connection impossible through this window — the same
    /// shape of fault as the blanked tunnel passphrase.
    auth_cert: Entity<TextInput>,
    auth_key: Entity<TextInput>,
    auth_key_password: Field,
    /// `provider`: which one, and its token. The provider used to be hardcoded
    /// to the literal string "token", so the engine's real providers could not
    /// be chosen at all.
    auth_provider: String,
    auth_token: Field,
    /// `kerberos`: a principal, which used to be written out as null.
    auth_principal: Entity<TextInput>,
    tls_mode: String,
    /// The three files a verified link can need: the CA that signs the server,
    /// and a client certificate with its key. `TlsSpec` has carried them since
    /// the engine's first version and the README promises them; this window had
    /// no way to type one, so a connection that needed a CA could only be made
    /// elsewhere — and then this form deleted it on the next save.
    tls_ca: Entity<TextInput>,
    tls_cert: Entity<TextInput>,
    tls_key: Entity<TextInput>,
    /// "read_only" | "confirm" | "open" — what the engine will let this
    /// connection do. It belongs on the connection, not on a preference, so it
    /// travels with the connection wherever it is opened from.
    policy: String,
    tunnel_on: bool,
    /// A SAVED tunnel, by name, instead of one spelled out here.
    ///
    /// The engine has resolved these since protocol 9 and no client ever
    /// offered them: a bastion is one machine and twenty connections go through
    /// it, and writing its host, user and key into twenty specs means changing
    /// twenty of them when the key rotates.
    tunnel_profile: Option<String>,
    /// The person's own code, run when this connection opens or closes.
    hooks: Vec<HookRow>,
    /// Hooks this window could not read, kept whole and written back out.
    hooks_kept: Vec<Value>,
    /// The saved scripts a `script` hook can name, handed over when the window
    /// opens. Names only — the window does not run them.
    scripts: Vec<String>,
    /// Which saved PROXY this connection goes through, if any.
    ///
    /// A profile only. A proxy spelled out in the spec is a shape this window
    /// does not edit — it reports it and leaves it alone, the way it treats a
    /// client certificate it did not write.
    proxy_profile: Option<String>,
    /// The saved proxies to choose from, handed over when the window opens.
    proxy_profiles: Vec<String>,
    /// The names the workspace knows about, handed over when the window opens.
    tunnel_profiles: Vec<String>,
    /// Their specs too, so a chosen one can SHOW what it is. A name alone tells
    /// somebody nothing about what their connection goes through.
    tunnel_profile_specs: std::collections::BTreeMap<String, Value>,
    /// The AWS profile names the engine read out of `~/.aws/config`, offered for
    /// any parameter a driver declares as `ParamType::AwsProfile`.
    aws_profiles: Vec<String>,

    /// The name to save the fields above under.
    profile_name: Entity<TextInput>,
    tunnel_host: Entity<TextInput>,
    tunnel_port: Entity<TextInput>,
    tunnel_user: Entity<TextInput>,
    tunnel_auth: String,
    /// The key FILE's path. A path, not a credential — so it is a plain field
    /// with no source picker, and it is its own field.
    ///
    /// One field used to back both this and the password, and the method
    /// decided which it meant. That is how a passphrase became a key path: the
    /// form loaded the passphrase into the shared field (dropping the path
    /// entirely), and saving wrote whatever was in that field back out as
    /// `path`. Opening a working connection and pressing save was enough to
    /// break it. Separate fields cannot do that to each other.
    tunnel_key_path: Entity<TextInput>,
    /// The key's passphrase, which used to be written out as a hardcoded `""` —
    /// so a passphrase-protected key could not be used at all.
    tunnel_passphrase: Field,
    tunnel_password: Field,

    /// The spec this form opened on, kept whole.
    ///
    /// **A form edits the fields it knows; it must not delete the ones it does
    /// not.** A spec can carry `tls.ca`, a client certificate, a spelled-out
    /// `proxy` and a hook of a kind this build never heard of — written by the
    /// Neovim client, by hand, or by a newer version of this window — and
    /// saving used to send only what these inputs hold, which silently dropped
    /// the rest. Everything edited
    /// here is merged ONTO this (see [`merge_spec`]); empty for a new one.
    existing_spec: Value,

    /// The last answer from each `conn.test` stage.
    tests: HashMap<&'static str, Result<String, String>>,
    error: Option<String>,
    /// What just happened that is not a failure — the wallet taking a secret,
    /// so far. Separate from `error` because a success printed in the error
    /// line reads as a failure whatever it says.
    note: Option<String>,
}

/// The spec to save: `edited` laid over `existing`, key by key.
///
/// A key whose edited value is `Null` is REMOVED rather than written as null —
/// that is how the form says "this connection no longer has a tunnel". `tls` is
/// the one key merged rather than replaced: the form owns `mode` and the paths
/// it shows, and anything else under `tls` belongs to whoever put it there.
pub fn merge_spec(existing: &Value, edited: Value) -> Value {
    let mut out = match existing {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    let Value::Object(edits) = edited else { return Value::Object(out) };
    for (key, value) in edits {
        if key == "tls" {
            let mut tls = match out.get("tls") {
                Some(Value::Object(m)) => m.clone(),
                _ => serde_json::Map::new(),
            };
            if let Value::Object(edited_tls) = value {
                for (k, v) in edited_tls {
                    if v.is_null() {
                        tls.remove(&k);
                    } else {
                        tls.insert(k, v);
                    }
                }
            }
            out.insert("tls".into(), Value::Object(tls));
            continue;
        }
        if value.is_null() {
            out.remove(&key);
        } else {
            out.insert(key, value);
        }
    }
    Value::Object(out)
}

impl ConnectionForm {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        look: crate::widgets::Look,
        // What the engine speaks. Only `>= 17` can be asked to put a secret in
        // the wallet, and a button that would fail is a button not to draw.
        proto: u32,
        // Whether a password may be saved as typed. The window's own policy,
        // pushed in like everything else it is told.
        secrets_from_a_source: bool,
        drivers: Vec<DriverMeta>,
        tunnel_profiles: Vec<String>,
        tunnel_profile_specs: std::collections::BTreeMap<String, Value>,
        // The saved proxies to offer. Names only: the window picks a PROFILE
        // and never spells a proxy out, so it has no use for the specs.
        proxy_profiles: Vec<String>,
        // The saved scripts a hook can name.
        scripts: Vec<String>,
        aws_profiles: Vec<String>,
        existing: Option<(String, String, Value)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let crate::widgets::Look { theme, scale, text_font, icon_font, lang } = look;
        let colors = colors_of(&theme);
        let driver = existing
            .as_ref()
            .map(|(_, d, _)| d.clone())
            .or_else(|| drivers.first().map(|d| d.kind.clone()))
            .unwrap_or_else(|| "sqlite".into());

        let mk = |cx: &mut Context<Self>, ph: &str, val: &str| {
            let (ph, val) = (ph.to_string(), val.to_string());
            cx.new(|cx| {
                let mut i = TextInput::new(cx, ph, colors);
                if !val.is_empty() {
                    i.set_text(val, cx);
                }
                i
            })
        };

        let (name_v, spec) = match &existing {
            Some((n, _, s)) => (n.clone(), s.clone()),
            None => (String::new(), json!({})),
        };
        let auth = spec.get("auth").cloned().unwrap_or(json!({}));
        let tunnel = spec.get("tunnel").cloned().unwrap_or(Value::Null);
        let get = |v: &Value, k: &str| {
            v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
        };

        // Each value from ITS OWN key. The old code read `password`, fell back
        // to `token`, and wrote whichever it found back out under whatever the
        // chosen kind happened to be — the same read-one-write-another fault
        // that moved a passphrase into a key path.
        let (pw_src, pw_arg) = Source::unwrap(&get(&auth, "password"));
        let (kp_src, kp_arg) = Source::unwrap(&get(&auth, "key_password"));
        let (tok_src, tok_arg) = Source::unwrap(&get(&auth, "token"));
        let tunnel_auth_v = tunnel.get("auth").cloned().unwrap_or(json!({}));
        let (pass_src, pass_arg) = Source::unwrap(&get(&tunnel_auth_v, "passphrase"));
        let (tpw_src, tpw_arg) = Source::unwrap(&get(&tunnel_auth_v, "password"));
        let key_path = get(&tunnel_auth_v, "path");

        let tls_v = spec.get("tls").cloned().unwrap_or(json!({}));
        let pass_input = cx.new(|cx| {
            let mut i = TextInput::new(cx, lang.tr("ph.passphrase"), colors);
            i.set_mask(true, cx);
            i
        });
        let mut form = ConnectionForm {
            workspace,
            proto,
            secrets_from_a_source,
            unlocking: None,
            pass_input,
            existing_spec: spec.clone(),
            tab: FormTab::Main,
            editing: existing.as_ref().map(|(n, _, _)| n.clone()),
            driver: driver.clone(),
            drivers,
            name: mk(cx, "A name you will recognise", &name_v),
            params: HashMap::new(),
            auth_kind: auth
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("none")
                .to_string(),
            auth_user: mk(cx, "user", &get(&auth, "user")),
            auth_password: Field {
                input: mk(cx, pw_src.hint(), &pw_arg),
                source: pw_src,
                stash: Stash::default(),
                secret: true,
            },
            auth_cert: mk(cx, "/etc/ssl/certs/client.pem", &get(&auth, "cert")),
            auth_key: mk(cx, "/etc/ssl/private/client.key", &get(&auth, "key")),
            auth_key_password: Field {
                input: mk(cx, kp_src.hint(), &kp_arg),
                source: kp_src,
                stash: Stash::default(),
                secret: true,
            },
            auth_provider: {
                let p = get(&auth, "provider");
                if p.is_empty() { "aws".to_string() } else { p }
            },
            auth_token: Field {
                input: mk(cx, tok_src.hint(), &tok_arg),
                source: tok_src,
                stash: Stash::default(),
                secret: true,
            },
            auth_principal: mk(cx, "user@REALM", &get(&auth, "principal")),
            tls_mode: spec
                .get("tls")
                .and_then(|t| t.get("mode"))
                .and_then(Value::as_str)
                .unwrap_or("prefer")
                .to_string(),
            tls_ca: mk(cx, "path to a CA bundle", &get(&tls_v, "ca")),
            tls_cert: mk(cx, "path to a client certificate", &get(&tls_v, "client_cert")),
            tls_key: mk(cx, "path to the certificate's key", &get(&tls_v, "client_key")),
            // A connection saved before policies existed has none recorded, and
            // comes back as "asks first" — the setting that fails toward asking.
            policy: spec
                .get("policy")
                .and_then(Value::as_str)
                .unwrap_or("confirm")
                .to_string(),
            tunnel_on: !tunnel.is_null() || spec.get("tunnel_profile").is_some(),
            tunnel_profile: spec
                .get("tunnel_profile")
                .and_then(Value::as_str)
                .map(str::to_string),
            tunnel_profiles,
            proxy_profiles,
            scripts,
            hooks: Vec::new(),
            hooks_kept: Vec::new(),
            proxy_profile: spec
                .get("proxy_profile")
                .and_then(Value::as_str)
                .map(str::to_string),
            tunnel_profile_specs,
            aws_profiles,
            profile_name: mk(cx, "name this tunnel", ""),
            tunnel_host: mk(cx, "bastion.example", &get(&tunnel, "host")),
            tunnel_port: mk(
                cx,
                "22",
                &tunnel
                    .get("port")
                    .and_then(Value::as_u64)
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
            tunnel_user: mk(cx, "user", &get(&tunnel, "user")),
            tunnel_auth: tunnel
                .get("auth")
                .and_then(|a| a.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("agent")
                .to_string(),
            tunnel_key_path: mk(cx, "~/.ssh/id_ed25519", &key_path),
            tunnel_passphrase: Field {
                input: mk(cx, pass_src.hint(), &pass_arg),
                source: pass_src,
                stash: Stash::default(),
                secret: true,
            },
            tunnel_password: Field {
                input: mk(cx, tpw_src.hint(), &tpw_arg),
                source: tpw_src,
                stash: Stash::default(),
                secret: true,
            },
            tests: HashMap::new(),
            error: None,
            note: None,
            theme,
            scale,
            font_family: text_font,
            icon_family: icon_font,
            lang,
        };
        let stored_params = spec.get("params").cloned().unwrap_or(json!({}));
        form.rebuild_params(&stored_params, cx);
        // The hooks the spec already carries, and whatever this window cannot
        // read kept aside to be written back.
        let (known, kept) = read_hooks(&spec);
        form.hooks_kept = kept;
        for h in known {
            form.push_hook(&h, cx);
        }
        // ENTER is the dialog's default action, from any field — which is what
        // Enter means in a form everywhere else, and the reason somebody fills
        // one in without ever reaching for the pointer.
        //
        // With one exception, and it is not arbitrary: Enter in the PROFILE
        // NAME field saves the profile, because that field belongs to the
        // "save as profile" row and saving the connection from it would be the
        // wrong one of two visible buttons.
        let name_field = form.profile_name.clone();
        let pass_field = form.pass_input.clone();
        for field in form.all_inputs() {
            let is_profile_name = field == name_field;
            // Enter in the PASSPHRASE box opens the wallet. It is the only
            // thing that box is for, and saving the connection from it would
            // be the wrong one of two visible buttons — the same rule the
            // profile-name box already follows.
            let is_pass = field == pass_field;
            cx.subscribe(&field, move |this, _f, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Submit) {
                    if is_pass {
                        this.unlock_then_store(cx);
                    } else if is_profile_name {
                        this.save_tunnel_profile(cx);
                    } else {
                        this.save(cx);
                    }
                }
            })
            .detach();
        }
        form
    }

    /// Build the parameter fields from the driver's own `DriverMeta`. Called
    /// again on a driver change, so the fields are always that driver's — the
    /// bug the previous client had, where a Cassandra port survived into a
    /// MongoDB connection.
    fn rebuild_params(&mut self, stored: &Value, cx: &mut Context<Self>) {
        let colors = colors_of(&self.theme);
        self.params.clear();
        let Some(meta) = self.drivers.iter().find(|d| d.kind == self.driver).cloned() else { return };
        for p in &meta.params {
            let existing = stored.get(&p.key).and_then(Value::as_str).unwrap_or("");
            let (src, arg) = if p.secret {
                Source::unwrap(existing)
            } else {
                (Source::Literal, existing.to_string())
            };
            let value = if arg.is_empty() && existing.is_empty() {
                p.default.clone().unwrap_or_default()
            } else {
                arg
            };
            let ph = p.label.clone();
            let input = cx.new(|cx| {
                let mut i = TextInput::new(cx, ph, colors);
                if !value.is_empty() {
                    i.set_text(value, cx);
                }
                i
            });
            self.params.insert(
                p.key.clone(),
                Field { input, source: src, stash: Stash::default(), secret: p.secret },
            );
        }
    }

    fn meta(&self) -> Option<&DriverMeta> {
        self.drivers.iter().find(|d| d.kind == self.driver)
    }

    /// The `ConnSpec` the engine takes — the same shape `conn.test` and
    /// `conn.connect` both read.
    fn spec(&self, cx: &Context<Self>) -> Value {
        let mut params = serde_json::Map::new();
        for (k, f) in &self.params {
            let raw = f.input.read(cx).text();
            let v = if f.secret { f.source.wrap(raw) } else { raw.trim().to_string() };
            if !v.is_empty() {
                params.insert(k.clone(), json!(v));
            }
        }
        let auth = self.auth_spec(cx);
        // A profile INSTEAD of a spelled-out tunnel. Naming one and spelling one
        // out is refused by the engine rather than resolved by precedence, so
        // the form sends exactly one of them.
        // Exactly one of the two, and `Null` for the other: `merge_spec` reads a
        // null as "take this key off the spec", which is what an unticked tunnel
        // means. Naming a profile AND spelling a tunnel out is refused by the
        // engine rather than resolved by precedence.
        let (tunnel, tunnel_profile) = match self.tunnel_profile.clone().filter(|_| self.tunnel_on) {
            Some(name) => (Value::Null, json!(name)),
            None if self.tunnel_on => (self.tunnel_spec(cx), Value::Null),
            None => (Value::Null, Value::Null),
        };
        // An empty box means the file is NOT part of the spec: `Null` takes the
        // key off (see `merge_spec`), rather than saving an empty path the
        // engine would then try to open.
        let path_or_null = |input: &Entity<TextInput>| {
            let text = input.read(cx).text().trim().to_string();
            if text.is_empty() { Value::Null } else { json!(text) }
        };
        let mut edited = json!({
            "driver": self.driver,
            "params": Value::Object(params),
            "auth": auth,
            "tls": {
                "mode": self.tls_mode,
                "ca": path_or_null(&self.tls_ca),
                "client_cert": path_or_null(&self.tls_cert),
                "client_key": path_or_null(&self.tls_key),
            },
            "tunnel": tunnel,
            "tunnel_profile": tunnel_profile,
            "policy": self.policy
        });
        // The PROXY, and only ever as a profile.
        //
        // Choosing one takes a spelled-out `proxy` off, because the engine
        // refuses a spec carrying both rather than picking between them. Having
        // chosen NONE is different from never having had one: `proxy_profile`
        // goes to null, which takes the key off, while a spelled-out `proxy` is
        // left exactly as it was — this window did not write it and does not
        // show it, so it is not this window's to delete.
        match self.proxy_profile.clone() {
            Some(name) => {
                edited["proxy_profile"] = json!(name);
                edited["proxy"] = Value::Null;
            }
            None => edited["proxy_profile"] = Value::Null,
        }
        // The HOOKS, now that this window edits them. An empty array means
        // there are none, which is different from the key being absent only in
        // that `merge_spec` would keep the old ones — so the array goes out
        // even when it is empty.
        edited["hooks"] = self.hooks_json(cx);
        merge_spec(&self.existing_spec, edited)
    }

    fn run_test(&mut self, stage: &'static str, cx: &mut Context<Self>) {
        let spec = self.spec(cx);
        let this = cx.entity().downgrade();
        let _ = self.workspace.update(cx, |w, cx| {
            w.form_test(stage, spec, this, cx);
        });
    }

    /// Record a stage's answer. Called by the workspace when the engine replies.
    pub fn test_answer(&mut self, stage: &'static str, result: Result<String, String>, cx: &mut Context<Self>) {
        self.tests.insert(stage, result);
        cx.notify();
    }

    /// Every live field in this window.
    pub fn all_inputs(&self) -> Vec<Entity<TextInput>> {
        let mut out = vec![
            self.pass_input.clone(),
            self.name.clone(),
            self.auth_user.clone(),
            self.auth_password.input.clone(),
            self.auth_cert.clone(),
            self.auth_key.clone(),
            self.auth_key_password.input.clone(),
            self.auth_token.input.clone(),
            self.auth_principal.clone(),
            self.tunnel_host.clone(),
            self.tunnel_port.clone(),
            self.tunnel_user.clone(),
            self.tunnel_key_path.clone(),
            self.tunnel_passphrase.input.clone(),
            self.tunnel_password.input.clone(),
            self.profile_name.clone(),
            self.tls_ca.clone(),
            self.tls_cert.clone(),
            self.tls_key.clone(),
        ];
        out.extend(self.params.values().map(|f| f.input.clone()));
        out
    }

    /// The saved tunnels arrived after this window opened.
    ///
    /// They are asked for as the window opens, and the answer comes back after
    /// it is already on screen — so the first open showed an empty list and
    /// "no saved tunnels yet" to somebody who HAS one. A window that is wrong
    /// until it is closed and opened again is a window that teaches people the
    /// feature does not work.
    pub fn profiles_arrived(
        &mut self,
        names: Vec<String>,
        specs: std::collections::BTreeMap<String, Value>,
        cx: &mut Context<Self>,
    ) {
        self.tunnel_profiles = names;
        self.tunnel_profile_specs = specs;
        cx.notify();
    }

    /// The engine refused the save. The window stays, with the reason in it.
    ///
    /// The worst outcome for a failed save is a window that closes anyway:
    /// everything typed is gone and nothing says why. So the refusal comes back
    /// HERE, where the fields still hold what somebody wrote.
    pub fn save_failed(&mut self, why: String, cx: &mut Context<Self>) {
        self.error = Some(why);
        cx.notify();
    }

    /// The chosen auth kind, as the engine's `AuthSpec`.
    ///
    /// Each kind writes ITS OWN keys and nothing else's. The engine's variants
    /// (`src/spec.rs`) are the list, so there is one place that says what each
    /// kind consists of and this follows it rather than guessing.
    fn auth_spec(&self, cx: &Context<Self>) -> Value {
        let val = |f: &Field| f.source.wrap(f.input.read(cx).text());
        auth_json(AuthParts {
            kind: &self.auth_kind,
            user: self.auth_user.read(cx).text(),
            password: &val(&self.auth_password),
            cert: self.auth_cert.read(cx).text(),
            key: self.auth_key.read(cx).text(),
            key_password: &val(&self.auth_key_password),
            provider: &self.auth_provider,
            token: &val(&self.auth_token),
            principal: self.auth_principal.read(cx).text(),
        })
    }

    /// Why a file-backed auth field cannot be right, said before a save.
    ///
    /// The same check the tunnel's key path gets, for the same reason: a
    /// certificate path that is not a path fails at dial time with the value
    /// quoted back at it.
    fn auth_file_problem(&self, cx: &Context<Self>) -> Option<String> {
        if self.auth_kind != "client_cert" {
            return None;
        }
        for (what, field) in [("certificate", &self.auth_cert), ("key", &self.auth_key)] {
            let raw = field.read(cx).text();
            let path = raw.trim();
            if path.is_empty() {
                return Some(format!("a {what} file is needed for a client certificate"));
            }
            if let Some(why) = path_problem(path) {
                return Some(format!("{what}: {why}"));
            }
        }
        None
    }

    /// The tunnel, as the engine's `TunnelSpec`.
    ///
    /// ONE definition, used by the connection's own spec and by a saved profile
    /// alike. Two builders of the same shape is how the last bug got in: a
    /// reader and a writer that disagreed about which key held what.
    fn tunnel_spec(&self, cx: &Context<Self>) -> Value {
        let tauth = tunnel_auth_json(
            &self.tunnel_auth,
            self.tunnel_key_path.read(cx).text(),
            &self
                .tunnel_passphrase
                .source
                .wrap(self.tunnel_passphrase.input.read(cx).text()),
            &self
                .tunnel_password
                .source
                .wrap(self.tunnel_password.input.read(cx).text()),
        );
        json!({
            "host": self.tunnel_host.read(cx).text(),
            // See `tunnel_port_problem`: a save is refused before this runs,
            // so the fallback is only so `spec` stays infallible.
            "port": crate::workspace::parse_port(&self.tunnel_port.read(cx).text(), 22)
                .unwrap_or(22),
            "user": self.tunnel_user.read(cx).text(),
            "auth": tauth
        })
    }

    /// What is wrong with the typed tunnel port, if anything. An empty field is
    /// the default; anything that is not a port is a refusal, not a silent 22.
    fn tunnel_port_problem(&self, cx: &Context<Self>) -> Option<String> {
        if !self.tunnel_on {
            return None;
        }
        crate::workspace::parse_port(&self.tunnel_port.read(cx).text(), 22)
            .err()
            .map(|e| format!("SSH tunnel: {e}"))
    }

    /// Store the tunnel fields as a named profile, for every other connection.
    fn save_tunnel_profile(&mut self, cx: &mut Context<Self>) {
        let name = self.profile_name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.error = Some("a profile needs a name".into());
            cx.notify();
            return;
        }
        if let Some(why) =
            crate::workspace::parse_port(&self.tunnel_port.read(cx).text(), 22).err()
        {
            self.error = Some(why);
            cx.notify();
            return;
        }
        // The same refusal a save gets, for the same reason: a profile with a
        // passphrase in its key path is a profile that breaks every connection
        // that adopts it.
        if let Some(why) = self.key_path_problem(cx) {
            self.error = Some(format!("SSH key: {why}"));
            cx.notify();
            return;
        }
        let spec = self.tunnel_spec(cx);
        let _ = self.workspace.update(cx, |w, _| {
            w.save_profile("tunnel", name.clone(), spec);
        });
        // It becomes selectable here without waiting for the panel to catch up.
        if !self.tunnel_profiles.contains(&name) {
            self.tunnel_profiles.push(name.clone());
            self.tunnel_profiles.sort();
        }
        self.error = None;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.error = Some("a connection needs a name".into());
            cx.notify();
            return;
        }
        // A SECRET TYPED AS ITSELF, where that has been turned off.
        //
        // Refused at the save, because the save is what would put it in the
        // store — and the store is a plain SQLite file that the other client
        // reads too. The message names the field and the way out rather than
        // just saying no.
        if self.secrets_from_a_source {
            let literal: Vec<&str> = [
                ("password", &self.auth_password),
                ("key passphrase", &self.auth_key_password),
                ("token", &self.auth_token),
                ("tunnel passphrase", &self.tunnel_passphrase),
                ("tunnel password", &self.tunnel_password),
            ]
            .iter()
            .filter(|(_, f)| {
                f.source == Source::Literal && !f.input.read(cx).text().trim().is_empty()
            })
            .map(|(n, _)| *n)
            .collect();
            if !literal.is_empty() {
                self.error = Some(format!(
                    "{} would be stored as typed, and this window is set not to do that. \
                     Press the key beside the field to put it in the wallet, or point it at \
                     the environment or a command.",
                    literal.join(", ")
                ));
                self.tab = FormTab::Main;
                cx.notify();
                return;
            }
        }
        // Refused here rather than at dial time. The connection that broke was
        // broken by a save, and a save is the last moment anybody is looking at
        // the field that is wrong.
        if let Some(why) = self.tunnel_port_problem(cx) {
            self.error = Some(why);
            self.tab = FormTab::Access;
            cx.notify();
            return;
        }
        if let Some(why) = self.key_path_problem(cx) {
            self.error = Some(format!("SSH key: {why}"));
            self.tab = FormTab::Access;
            cx.notify();
            return;
        }
        // The same refusal for a client certificate, which is a pair of files
        // and until now could not be given either.
        if let Some(why) = self.auth_file_problem(cx) {
            self.error = Some(why);
            self.tab = FormTab::Main;
            cx.notify();
            return;
        }
        let mut spec = self.spec(cx);
        if let Some(m) = spec.as_object_mut() {
            m.remove("driver");
        }
        let (driver, old) = (self.driver.clone(), self.editing.clone());
        let _ = self.workspace.update(cx, |w, cx| {
            w.form_save(name, driver, spec, old, cx);
        });
    }
}

/// The typed-in values an auth kind is built from.
pub struct AuthParts<'a> {
    pub kind: &'a str,
    pub user: &'a str,
    pub password: &'a str,
    pub cert: &'a str,
    pub key: &'a str,
    pub key_password: &'a str,
    pub provider: &'a str,
    pub token: &'a str,
    pub principal: &'a str,
}

/// One auth kind, as the engine's `AuthSpec` wants it.
///
/// Free of the widgets on purpose, so the thing under test is the thing that
/// runs. A test asserting against hand-written JSON beside this would agree
/// with itself forever and catch nothing — which is how three of these bugs
/// survived.
///
/// Each arm writes ONLY the keys its own variant in `src/spec.rs` declares.
pub fn auth_json(p: AuthParts<'_>) -> Value {
    match p.kind {
        "password" => json!({
            "kind": "password",
            "user": p.user,
            "password": p.password,
        }),
        "client_cert" => json!({
            "kind": "client_cert",
            "cert": p.cert.trim(),
            "key": p.key.trim(),
            "key_password": p.key_password,
            "user": p.user,
        }),
        "provider" => json!({
            "kind": "provider",
            "provider": p.provider,
            "token": p.token,
            "user": p.user,
        }),
        "kerberos" => json!({
            "kind": "kerberos",
            // Absent rather than empty: the engine's field is an `Option`, and
            // "" is a principal nobody has.
            "principal": if p.principal.trim().is_empty() {
                Value::Null
            } else {
                json!(p.principal.trim())
            },
        }),
        _ => json!({ "kind": "none" }),
    }
}

/// Open the system file chooser and put what comes back in `input`.
///
/// The SYSTEM one, through GPUI, which on Linux goes to `xdg-desktop-portal` —
/// so it is the dialog the rest of this desktop uses, with its own bookmarks and
/// places, and it works under Wayland like everything else here. Checked before
/// relying on it: the portal and its GTK backend are both running on this
/// machine.
///
/// The field stays a TEXT field and this is an addition to it. A path can come
/// from a variable, from a script, or from a machine where no portal answers,
/// and a typed path must never be the harder road.
///
/// When the portal cannot be reached the button SAYS so rather than doing
/// nothing — a control that silently fails is one somebody presses twice and
/// then stops trusting.
/// The chooser for a file to WRITE: a name that need not exist yet.
///
/// A separate call from `pick_file` because the platform prompt is a different
/// one — "choose an existing file" and "name a new file" are different
/// questions and the portal asks them differently.
pub fn pick_new_path<F>(input: Entity<TextInput>, on_fail: F, cx: &mut gpui::App)
where
    F: FnOnce(String, &mut gpui::AsyncApp) + 'static,
{
    let rx = cx.prompt_for_new_path(std::path::Path::new(""), None);
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        match rx.await {
            Ok(Ok(Some(path))) => {
                let text = path.to_string_lossy().to_string();
                let _ = input.update(cx, |i, cx| i.set_text(text, cx));
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => on_fail(format!("the file chooser could not open: {e}"), cx),
            Err(_) => on_fail(
                "the file chooser did not answer — this desktop may have no \
                 xdg-desktop-portal running"
                    .to_string(),
                cx,
            ),
        }
    })
    .detach();
}

pub fn pick_file<F>(input: Entity<TextInput>, on_fail: F, cx: &mut gpui::App)
where
    F: FnOnce(String, &mut gpui::AsyncApp) + 'static,
{
    let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Choose".into()),
    });
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        match rx.await {
            // A path was chosen.
            Ok(Ok(Some(paths))) => {
                if let Some(p) = paths.into_iter().next() {
                    let text = p.to_string_lossy().to_string();
                    let _ = input.update(cx, |i, cx| i.set_text(text, cx));
                }
            }
            // Cancelled: nothing to say and nothing to change.
            Ok(Ok(None)) => {}
            Ok(Err(e)) => on_fail(format!("the file chooser could not open: {e}"), cx),
            Err(_) => on_fail(
                "the file chooser did not answer — this desktop may have no \
                 xdg-desktop-portal running"
                    .to_string(),
                cx,
            ),
        }
    })
    .detach();
}

/// A tunnel's auth, as the engine's `TunnelAuth`.
///
/// ONE builder, used by the connection window and by the profile window alike.
/// Two builders of this shape is exactly how a passphrase ended up written into
/// a key path, and the second window is precisely the moment that would happen
/// again.
///
/// The passphrase is CARRIED, not blanked. It used to be written out as a
/// literal empty string, which quietly made every passphrase-protected key
/// unusable.
pub fn tunnel_auth_json(kind: &str, path: &str, passphrase: &str, password: &str) -> Value {
    match kind {
        "key" => json!({ "kind": "key", "path": path, "passphrase": passphrase }),
        "password" => json!({ "kind": "password", "password": password }),
        _ => json!({ "kind": "agent" }),
    }
}

/// The provider names the engine's own `AuthSpec` documents.
///
/// Not built from `access.list`: that returns the AWS PROFILE names out of
/// `~/.aws/config`, which is a different thing entirely, and `AuthSpec` has no
/// field to put one in. Offering them here would be putting a value of one kind
/// into a box meaning another — which is the exact fault this change exists to
/// undo.
const PROVIDERS: [&str; 3] = ["aws", "oauth", "oidc"];

/// Why this text is not a usable path to an existing file.
///
/// ONE definition, used by the tunnel's key, the client certificate and its
/// key. The check exists because of a value typed into the wrong box: a
/// passphrase has punctuation and spaces and no separator, and looks nothing
/// like a path — so the form can say so before anything is dialled.
pub fn path_problem(path: &str) -> Option<String> {
    if !path.starts_with('/') && !path.starts_with('~') && !path.starts_with('.') {
        return Some("that is not a path — it starts with / or ~".into());
    }
    if path.chars().any(char::is_whitespace) {
        return Some("a path has no spaces in it".into());
    }
    let expanded = match path.strip_prefix('~') {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}{rest}"),
            Err(_) => path.to_string(),
        },
        None => path.to_string(),
    };
    if !std::path::Path::new(&expanded).is_file() {
        return Some(format!("there is no file at {path}"));
    }
    None
}

pub fn colors_of(t: &Theme) -> InputColors {
    InputColors {
        text: t.fg(),
        placeholder: t.muted(),
        cursor: t.accent(),
        selection: t.selection_bg(),
        background: t.base(),
        border: t.border(),
    }
}

impl ConnectionForm {
    /// Move the keyboard to the next field, or the previous one.
    ///
    /// The ORDER is `all_inputs()`, which is also the order a theme change
    /// walks — one list, so tab cannot visit a field the palette forgets.
    fn step_focus(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.all_inputs();
        let now = fields
            .iter()
            .position(|i| i.read(cx).focus_handle.is_focused(window));
        if let Some(next) = crate::widgets::focus_step(fields.len(), now, back) {
            window.focus(&fields[next].read(cx).focus_handle);
            cx.notify();
        }
    }

    /// One phrase of the interface, in the chosen language.
    fn tr(&self, key: &str) -> SharedString {
        SharedString::from(self.lang.tr(key))
    }

    /// The language changed behind this window.
    pub fn set_lang(&mut self, lang: Lang, cx: &mut Context<Self>) {
        self.lang = lang;
        cx.notify();
    }

    /// Kept as a method for the forty call sites below; the DRAWING is the
    /// shared one, so a change to it reaches every window at once.
    fn small(&self, text: impl Into<SharedString>, c: gpui::Rgba) -> impl IntoElement {
        crate::widgets::small(self.scale, text, c)
    }

    /// A labelled row: label, source chooser where one applies, field.
    /// A labelled field with its source picker.
    ///
    /// Takes the PARTS rather than a `Field`, so no call site has to build a
    /// throwaway `Field` to be able to draw one — those copies are how a field's
    /// stash would silently be left behind.
    fn row(
        &self,
        label: &str,
        input: &Entity<TextInput>,
        source: Source,
        secret: bool,
        key: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = &self.theme;
        let ms = self.scale;
        // The label and, under a template source, what the field is now asking
        // for. ON ITS OWN LINE rather than appended to the label: `Password ·
        // Environment variable` in a column `label_w` wide was cut to
        // `Password · Env…` on the English labels and lost the whole second
        // half on the Bulgarian ones, which is the half that says what to type.
        let aside = (secret && source != Source::Literal).then(|| source.field_label());
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .py(px(2.0))
            .child(
                div()
                    .id(SharedString::from(format!("lbl-{key}")))
                    .w(px(ms.label_w()))
                    .flex_none()
                    .flex()
                    .flex_col()
                    // The whole label on hover, since a translated one is
                    // longer than the English it replaced and this column does
                    // not grow.
                    .tooltip(crate::tip::Tip::build(
                        match aside {
                            Some(a) => format!("{label} · {a}"),
                            None => label.to_string(),
                        },
                        None,
                        self.theme.clone(),
                        ms,
                    ))
                    .child(
                        div()
                            .w(px(ms.label_w()))
                            .truncate()
                            .text_size(ms.small())
                            .text_color(t.muted())
                            .child(SharedString::from(label.to_string())),
                    )
                    .children(aside.map(|a| {
                        div()
                            .w(px(ms.label_w()))
                            .truncate()
                            .text_size(ms.tiny())
                            .text_color(t.accent())
                            .child(SharedString::from(a))
                    })),
            );

        if secret {
            let mut picker = div().flex().gap_1();
            for s in Source::all() {
                let on = source == s;
                let k = key.clone();
                picker = picker.child(
                    chip(&t, ms, format!("src-{k}-{}", s.label()), s.label(), on)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_source(&k, s, cx);
                            cx.notify();
                        })),
                );
            }
            row = row.child(picker);
        }
        // INTO THE WALLET. Only for a literal — the other three sources are
        // already a pointer at a secret kept somewhere else, and there would be
        // nothing to move.
        //
        // Named after the connection, as lvim-db names them, so the Neovim
        // client looks the secret up in the same place. The password gets the
        // bare `db/<name>`, which is that convention; the other secrets a
        // connection can have hang below it, because one name for four secrets
        // would be one secret overwritten three times.
        if secret && source == Source::Literal && self.proto >= 17 {
            let typed = input.read(cx).text().to_string();
            let conn = self.name.read(cx).text().trim().to_string();
            let can = !typed.is_empty() && !conn.is_empty();
            let (k, field) = (key.clone(), key.clone());
            row = row.child(
                div()
                    .id(SharedString::from(format!("vault-{key}")))
                    .flex_none()
                    .cursor_pointer()
                    .tooltip(crate::tip::Tip::build(
                        if can {
                            format!(
                                "{} \u{2192} {}",
                                self.tr("tip.to_keyring"),
                                vault_name(&conn, &k)
                            )
                        } else if conn.is_empty() {
                            self.tr("tip.to_keyring_needs_name").to_string()
                        } else {
                            self.tr("tip.to_keyring_needs_value").to_string()
                        },
                        None,
                        self.theme.clone(),
                        ms,
                    ))
                    .hover(|d| d.bg(t.hover_mark()))
                    .child(self.icon_inline(
                        "\u{f084}",
                        if can { t.accent() } else { t.disabled() },
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if can {
                            this.to_keyring(&field, cx);
                        }
                    })),
            );
        }
        // Dots, and an eye to look.
        //
        // Only when the value IS the credential. Under `Env`, `Command` and
        // `Keyring` the field holds the NAME of a variable, a command line or a
        // keyring path — public text that somebody has to be able to read and
        // check, and masking it hides nothing while making it unusable.
        let hide = secret && source == Source::Literal;
        let shown = input.read(cx).revealed();
        input.update(cx, |i, cx| i.set_mask(hide, cx));
        row = row.child(div().flex_1().text_size(ms.body()).child(input.clone()));
        if hide {
            let input = input.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("eye-{key}")))
                    .flex_none()
                    .cursor_pointer()
                    .tooltip(crate::tip::Tip::build(
                        if shown { self.tr("tip.hide") } else { self.tr("tip.reveal") },
                        None,
                        self.theme.clone(),
                        ms,
                    ))
                    .hover(|d| d.bg(t.hover_mark()))
                    // Open eye while it is showing, struck-through eye while it
                    // is not: the icon says what you are looking at, not what
                    // pressing it would do.
                    .child(self.icon_inline(
                        if shown { "\u{f06e}" } else { "\u{f070}" },
                        if shown { t.warning() } else { t.muted() },
                    ))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        input.update(cx, |i, cx| i.toggle_reveal(cx));
                        cx.notify();
                    })),
            );
        }
        row
    }

    /// Move a field to a different source, keeping what was typed in the old one.
    ///
    /// The swap is the point: each mode holds a DIFFERENT KIND of value, so
    /// carrying one across would show a variable name where a password was and
    /// then save it as the password. The hint changes too, because an empty box
    /// that wants `PGPASSWORD` and an empty box that wants a password look the
    /// same.
    pub(crate) fn swap_source(f: &mut Field, to: Source, cx: &mut gpui::App) {
        if f.source == to {
            return;
        }
        let current = f.input.read(cx).text().to_string();
        f.stash.put(f.source, current);
        let next = f.stash.take(to);
        f.source = to;
        f.input.update(cx, |i, cx| {
            i.set_text(next, cx);
            i.set_placeholder(to.hint(), cx);
            // Only a literal is the secret itself.
            i.set_mask(f.secret && to == Source::Literal, cx);
        });
    }

    /// One hook, drawn.
    ///
    /// The MOMENT first, because it is the thing that changes what the hook
    /// means: a failure before the dial stops the connection and a failure
    /// anywhere else does not, and that is said under the row rather than left
    /// to be discovered.
    fn hook_el(&self, i: usize, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme.clone();
        let ms = self.scale;
        let h = &self.hooks[i];
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .py_1()
            .px_2()
            .mb_1()
            .border_1()
            .border_color(t.border());

        let mut when = div().flex().items_center().gap_1().child(
            div().w(px(ms.label_w())).child(self.small(self.tr("hook.when"), t.muted())),
        );
        for ev in HOOK_EVENTS {
            let on = h.on == *ev;
            when = when.child(
                chip(&t, ms, format!("hk-{i}-{ev}"), ev.replace('_', " "), on).on_click(
                    cx.listener(move |this, _, _, cx| {
                        if let Some(h) = this.hooks.get_mut(i) {
                            h.on = ev.to_string();
                        }
                        cx.notify();
                    }),
                ),
            );
        }
        card = card.child(when);

        let mut what = div().flex().items_center().gap_1().child(
            div().w(px(ms.label_w())).child(self.small(self.tr("hook.what"), t.muted())),
        );
        for kind in ["command", "script", "inline"] {
            let on = h.kind == kind;
            what = what.child(
                chip(&t, ms, format!("hkk-{i}-{kind}"), kind, on).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.set_hook_kind(i, kind, cx);
                        cx.notify();
                    },
                )),
            );
        }
        // The LANGUAGE, only for an inline body: a command line goes through a
        // shell and a saved script carries its own language.
        if h.kind == "inline" {
            for l in ["bash", "python", "lua"] {
                let on = h.lang == l;
                what = what.child(
                    chip(&t, ms, format!("hkl-{i}-{l}"), l, on).on_click(cx.listener(
                        move |this, _, _, cx| {
                            if let Some(h) = this.hooks.get_mut(i) {
                                h.lang = l.to_string();
                            }
                            cx.notify();
                        },
                    )),
                );
            }
        }
        what = what.child(div().flex_1()).child(
            btn(&t, ms, format!("hkrm-{i}"), self.tr("hook.remove"), Btn::Quiet).on_click(
                cx.listener(move |this, _, _, cx| {
                    if i < this.hooks.len() {
                        this.hooks.remove(i);
                    }
                    cx.notify();
                }),
            ),
        );
        card = card.child(what);

        if h.kind == "inline" {
            card = card.child(div().text_size(ms.body()).child(h.body.clone()));
        } else if h.kind == "script" {
            card = card.child(div().text_size(ms.body()).child(h.source.clone()));
            // WHICH scripts there are. A box wanting a name with no way to
            // learn the names is a box nobody can fill correctly.
            if self.scripts.is_empty() {
                card = card.child(self.small(self.tr("hook.no_scripts"), t.muted()));
            } else {
                let mut pick = div().flex().flex_wrap().gap_1();
                for name in self.scripts.clone() {
                    let on = h.source.read(cx).text().trim() == name;
                    let (n, target) = (name.clone(), h.source.clone());
                    pick = pick.child(
                        chip(&t, ms, format!("hks-{i}-{name}"), name.clone(), on).on_click(
                            cx.listener(move |_this, _, _, cx| {
                                let n = n.clone();
                                target.update(cx, |x, cx| x.set_text(n, cx));
                                cx.notify();
                            }),
                        ),
                    );
                }
                card = card.child(pick);
            }
        } else {
            card = card.child(div().text_size(ms.body()).child(h.source.clone()));
        }

        card = card.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(px(ms.label_w()))
                        .child(self.small(self.tr("hook.timeout"), t.muted())),
                )
                .child(div().w(px(ms.base() * 6.0)).text_size(ms.body()).child(h.timeout.clone())),
        );
        card = card.child(self.small(
            if hook_blocks(&h.on) {
                self.tr("hook.blocking")
            } else {
                self.tr("hook.not_blocking")
            },
            if hook_blocks(&h.on) { t.warning() } else { t.muted() },
        ));
        card
    }

    /// One hook row, from what the spec says.
    fn push_hook(&mut self, h: &Value, cx: &mut Context<Self>) {
        let colors = colors_of(&self.theme);
        let get = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let kind = {
            let k = get("kind");
            if k.is_empty() { "command".to_string() } else { k }
        };
        let source = get("source");
        let mk = |ph: &str, val: &str, cx: &mut Context<Self>| {
            let (ph, val) = (ph.to_string(), val.to_string());
            cx.new(|cx| {
                let mut i = TextInput::new(cx, ph, colors);
                if !val.is_empty() {
                    i.set_text(val, cx);
                }
                i
            })
        };
        let lang = {
            let l = get("lang");
            // BASH by default, because a shell line is what a hook usually is
            // and because it is the one interpreter that is always there.
            if l.is_empty() { "bash".to_string() } else { l }
        };
        let (theme, scale, font) = (self.theme.clone(), self.scale, self.font_family.clone());
        let body = cx.new(|cx| {
            crate::editor_el::Editor::new(
                if kind == "inline" { &source } else { "" },
                crate::editor_el::Flavour::Plain,
                colors,
                theme,
                scale,
                font,
                cx,
            )
        });
        let row = HookRow {
            on: {
                let o = get("on");
                if HOOK_EVENTS.contains(&o.as_str()) { o } else { "pre_connect".into() }
            },
            source: mk(
                self.lang.tr(match kind.as_str() {
                    "script" => "hook.script_hint",
                    _ => "hook.cmd_hint",
                }),
                if kind == "inline" { "" } else { &source },
                cx,
            ),
            body,
            lang,
            timeout: mk(
                "",
                &h.get("timeout_ms").and_then(Value::as_u64).map(|t| t.to_string()).unwrap_or_default(),
                cx,
            ),
            kind,
            stash: std::collections::HashMap::new(),
        };
        self.hooks.push(row);
    }

    /// A new, empty hook. Before the dial, a shell line: the common case, and
    /// the only moment where failing means anything.
    fn add_hook(&mut self, cx: &mut Context<Self>) {
        self.push_hook(&json!({ "on": "pre_connect", "kind": "command", "source": "" }), cx);
    }

    /// Switch a hook's kind, keeping what was typed under the old one.
    ///
    /// The same cure as the secret sources: a command line carried into a
    /// script name would be saved as the name of a script that does not exist.
    fn set_hook_kind(&mut self, i: usize, kind: &str, cx: &mut Context<Self>) {
        let Some(h) = self.hooks.get_mut(i) else { return };
        if h.kind == kind {
            return;
        }
        if h.kind == "inline" {
            h.stash.insert("inline".into(), h.body.read(cx).text());
        } else {
            h.stash.insert(h.kind.clone(), h.source.read(cx).text().to_string());
        }
        h.kind = kind.to_string();
        let back = h.stash.get(kind).cloned().unwrap_or_default();
        if kind == "inline" {
            h.body.update(cx, |e, cx| e.set_text(&back, cx));
        } else {
            let hint = self.lang.tr(if kind == "script" {
                "hook.script_hint"
            } else {
                "hook.cmd_hint"
            });
            let f = h.source.clone();
            f.update(cx, |i, cx| {
                i.set_text(back, cx);
                i.set_placeholder(hint, cx);
            });
        }
    }

    /// The `hooks` array this window describes, plus whatever it could not read.
    fn hooks_json(&self, cx: &Context<Self>) -> Value {
        let mut out: Vec<Value> = Vec::new();
        for h in &self.hooks {
            let source = if h.kind == "inline" {
                h.body.read(cx).text()
            } else {
                h.source.read(cx).text().trim().to_string()
            };
            // An EMPTY hook is not saved. A hook with nothing to run is a row
            // somebody started and left, and writing it out would mean the
            // engine running an empty shell line on every connect.
            if source.trim().is_empty() {
                continue;
            }
            let mut o = json!({ "on": h.on, "kind": h.kind, "source": source });
            if h.kind == "inline" {
                o["lang"] = json!(h.lang);
            }
            // A blank timeout is the ENGINE's default, not forever, so the key
            // is left off rather than written as null.
            if let Ok(ms) = h.timeout.read(cx).text().trim().parse::<u64>() {
                if ms > 0 {
                    o["timeout_ms"] = json!(ms);
                }
            }
            out.push(o);
        }
        out.extend(self.hooks_kept.iter().cloned());
        Value::Array(out)
    }

    /// Close this window, from INSIDE it.
    ///
    /// Its own `&mut Window`, not a round trip through the workspace: asking
    /// the workspace to close it means `WindowHandle::update` on the window
    /// that is already being updated, which gpui refuses — and the `let _ =`
    /// on the result meant Cancel and Escape did nothing and said nothing.
    /// The same fault the profile window had.
    fn leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |w, cx| w.forget_form(cx));
        window.remove_window();
    }

    /// Hand the typed secret to the engine, to be put in the wallet.
    ///
    /// The field is NOT switched here. It is switched when the engine says the
    /// secret landed — see `stored_in_keyring`. Switching first would mean a
    /// form pointing at a wallet entry that does not exist, with the password
    /// that was typed already gone.
    fn to_keyring(&mut self, key: &str, cx: &mut Context<Self>) {
        let conn = self.name.read(cx).text().trim().to_string();
        let Some(f) = self.field(key) else { return };
        let value = f.input.read(cx).text().to_string();
        if conn.is_empty() || value.is_empty() {
            return;
        }
        let name = vault_name(&conn, key);
        let (k, me) = (key.to_string(), cx.entity().downgrade());
        self.error = None;
        self.note = Some(format!("storing {name}\u{2026}"));
        let _ = self.workspace.update(cx, |w, _| w.vault_put(k, name, value, me));
        cx.notify();
    }

    /// The engine stored it. NOW the field becomes a pointer at the wallet.
    pub fn stored_in_keyring(&mut self, key: &str, name: &str, cx: &mut Context<Self>) {
        if self.field(key).is_none() {
            return;
        }
        let f = self.field_mut(key).expect("just checked");
        Self::swap_source(f, Source::Keyring, cx);
        if let Some(f) = self.field_mut(key) {
            f.input.update(cx, |i, cx| i.set_text(name.to_string(), cx));
        }
        self.error = None;
        self.note = Some(format!("{name} is in the keyring"));
        cx.notify();
    }

    /// The wallet refused it — locked, or not running. The field keeps the
    /// value, because it is the only copy.
    ///
    /// A LOCKED wallet is not a failure to report and walk away from: it is
    /// one passphrase away from working, and sending somebody to Settings to
    /// type it — then back here to press the key again — is three windows for
    /// one thought. The row appears here instead.
    pub fn keyring_refused(&mut self, key: &str, why: &str, cx: &mut Context<Self>) {
        self.note = None;
        if why.contains("locked") {
            self.unlocking = Some(key.to_string());
            self.error = None;
            self.pass_input.update(cx, |i, cx| i.set_text(String::new(), cx));
        } else {
            self.unlocking = None;
            self.error = Some(why.to_string());
        }
        cx.notify();
    }

    /// The passphrase was typed here. Open the wallet, and if it opens, put
    /// the secret in — which is what was being asked for when it was locked.
    fn unlock_then_store(&mut self, cx: &mut Context<Self>) {
        let pass = self.pass_input.read(cx).text().to_string();
        if pass.is_empty() {
            self.error = Some(self.lang.tr("wallet.need_pass").to_string());
            cx.notify();
            return;
        }
        let field = self.unlocking.clone();
        let me = cx.entity().downgrade();
        self.note = Some(self.lang.tr("wallet.unlocking").to_string());
        let _ = self.workspace.update(cx, |w, _| w.unlock_for_form(pass, field, me));
        self.pass_input.update(cx, |i, cx| i.set_text(String::new(), cx));
        cx.notify();
    }

    /// The wallet opened. The key that was pressed is pressed again, by the
    /// window rather than by the person — they already said what they wanted.
    ///
    /// AFTER this update, not inside it. The workspace is what calls this, from
    /// inside its own update, and `to_keyring` asks the workspace to store the
    /// secret — which gpui refuses with "cannot update Workspace while it is
    /// already being updated", and refuses by PANICKING, so the window died on
    /// the one keystroke that was supposed to make it work. Deferred, the press
    /// happens once the workspace has been handed back to the app.
    pub fn wallet_opened(&mut self, field: Option<String>, cx: &mut Context<Self>) {
        self.unlocking = None;
        self.note = Some(self.lang.tr("wallet.open").to_string());
        if let Some(k) = field {
            let me = cx.entity().downgrade();
            cx.defer(move |cx| {
                let _ = me.update(cx, |f, cx| f.to_keyring(&k, cx));
            });
        }
        cx.notify();
    }

    /// It did not open. The passphrase is gone and the row stays, because the
    /// next thing somebody does is type it again.
    pub fn wallet_refused(&mut self, why: &str, cx: &mut Context<Self>) {
        self.note = None;
        self.error = Some(why.to_string());
        cx.notify();
    }

    /// One field, by the key its row was drawn with.
    fn field(&self, key: &str) -> Option<&Field> {
        match key {
            "auth-password" => Some(&self.auth_password),
            "auth-key-password" => Some(&self.auth_key_password),
            "auth-token" => Some(&self.auth_token),
            "tunnel-passphrase" => Some(&self.tunnel_passphrase),
            "tunnel-password" => Some(&self.tunnel_password),
            other => self.params.get(other),
        }
    }

    fn field_mut(&mut self, key: &str) -> Option<&mut Field> {
        match key {
            "auth-password" => Some(&mut self.auth_password),
            "auth-key-password" => Some(&mut self.auth_key_password),
            "auth-token" => Some(&mut self.auth_token),
            "tunnel-passphrase" => Some(&mut self.tunnel_passphrase),
            "tunnel-password" => Some(&mut self.tunnel_password),
            other => self.params.get_mut(other),
        }
    }

    fn set_source(&mut self, key: &str, s: Source, cx: &mut gpui::App) {
        let f = match key {
            "auth-password" => &mut self.auth_password,
            "auth-key-password" => &mut self.auth_key_password,
            "auth-token" => &mut self.auth_token,
            "tunnel-passphrase" => &mut self.tunnel_passphrase,
            "tunnel-password" => &mut self.tunnel_password,
            other => match self.params.get_mut(other) {
                Some(f) => f,
                None => return,
            },
        };
        Self::swap_source(f, s, cx);
    }

    /// Why this key path cannot be right, said before anything is saved.
    ///
    /// A refusal with a reason beats a connection that fails at dial time with
    /// the value quoted back at it. And it is the shape of the mistake that was
    /// actually made: a passphrase typed where a path goes looks nothing like a
    /// path, and a path that is not there is not a key.
    fn key_path_problem(&self, cx: &Context<Self>) -> Option<String> {
        if self.tunnel_auth != "key" || !self.tunnel_on {
            return None;
        }
        let raw = self.tunnel_key_path.read(cx).text();
        let path = raw.trim();
        if path.is_empty() {
            return Some("a key file is needed, or choose another method".into());
        }
        path_problem(path)
    }

    /// A path field: the box, and a button that opens the system chooser.
    ///
    /// The chosen path goes into the SAME box a typed one would, so it meets
    /// exactly the same check (`path_problem`) and there are not two answers
    /// about one file.
    fn path_row(
        &self,
        // Not `&'static str` any more: the label comes from the catalogue, and
        // a translated phrase is owned, not a literal in the binary.
        label: impl Into<SharedString>,
        input: &Entity<TextInput>,
        id: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (t, ms) = (&self.theme, self.scale);
        let target = input.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .py(px(2.0))
            .child(div().w(px(ms.label_w())).flex_none().child(self.small(label, t.muted())))
            .child(div().flex_1().text_size(ms.body()).child(input.clone()))
            .child(
                btn(t, ms, id, self.tr("form.browse"), Btn::Secondary)
                    .on_click(cx.listener(move |_this, _, _, cx| {
                        let me = cx.entity().downgrade();
                        pick_file(
                            target.clone(),
                            move |why, cx| {
                                // The reason lands in the window, beside the
                                // button. A control that fails in silence is one
                                // somebody presses twice and then stops trusting.
                                let _ = me.update(cx, |f: &mut ConnectionForm, cx| {
                                    f.error = Some(why);
                                    cx.notify();
                                });
                            },
                            cx,
                        );
                        cx.notify();
                    })),
            )
    }

    /// `path_row` for a label and id only known at runtime.
    fn path_row_dyn(
        &self,
        label: &str,
        input: &Entity<TextInput>,
        key: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (t, ms) = (&self.theme, self.scale);
        let target = input.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .py(px(2.0))
            .child(
                div()
                    .w(px(ms.label_w()))
                    .flex_none()
                    .child(self.small(label.to_string(), t.muted())),
            )
            .child(div().flex_1().text_size(ms.body()).child(input.clone()))
            .child(
                btn(t, ms, format!("pick-{key}"), self.tr("form.browse"), Btn::Secondary)
                    .on_click(cx.listener(move |_this, _, _, cx| {
                        let me = cx.entity().downgrade();
                        pick_file(
                            target.clone(),
                            move |why, cx| {
                                let _ = me.update(cx, |f: &mut ConnectionForm, cx| {
                                    f.error = Some(why);
                                    cx.notify();
                                });
                            },
                            cx,
                        );
                        cx.notify();
                    })),
            )
    }

    /// A small icon in a fixed square, the same one the main window uses.
    ///
    /// In the ICON font. The eye (`f06e`/`f070`) is a Nerd Font codepoint, and
    /// drawn in the text font it is a box — which is what it was, because this
    /// inherited the root's family.
    fn icon_inline(&self, glyph: &'static str, colour: gpui::Rgba) -> gpui::Div {
        let ms = self.scale;
        // The size of the text beside it, optically — see `icon_beside`.
        let size = ms.icon_beside(ms.small());
        let box_w = (f32::from(size) * 1.25).round();
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .w(px(box_w))
            .h(px(box_w))
            .line_height(px(box_w))
            .font_family(SharedString::from(self.icon_family.clone()))
            .text_size(size)
            .text_color(colour)
            .child(glyph)
    }

    /// A drawn checkbox: a box, with a tick in it when it is on.
    fn tick(&self, on: bool) -> gpui::Div {
        crate::widgets::tick(&self.theme, self.scale, on)
    }

    /// What stops this test from being worth running, and WHERE to fix it.
    ///
    /// Pressing "Test tls" and being told `missing required parameter
    /// 'database'` is an answer about a field on another tab, and it does not
    /// say that it is on another tab, or which field it means. The form already
    /// knows the engine's required parameters, so it can say so before sending
    /// anything — and the button that cannot succeed is not offered as though
    /// it could.
    fn blocking(&self, cx: &Context<Self>) -> Option<String> {
        if self.name.read(cx).text().trim().is_empty() {
            return Some("needs a name — the field at the top of this window".into());
        }
        let meta = self.meta()?;
        for p in meta.params.iter().filter(|p| p.required) {
            let empty = self
                .params
                .get(&p.key)
                .map_or(true, |f| f.input.read(cx).text().trim().is_empty());
            if empty {
                return Some(format!("needs {} — the Main tab", p.label));
            }
        }
        None
    }

    fn stage_row(&self, stage: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let t = &self.theme;
        let ms = self.scale;
        // A test that cannot succeed is not offered as though it could: the
        // button goes quiet and the reason stands beside it, naming the field
        // and the tab it is on.
        let blocked = self.blocking(cx);
        let mut row = div().flex().items_center().gap_2().py_1().child(
            div()
                .id(SharedString::from(format!("test-{stage}")))
                .px_2()
                .text_size(ms.small())
                .border_1()
                .border_color(if blocked.is_some() { t.border() } else { t.accent() })
                .text_color(if blocked.is_some() { t.muted() } else { t.fg() })
                .when(blocked.is_none(), |d| {
                    d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                        this.run_test(stage, cx);
                        cx.notify();
                    }))
                })
                .child(SharedString::from(format!("Test {stage}"))),
        );
        if let Some(why) = blocked {
            return row.child(self.small(why, t.muted()));
        }
        // The engine's answer, verbatim: it names the layer, and summarising it
        // would throw away the only useful part.
        row = match self.tests.get(stage) {
            Some(Ok(detail)) => row.child(self.small(detail.clone(), t.success())),
            Some(Err(e)) => row.child(self.small(e.clone(), t.error())),
            None => row,
        };
        row
    }
}

impl ConnectionForm {
    /// Wear the new theme, fields included.
    ///
    /// A window is given a theme when it opens and nothing ever told it about a
    /// change, so one left open across a theme switch kept the old palette —
    /// and its fields kept it twice over, since they are handed their colours
    /// at construction.
    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        let colors = crate::form::colors_of(&self.theme);
        for input in self.all_inputs() {
            input.update(cx, |i, cx| i.set_colors(colors, cx));
        }
        cx.notify();
    }
}

impl Render for ConnectionForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme.clone();
        let ms = self.scale;
        let display = self
            .meta()
            .map(|m| m.display.clone())
            .unwrap_or_else(|| self.driver.clone());

        let mut head = div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .px_3()
            .py_2()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_color(t.fg()).child(SharedString::from(format!(
                        "{} {display} connection",
                        if self.editing.is_some() { "Edit" } else { "New" }
                    ))))
                    .child(self.small(self.tr("form.required"), t.muted())),
            );
        // The name sits top right, not as the first row of the form.
        head = head.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_none()
                .child(self.small(self.tr("form.name"), t.muted()))
                .child(
                    // A real width, and a floor under it: at a large text size
                    // `field_w` is wider than what the heading leaves, and the
                    // field then squeezed the heading to nothing rather than
                    // giving any of its own room back.
                    div()
                        .w(px(ms.field_w()))
                        .min_w(px(ms.base() * 10.0))
                        .flex_shrink()
                        .child(self.name.clone()),
                ),
        );

        let mut tabs = div()
            .flex()
            .gap_3()
            .w_full()
            .px_3()
            .py_1()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border());
        for ft in [FormTab::Main, FormTab::Access, FormTab::Hooks, FormTab::Settings] {
            let on = self.tab == ft;
            tabs = tabs.child(
                div()
                    .id(ft.id())
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.body())
                    .text_color(if on { t.fg() } else { t.muted() })
                    .when(on, |d| d.border_b_1().border_color(t.accent()))
                    .child(self.tr(ft.key()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = ft;
                        cx.notify();
                    })),
            );
        }
        // The tunnel is worth seeing from outside its tab — but as a BADGE,
        // not as a fourth tab. Set in the same row at the same size and colour
        // as the three tabs, it was read as one, and reported as "an error with
        // ssh": somebody clicked what looked like a tab and nothing happened.
        // Pushed to the far edge, filled, and smaller, it is a state and not a
        // destination.
        if self.tunnel_on {
            tabs = tabs.child(
                div()
                    .ml_auto()
                    .px_1()
                    .bg(t.accent())
                    .text_size(ms.tiny())
                    .text_color(t.inverted())
                    .child(self.tr("form.ssh")),
            );
        }

        // It SCROLLS. The Access tab with a tunnel open is taller than the
        // window at any text size over about 16, and `overflow_hidden` simply
        // cut the bottom off — including the "save as profile" row, which is
        // the only way a tunnel becomes reusable. `min_h_0` is the half that is
        // easy to leave out: a flex child refuses to shrink below its content
        // without it, so the footer went off the bottom instead.
        let mut body = crate::widgets::scroll("form-body")
            .outer(|d| d.p_3())
            .rows(|d| d.gap_1());
        match self.tab {
            FormTab::Main => {
                // The TYPE comes along, because how a field is drawn is the
                // driver's declaration and not the client's guess at its name.
                let keys: Vec<(String, String, String)> = self
                    .meta()
                    .map(|m| {
                        m.params
                            .iter()
                            .map(|p| {
                                (
                                    p.key.clone(),
                                    if p.required {
                                        format!("{} *", p.label)
                                    } else {
                                        p.label.clone()
                                    },
                                    p.kind.clone(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let drivers: Vec<(String, String)> =
                    self.drivers.iter().map(|d| (d.kind.clone(), d.display.clone())).collect();

                let mut picker = div().flex().flex_wrap().gap_1().py_1();
                for (kind, disp) in drivers {
                    let on = kind == self.driver;
                    picker = picker.child(
                        chip(&t, ms, format!("drv-{kind}"), disp, on)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.driver != kind {
                                    this.driver = kind.clone();
                                    // The new driver's fields, with its defaults.
                                    this.rebuild_params(&json!({}), cx);
                                    this.tests.clear();
                                }
                                cx.notify();
                            })),
                    );
                }
                body = body.child(picker);

                for (key, label, kind) in keys {
                    let Some(f) = self.params.get(&key) else { continue };
                    let (input, source, secret) = (f.input.clone(), f.source, f.secret);
                    // A parameter the driver declares as an AWS profile gets the
                    // names the engine read out of `~/.aws/config`. Chosen by
                    // TYPE: a client that matched the key `aws_profile` would
                    // need editing for the next driver that wants one, and would
                    // be guessing at a name instead of reading a declaration.
                    // A driver's own FILE parameter — sqlite, duckdb,
                    // firebird — gets the chooser too, by TYPE rather than by
                    // the client knowing those three by name.
                    if kind == "file" {
                        body = body.child(self.path_row_dyn(&label, &input, &key, cx));
                        continue;
                    }
                    if kind == "aws_profile" {
                        let mut pick = div().flex().flex_wrap().items_center().gap_1().py_1().child(
                            div().w(px(ms.label_w())).child(self.small(label.clone(), t.muted())),
                        );
                        let current = input.read(cx).text().trim().to_string();
                        for name in self.aws_profiles.clone() {
                            let on = current == name;
                            let (pick_name, target) = (name.clone(), input.clone());
                            pick = pick.child(
                                chip(&t, ms, format!("awsp-{name}"), name.clone(), on)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        target.update(cx, |i, cx| {
                                            i.set_text(pick_name.clone(), cx)
                                        });
                                        cx.notify();
                                    })),
                            );
                        }
                        body = body.child(pick);
                        // Still typable. The file may not be there, or may be
                        // somewhere this engine was not told to look, and a
                        // picker that is the ONLY way in would make a working
                        // profile unreachable.
                        body = body.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .py(px(2.0))
                                .child(div().w(px(ms.label_w())))
                                .child(div().flex_1().text_size(ms.body()).child(input.clone())),
                        );
                        if self.aws_profiles.is_empty() {
                            body = body.child(self.small(self.tr("form.no_aws"), t.muted()));
                        }
                        continue;
                    }
                    body = body.child(self.row(
                        &label,
                        &input,
                        source,
                        secret,
                        SharedString::from(key.clone()),
                        cx,
                    ));
                }

                let auths: Vec<String> = self.meta().map(|m| m.auth.clone()).unwrap_or_default();
                let mut auth_picker = div().flex().items_center().gap_2().py_1().child(
                    div().w(px(ms.label_w())).child(self.small(self.tr("form.auth"), t.muted())),
                );
                for a in auths {
                    let on = a == self.auth_kind;
                    auth_picker = auth_picker.child(
                        chip(&t, ms, format!("auth-{a}"), a.clone(), on)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.auth_kind = a.clone();
                                cx.notify();
                            })),
                    );
                }
                body = body.child(auth_picker);
                // EACH KIND ITS OWN FIELDS. One pair of boxes stood for all of
                // them and was squeezed into whichever shape the kind wanted on
                // the way out, which is why a client certificate could not be
                // given one and a provider could not be chosen.
                let plain = |this: &Self, label: &'static str, input: &Entity<TextInput>| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py(px(2.0))
                        .child(div().w(px(ms.label_w())).child(this.small(label, t.muted())))
                        .child(div().flex_1().text_size(ms.body()).child(input.clone()))
                };
                if matches!(self.auth_kind.as_str(), "password" | "client_cert" | "provider") {
                    body = body.child(plain(self, "User", &self.auth_user));
                }
                match self.auth_kind.as_str() {
                    "password" => {
                        let (i, src) = (self.auth_password.input.clone(), self.auth_password.source);
                        body = body.child(self.row(&self.tr("form.password"), &i, src, true, "auth-password".into(), cx));
                    }
                    "client_cert" => {
                        let (c, k) = (self.auth_cert.clone(), self.auth_key.clone());
                        body = body.child(self.path_row(self.tr("form.certificate"), &c, "pick-cert", cx));
                        body = body.child(self.path_row(self.tr("form.key"), &k, "pick-ckey", cx));
                        if let Some(why) = self.auth_file_problem(cx) {
                            body = body.child(self.small(why, t.error()));
                        }
                        let (i, src) =
                            (self.auth_key_password.input.clone(), self.auth_key_password.source);
                        body = body.child(self.row(
                            "Key password",
                            &i,
                            src,
                            true,
                            "auth-key-password".into(),
                            cx,
                        ));
                    }
                    "provider" => {
                        // The providers the ENGINE found, not free text. A name
                        // typed by hand is a name that fails at connect time.
                        let mut pr = div().flex().items_center().gap_2().py_1().child(
                            div().w(px(ms.label_w())).child(self.small(self.tr("form.provider"), t.muted())),
                        );
                        for p in PROVIDERS {
                            let on = self.auth_provider == p;
                            let pick = p;
                            pr = pr.child(
                                chip(&t, ms, format!("prov-{p}"), p, on)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.auth_provider = pick.to_string();
                                        cx.notify();
                                    })),
                            );
                        }
                        body = body.child(pr);
                        let (i, src) = (self.auth_token.input.clone(), self.auth_token.source);
                        body = body.child(self.row(&self.tr("form.token"), &i, src, true, "auth-token".into(), cx));
                        // Said plainly: the name is RECORDED and, today, read by
                        // nothing. Snowflake is the only driver that takes a
                        // provider and it uses the token alone. Better to say so
                        // than to let a chosen name imply an effect it has not
                        // got.
                        body = body.child(self.small(self.tr("form.token_note"), t.muted()));
                    }
                    "kerberos" => {
                        body = body.child(plain(self, "Principal", &self.auth_principal));
                    }
                    _ => {}
                }
                if self.auth_kind != "none" {
                    body = body.child(self.small(self.tr("form.source_note"), t.muted()));
                }
                body = body.child(self.stage_row("endpoint", cx));
                body = body.child(self.stage_row("auth", cx));
            }
            FormTab::Access => {
                // What the ENGINE says this driver can do. The client offers
                // nothing else: a row of five SSL modes and a "Test tls" button
                // on a driver that declares `tls: false` is the client promising
                // something the engine will refuse.
                let has_tls = self.meta().map_or(false, |m| m.caps.tls);
                let has_tunnel = self.meta().map_or(false, |m| m.caps.tunnel);
                if !has_tls {
                    body = body.child(self.small(
                        format!("{display} does not offer TLS, so there is nothing to configure here."),
                        t.muted(),
                    ));
                }
                if has_tls {
                let mut tls = div().flex().items_center().gap_2().py_1().child(
                    div().w(px(ms.label_w())).child(self.small(self.tr("form.ssl_mode"), t.muted())),
                );
                for m in ["disable", "prefer", "require", "verify_ca", "verify_full"] {
                    let on = self.tls_mode == m;
                    tls = tls.child(
                        chip(&t, ms, format!("tls-{m}"), m, on)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tls_mode = m.to_string();
                                cx.notify();
                            })),
                    );
                }
                body = body.child(tls);
                if self.tls_mode == "disable" {
                    body = body.child(self.small(self.tr("form.plaintext_note"), t.warning()));
                }
                // The three files, under the mode they belong to. Shown for
                // every TLS-capable driver: which of them a server wants is the
                // server's business, and an empty box costs nothing.
                body = body.child(self.path_row(self.tr("form.ca_bundle"), &self.tls_ca.clone(), "pick-tls-ca", cx));
                body = body
                    .child(self.path_row(self.tr("form.client_cert"), &self.tls_cert.clone(), "pick-tls-cert", cx));
                body = body
                    .child(self.path_row(self.tr("form.client_key"), &self.tls_key.clone(), "pick-tls-key", cx));
                body = body.child(self.stage_row("tls", cx));
                }

                // THE PROXY, above the tunnel, because that is the order the
                // two happen in: a proxy is how this machine reaches anything
                // at all, and a tunnel rides on top of a reachable SSH server.
                //
                // Drawn for every driver, unlike the tunnel: a proxy carries
                // the TCP dial itself, so it applies wherever there is one.
                let mut pxp = div().flex().items_center().gap_2().py_1().child(
                    div().w(px(ms.label_w())).child(self.small(self.tr("form.proxy"), t.muted())),
                );
                pxp = pxp.child(
                    chip(&t, ms, "px-none", self.tr("form.none"), self.proxy_profile.is_none())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.proxy_profile = None;
                            cx.notify();
                        })),
                );
                for name in self.proxy_profiles.clone() {
                    let on = self.proxy_profile.as_deref() == Some(name.as_str());
                    let n = name.clone();
                    pxp = pxp.child(
                        chip(&t, ms, format!("px-p-{name}"), name.clone(), on).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.proxy_profile = Some(n.clone());
                                cx.notify();
                            }),
                        ),
                    );
                }
                // TEST it on its own, before the connection: a proxy that
                // cannot be reached fails every stage above it, and all of
                // them would say the database was unreachable.
                if self.proxy_profile.is_some() {
                    pxp = pxp.child(
                        btn(&t, ms, "test-proxy", self.tr("form.test_proxy"), Btn::Secondary)
                            .on_click(cx.listener(|this, _, _, cx| this.run_test("proxy", cx))),
                    );
                }
                body = body.child(pxp);
                // The engine's own answer, verbatim, in the same shape the
                // other stages report theirs.
                match self.tests.get("proxy") {
                    Some(Ok(detail)) => {
                        body = body.child(self.small(detail.clone(), t.success()))
                    }
                    Some(Err(e)) => body = body.child(self.small(e.clone(), t.error())),
                    None => {}
                }
                if self.proxy_profiles.is_empty() && self.proxy_profile.is_none() {
                    body = body.child(self.small(self.tr("form.no_proxies"), t.muted()));
                }
                // A proxy WRITTEN OUT in the spec, which this window does not
                // edit. Said rather than hidden, and said plainly: choosing a
                // profile above would replace it.
                if let Some(px) = self.existing_spec.get("proxy").filter(|v| !v.is_null()) {
                    let how = px.get("kind").and_then(Value::as_str).unwrap_or("proxy");
                    let host = px.get("host").and_then(Value::as_str).unwrap_or("?");
                    body = body.child(self.small(
                        format!("{} {how} \u{2192} {host}", self.tr("form.proxy_written_out")),
                        t.warning(),
                    ));
                }

                if !has_tunnel {
                    body = body.child(self.small(
                        format!("{display} cannot be reached through an SSH tunnel."),
                        t.muted(),
                    ));
                }
                if has_tunnel {
                body = body.child(
                    div()
                        .id("tunnel-toggle")
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .cursor_pointer()
                        // A drawn box with a tick in it, like every other
                        // checkbox in this window. `[x]` was the shape of a
                        // checkbox typed out rather than a checkbox.
                        .child(self.tick(self.tunnel_on))
                        .child(
                            div()
                                .text_size(ms.small())
                                .text_color(if self.tunnel_on { t.fg() } else { t.muted() })
                                .child(self.tr("form.ssh_tunnel")),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tunnel_on = !this.tunnel_on;
                            cx.notify();
                        })),
                );
                // Said even when the box is UNTICKED. The picker lives inside
                // the tunnel section, so with the tunnel off there was nothing
                // anywhere to say saved tunnels exist at all — and a feature
                // somebody cannot discover is a feature they do not have.
                if !self.tunnel_on && !self.tunnel_profiles.is_empty() {
                    body = body.child(self.small(
                        format!(
                            "{} saved tunnel(s) to choose from: {} \u{2014} tick SSH tunnel above.",
                            self.tunnel_profiles.len(),
                            self.tunnel_profiles.join(", ")
                        ),
                        t.accent(),
                    ));
                }
                if self.tunnel_on {
                    // Its own tunnel, or one of the saved ones. The engine
                    // refuses a spec that names a profile AND spells a tunnel
                    // out, so this is a choice rather than two things to fill.
                    let mut pick = div().flex().items_center().gap_2().py_1().child(
                        div().w(px(ms.label_w())).child(self.small(self.tr("form.tunnel"), t.muted())),
                    );
                    let own = self.tunnel_profile.is_none();
                    pick = pick.child(
                        chip(&t, ms, "tun-own", self.tr("form.own"), own)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tunnel_profile = None;
                                cx.notify();
                            })),
                    );
                    for name in self.tunnel_profiles.clone() {
                        let on = self.tunnel_profile.as_deref() == Some(name.as_str());
                        let n = name.clone();
                        pick = pick.child(
                            chip(&t, ms, format!("tun-p-{name}"), name.clone(), on)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.tunnel_profile = Some(n.clone());
                                    cx.notify();
                                })),
                        );
                    }
                    body = body.child(pick);
                    if self.tunnel_profiles.is_empty() {
                        body = body.child(self.small(self.tr("form.no_tunnels"), t.muted()));
                    }
                }
                // A chosen profile REPLACES the fields rather than sitting
                // beside empty ones: the values are in the profile, and a row of
                // blanks next to its name reads as something left unfilled.
                if self.tunnel_on && self.tunnel_profile.is_some() {
                    let name = self.tunnel_profile.clone().unwrap_or_default();
                    // WHAT it is, not just that it is. A name on its own does not
                    // tell somebody what their connection goes through, and
                    // hunting for it in another window to find out is the thing
                    // this is here to avoid.
                    let summary = self
                        .tunnel_profile_specs
                        .get(&name)
                        .map(|sp| {
                            let g = |k: &str| {
                                sp.get(k).and_then(Value::as_str).unwrap_or("").to_string()
                            };
                            let port = sp.get("port").and_then(Value::as_u64).unwrap_or(22);
                            let auth = sp.get("auth").cloned().unwrap_or(json!({}));
                            let kind = auth.get("kind").and_then(Value::as_str).unwrap_or("agent");
                            let path = auth.get("path").and_then(Value::as_str).unwrap_or("");
                            let who = {
                                let u = g("user");
                                if u.is_empty() { String::new() } else { format!("{u}@") }
                            };
                            if kind == "key" && !path.is_empty() {
                                format!("{who}{}:{port} · key {path}", g("host"))
                            } else {
                                format!("{who}{}:{port} · {kind}", g("host"))
                            }
                        })
                        .unwrap_or_else(|| "(its values are not loaded yet)".to_string());
                    body = body.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py_1()
                            .child(
                                div()
                                    .w(px(ms.label_w()))
                                    .flex_none()
                                    .child(self.small(self.tr("form.goes_through"), t.muted())),
                            )
                            .child(div().flex_1().child(self.small(summary, t.fg())))
                            .child(
                                btn(&t, ms, "edit-this-profile", self.tr("form.edit_tunnel"), Btn::Link)
                                    .on_click(cx.listener({
                                        let n = name.clone();
                                        move |this, _, _, cx| {
                                            let sp = this
                                                .tunnel_profile_specs
                                                .get(&n)
                                                .cloned()
                                                .unwrap_or(json!({}));
                                            let n2 = n.clone();
                                            let _ = this.workspace.update(cx, move |w, cx| {
                                                w.open_profile("tunnel", Some((n2, sp)), cx);
                                            });
                                        }
                                    })),
                            ),
                    );
                    body = body.child(self.small(self.tr("form.profile_note"), t.muted()));
                    body = body.child(self.stage_row("tunnel", cx));
                }
                if self.tunnel_on && self.tunnel_profile.is_none() {
                    for (label, input) in [
                        (self.tr("form.ssh_host"), self.tunnel_host.clone()),
                        (self.tr("form.ssh_port"), self.tunnel_port.clone()),
                        (self.tr("form.ssh_user"), self.tunnel_user.clone()),
                    ] {
                        body = body.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .py(px(2.0))
                                .child(div().w(px(ms.label_w())).child(self.small(label, t.muted())))
                                .child(div().flex_1().child(input)),
                        );
                    }
                    let mut m = div().flex().items_center().gap_2().py_1().child(
                        div().w(px(ms.label_w())).child(self.small(self.tr("form.method"), t.muted())),
                    );
                    for a in ["agent", "key", "password"] {
                        let on = self.tunnel_auth == a;
                        m = m.child(
                            chip(&t, ms, format!("tauth-{a}"), a, on)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.tunnel_auth = a.to_string();
                                    cx.notify();
                                })),
                        );
                    }
                    body = body.child(m);
                    // Each method shows ITS OWN fields. Nothing a method writes
                    // is read back by another one.
                    if self.tunnel_auth == "key" {
                        let kp = self.tunnel_key_path.clone();
                        body = body.child(self.path_row(self.tr("form.key_file"), &kp, "pick-tkey", cx));
                        if let Some(why) = self.key_path_problem(cx) {
                            body = body.child(self.small(why, t.error()));
                        }
                        let (i, src) =
                            (self.tunnel_passphrase.input.clone(), self.tunnel_passphrase.source);
                        body = body.child(self.row(
                            "Passphrase",
                            &i,
                            src,
                            true,
                            "tunnel-passphrase".into(),
                            cx,
                        ));
                        body = body.child(self.small(self.tr("form.passphrase_note"), t.muted()));
                    }
                    if self.tunnel_auth == "password" {
                        let (i, src) =
                            (self.tunnel_password.input.clone(), self.tunnel_password.source);
                        body = body.child(self.row(&self.tr("ui.password"), &i, src, true, "tunnel-password".into(), cx));
                    }
                    body = body.child(self.stage_row("tunnel", cx));
                    // This is how a profile is CREATED: out of the fields
                    // somebody has just filled in and proved with "Test tunnel".
                    // A separate editor would be a second place to type the same
                    // three values.
                    body = body.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py_1()
                            .child(
                                div()
                                    .w(px(ms.label_w()))
                                    .child(self.small(self.tr("form.save_profile"), t.muted())),
                            )
                            .child(
                                div()
                                    .w(px(ms.field_w()))
                                    .text_size(ms.body())
                                    .child(self.profile_name.clone()),
                            )
                            .child(
                                btn(&t, ms, "tun-save-profile", self.tr("form.save"), Btn::Secondary).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.save_tunnel_profile(cx);
                                    }),
                                ),
                            ),
                    );
                    body = body.child(self.small(self.tr("form.save_over_note"), t.muted()));
                }
                }
            }
            FormTab::Hooks => {
                body = body.child(self.small(self.tr("hook.output_goes"), t.muted()));
                if self.hooks.is_empty() {
                    body = body.child(self.small(self.tr("hook.none"), t.muted()));
                }
                for i in 0..self.hooks.len() {
                    body = body.child(self.hook_el(i, cx));
                }
                body = body.child(
                    btn(&t, ms, "hook-add", self.tr("hook.add"), Btn::Secondary).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.add_hook(cx);
                            cx.notify();
                        }),
                    ),
                );
                // What this window could NOT read, said plainly and kept. A
                // client that quietly dropped it would turn "lode is older
                // than this spec" into "the spec lost half its hooks".
                if !self.hooks_kept.is_empty() {
                    body = body.child(self.small(
                        format!(
                            "{} hook(s) in this spec use something this window does not know; \
                             they are left exactly as they are.",
                            self.hooks_kept.len()
                        ),
                        t.warning(),
                    ));
                }
            }
            FormTab::Settings => {
                body = body.child(
                    div()
                        .pt_1()
                        .text_size(ms.tiny())
                        .text_color(t.header_fg())
                        .child(self.tr("form.policy_heading")),
                );
                body = body.child(self.small(self.tr("form.policy_note"), t.muted()));
                for (key, label, what) in [
                    ("read_only", "form.policy_read", "form.policy_read_why"),
                    ("confirm", "form.policy_confirm", "form.policy_confirm_why"),
                    ("open", "form.policy_open", "form.policy_open_why"),
                ] {
                    let on = self.policy == key;
                    body = body.child(
                        div()
                            .id(SharedString::from(format!("pol-{key}")))
                            .flex()
                            .flex_col()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .when(on, |d| d.bg(t.selected_mark()).border_l_2().border_color(t.accent()))
                            .when(!on, |d| d.border_l_2().border_color(t.panel()))
                            .hover(|d| d.bg(t.hover_mark()))
                            .child(
                                div()
                                    .text_size(ms.body())
                                    .text_color(if on { t.accent() } else { t.fg() })
                                    .child(self.tr(label)),
                            )
                            .child(self.small(self.tr(what), t.muted()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.policy = key.to_string();
                                cx.notify();
                            })),
                    );
                }
            }
        }

        // THE WALLET IS SHUT, and one passphrase opens it. Here rather than
        // in Settings: the key was pressed on this field, in this window, and
        // the answer to "it is locked" is a box to type in, not an errand.
        if self.unlocking.is_some() {
            body = body.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .my_1()
                    .p_2()
                    .border_1()
                    .border_color(t.warning())
                    .child(self.small(self.tr("wallet.locked"), t.warning()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(ms.base() * 14.0))
                                    .text_size(ms.body())
                                    .child(self.pass_input.clone()),
                            )
                            .child(
                                btn(&t, ms, "form-unlock", self.tr("ui.unlock"), Btn::Primary)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.unlock_then_store(cx)
                                    })),
                            )
                            .child(
                                btn(&t, ms, "form-unlock-no", self.tr("ui.cancel"), Btn::Quiet)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.unlocking = None;
                                        cx.notify();
                                    })),
                            ),
                    ),
            );
        }
        if let Some(e) = &self.error {
            body = body.child(self.small(e.clone(), t.error()));
        }
        if let Some(n) = &self.note {
            body = body.child(self.small(n.clone(), t.success()));
        }

        // Test on the left, Cancel and Save on the right — the arrangement a
        // dialog has everywhere, and the one dbflux uses.
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .px_3()
            .py_2()
            .bg(t.panel())
            .border_t_1()
            .border_color(t.border())
            .child(
                btn(&t, ms, "test-all", self.tr("form.test_conn"), Btn::Secondary).on_click(cx.listener(
                    |this, _, _, cx| {
                        // The deepest stage: it exercises everything under it.
                        this.run_test("auth", cx);
                        cx.notify();
                    },
                )),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(btn(&t, ms, "form-cancel", self.tr("ui.cancel"), Btn::Quiet).on_click(
                        cx.listener(|this, _, window: &mut Window, cx| this.leave(window, cx)),
                    ))
                    .child(btn(&t, ms, "form-save", self.tr("form.save"), Btn::Primary).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.save(cx);
                            cx.notify();
                        },
                    ))),
            );

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
            // The main window's type, handed over rather than written here.
            // This said "CodeNerdFont" — so somebody whose $LODE_FONT is
            // anything else got one window in a different face, and a machine
            // without that font got whatever the platform fell back to.
            .font_family(SharedString::from(self.font_family.clone()))
            .text_size(ms.body())
            // The KEYBOARD. Tab and shift-tab walk the fields; escape closes.
            // The actions bubble out of whichever field has the keyboard, and
            // are handled here because moving the focus needs a `Window` and a
            // field's own subscription has none.
            .on_action(cx.listener(|this, _: &crate::input::FocusNext, window, cx| {
                this.step_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::input::FocusPrev, window, cx| {
                this.step_focus(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &crate::app::Escape, window, cx| {
                this.leave(window, cx)
            }))
            .child(head)
            .child(tabs)
            .child(body)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Saving a connection must not delete what the form cannot see.** A spec
    /// written elsewhere — by the Neovim client, by hand — carries a CA bundle
    /// and a client certificate, and used to carry a proxy profile and hooks
    /// this window had no face for. The form used to send only the keys it
    /// knew, so opening such a connection and pressing Save silently stripped
    /// them. It edits the proxy profile and the hooks now; what it still does
    /// not show it still keeps, which is what this checks.
    #[test]
    fn a_save_keeps_the_keys_the_form_never_shows() {
        let existing = json!({
            "driver": "postgres",
            "params": { "host": "old.example", "port": "5432" },
            "auth": { "kind": "password", "user": "a", "password": "p" },
            "tls": { "mode": "require", "ca": "/etc/ssl/ca.pem", "client_cert": "/etc/ssl/c.pem" },
            "proxy_profile": "office",
            "hooks": { "before_connect": "notify" },
            "policy": "read_write"
        });
        let edited = json!({
            "driver": "postgres",
            "params": { "host": "new.example", "port": "5432" },
            "auth": { "kind": "password", "user": "b", "password": "q" },
            "tls": { "mode": "verify_full" },
            "tunnel": Value::Null,
            "tunnel_profile": Value::Null,
            "policy": "read_only"
        });

        let out = merge_spec(&existing, edited);
        // What the form owns is the form's.
        assert_eq!(out["params"]["host"], json!("new.example"));
        assert_eq!(out["auth"]["user"], json!("b"));
        assert_eq!(out["policy"], json!("read_only"));
        assert_eq!(out["tls"]["mode"], json!("verify_full"));
        // What it does not own survives untouched — including the rest of `tls`.
        assert_eq!(out["tls"]["ca"], json!("/etc/ssl/ca.pem"));
        assert_eq!(out["tls"]["client_cert"], json!("/etc/ssl/c.pem"));
        assert_eq!(out["proxy_profile"], json!("office"));
        assert_eq!(out["hooks"]["before_connect"], json!("notify"));
    }

    /// A hook this window cannot read is KEPT, not dropped.
    ///
    /// The engine will grow moments and kinds this build has never heard of,
    /// and the Neovim client may already write one. A client that quietly
    /// deleted what it failed to parse would turn "lode is older than this
    /// spec" into "the spec lost half its hooks" — and the person would find
    /// out when the hook stopped running, not when it was dropped.
    #[test]
    fn a_hook_this_window_cannot_read_survives_it() {
        let spec = json!({ "hooks": [
            { "on": "pre_connect", "kind": "command", "source": "vpn up" },
            // A moment this build does not know.
            { "on": "pre_query", "kind": "command", "source": "echo hi" },
            // A kind it does not know.
            { "on": "post_connect", "kind": "webhook", "source": "https://x" },
            // No kind at all, which the engine reads as a command.
            { "on": "post_disconnect", "source": "vpn down" },
        ]});
        let (known, kept) = super::read_hooks(&spec);
        assert_eq!(known.len(), 2, "{known:?}");
        assert_eq!(known[0]["source"], json!("vpn up"));
        assert_eq!(known[1]["source"], json!("vpn down"));
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert_eq!(kept[0]["on"], json!("pre_query"));
        assert_eq!(kept[1]["kind"], json!("webhook"));
        // Nothing is lost between the two halves.
        assert_eq!(known.len() + kept.len(), 4);
        // And a spec with no hooks is no rows, not one empty row.
        assert_eq!(super::read_hooks(&json!({})), (vec![], vec![]));
    }

    /// Only ONE moment stops anything, and the window must say which.
    ///
    /// The other three run after the thing they are named for, so there is
    /// nothing left to stop. Telling somebody that a post-disconnect hook can
    /// refuse a disconnect would be telling them something the engine does not
    /// do.
    #[test]
    fn only_a_hook_before_the_dial_stops_it() {
        assert!(super::hook_blocks("pre_connect"));
        for other in ["post_connect", "pre_disconnect", "post_disconnect"] {
            assert!(!super::hook_blocks(other), "{other}");
        }
        // Every moment the window offers is one the engine knows.
        assert_eq!(super::HOOK_EVENTS.len(), 4);
        assert!(super::HOOK_EVENTS.contains(&"pre_connect"));
    }

    /// Naming a proxy profile takes a spelled-out proxy OFF, and choosing none
    /// leaves one alone.
    ///
    /// The engine refuses a spec carrying both rather than picking between
    /// them, so the two are exclusive. But "none chosen" and "never had one"
    /// are different: this window does not show a spelled-out proxy and so has
    /// no business deleting it — only an actual choice replaces it.
    #[test]
    fn a_proxy_is_a_profile_or_written_out_but_never_both() {
        let spelled = json!({
            "driver": "mysql",
            "proxy": { "kind": "socks5", "host": "socks.example", "port": 1080 },
            "tls": { "mode": "disable" }
        });
        // NONE chosen: the spelled-out one survives, because the form neither
        // showed it nor replaced it.
        let out = merge_spec(&spelled, json!({ "proxy_profile": Value::Null }));
        assert_eq!(out["proxy"]["host"], json!("socks.example"));
        assert!(out.get("proxy_profile").is_none(), "{out}");

        // A profile CHOSEN: the spelled-out one goes, or the engine would
        // refuse the connection for carrying two answers.
        let out = merge_spec(&spelled, json!({ "proxy_profile": "office", "proxy": Value::Null }));
        assert_eq!(out["proxy_profile"], json!("office"));
        assert!(out.get("proxy").is_none(), "{out}");

        // And a profile UNCHOSEN goes off the spec rather than being written
        // as null, the same way a tunnel does.
        let with_profile = json!({ "driver": "mysql", "proxy_profile": "office" });
        let out = merge_spec(&with_profile, json!({ "proxy_profile": Value::Null }));
        assert!(out.get("proxy_profile").is_none(), "{out}");
    }

    /// A tunnel switched off is REMOVED, not written as null: the engine reads
    /// the key's presence, and the two names are exclusive — naming a profile
    /// takes the spelled-out tunnel off the spec and the other way round.
    #[test]
    fn switching_a_tunnel_off_takes_it_off_the_spec() {
        let with_tunnel = json!({
            "driver": "mysql",
            "tunnel": { "host": "jump", "auth": { "kind": "key", "path": "~/.ssh/id" } },
            "tls": { "mode": "disable" }
        });

        let off = merge_spec(
            &with_tunnel,
            json!({ "tunnel": Value::Null, "tunnel_profile": Value::Null, "tls": { "mode": "disable" } }),
        );
        assert!(off.get("tunnel").is_none(), "an unticked tunnel is gone, not null: {off}");
        assert!(off.get("tunnel_profile").is_none());

        let by_profile = merge_spec(
            &with_tunnel,
            json!({ "tunnel": Value::Null, "tunnel_profile": "office", "tls": { "mode": "disable" } }),
        );
        assert!(by_profile.get("tunnel").is_none(), "a profile replaces a spelled-out tunnel");
        assert_eq!(by_profile["tunnel_profile"], json!("office"));
    }

    /// The three TLS files round-trip, and an empty box takes its key off the
    /// spec rather than saving a path to nowhere.
    #[test]
    fn the_tls_files_go_in_and_come_back_out() {
        let saved = merge_spec(
            &json!({ "driver": "postgres" }),
            json!({ "tls": { "mode": "verify_full", "ca": "~/ca.pem", "client_cert": "~/c.pem", "client_key": "~/c.key" } }),
        );
        assert_eq!(saved["tls"]["ca"], json!("~/ca.pem"));
        assert_eq!(saved["tls"]["client_cert"], json!("~/c.pem"));
        assert_eq!(saved["tls"]["client_key"], json!("~/c.key"));

        // Cleared in the window: gone from the spec, not an empty string.
        let cleared = merge_spec(
            &saved,
            json!({ "tls": { "mode": "require", "ca": Value::Null, "client_cert": Value::Null, "client_key": Value::Null } }),
        );
        assert_eq!(cleared["tls"]["mode"], json!("require"));
        assert!(cleared["tls"].get("ca").is_none(), "an empty box is no key: {cleared}");
        assert!(cleared["tls"].get("client_cert").is_none());
        assert!(cleared["tls"].get("client_key").is_none());
    }

    /// A new connection has nothing to merge onto, and must come out as the
    /// engine's shape all the same.
    #[test]
    fn a_new_connection_merges_onto_nothing() {
        let out = merge_spec(&json!({}), json!({ "driver": "sqlite", "tls": { "mode": "disable" } }));
        assert_eq!(out["driver"], json!("sqlite"));
        assert_eq!(out["tls"]["mode"], json!("disable"));
    }

    /// Each auth kind carries ITS OWN keys and not one belonging to another.
    ///
    /// This is the check that would have caught all three of the field-sharing
    /// bugs: the tunnel path that was really a passphrase, the `cert` and `key`
    /// hardcoded empty so a client certificate could never be configured, and
    /// the provider pinned to the literal string "token". Every one of them is
    /// a value written under a key that did not mean it.
    ///
    /// The engine's `AuthSpec` (`src/spec.rs`) is the list being asserted.
    #[test]
    fn every_auth_kind_writes_its_own_keys_and_no_others() {
        // kind → exactly the keys the engine's variant declares.
        let expected: [(&str, &[&str]); 5] = [
            ("none", &[]),
            ("password", &["user", "password"]),
            ("client_cert", &["cert", "key", "key_password", "user"]),
            ("provider", &["provider", "token", "user"]),
            ("kerberos", &["principal"]),
        ];
        // Every key any kind uses, so a stray one can be spotted by name.
        let all: Vec<&str> = vec![
            "user", "password", "cert", "key", "key_password", "provider", "token", "principal",
        ];

        for (kind, keys) in expected {
            // THE REAL BUILDER, with every box filled in — so a value that
            // leaks from one kind into another is visible as itself.
            let built = auth_json(AuthParts {
                kind,
                user: "the-user",
                password: "the-password",
                cert: "/the/cert.pem",
                key: "/the/key.pem",
                key_password: "the-key-password",
                provider: "aws",
                token: "the-token",
                principal: "me@REALM",
            });
            let obj = built.as_object().expect("an object");

            for k in keys {
                assert!(obj.contains_key(*k), "{kind} is missing its own key {k}");
            }
            for k in &all {
                if keys.contains(k) {
                    continue;
                }
                assert!(
                    !obj.contains_key(*k),
                    "{kind} carries {k}, which belongs to another kind"
                );
            }
            // And nothing is present-but-blank, which is how `cert` and `key`
            // were "there" while making the kind unusable.
            for k in keys {
                if let Some(v) = obj.get(*k).and_then(Value::as_str) {
                    assert!(!v.is_empty(), "{kind}: {k} is present but empty");
                }
            }
        }

        // The three faults that were actually in the code, named one by one.
        let cc = auth_json(AuthParts {
            kind: "client_cert",
            user: "u",
            password: "",
            cert: "/the/cert.pem",
            key: "/the/key.pem",
            key_password: "kp",
            provider: "aws",
            token: "",
            principal: "",
        });
        assert_eq!(cc["cert"], "/the/cert.pem", "cert was hardcoded empty");
        assert_eq!(cc["key"], "/the/key.pem", "key was hardcoded empty");

        let pv = auth_json(AuthParts {
            kind: "provider",
            user: "u",
            password: "",
            cert: "",
            key: "",
            key_password: "",
            provider: "oidc",
            token: "t",
            principal: "",
        });
        assert_eq!(pv["provider"], "oidc", "the provider was hardcoded to \"token\"");

        let kb = auth_json(AuthParts {
            kind: "kerberos",
            user: "u",
            password: "",
            cert: "",
            key: "",
            key_password: "",
            provider: "",
            token: "",
            principal: "me@REALM",
        });
        assert_eq!(kb["principal"], "me@REALM", "the principal was hardcoded null");
        // An empty principal is absent, not "".
        let kb2 = auth_json(AuthParts { principal: "  ", ..AuthParts {
            kind: "kerberos", user: "", password: "", cert: "", key: "",
            key_password: "", provider: "", token: "", principal: "",
        }});
        assert!(kb2["principal"].is_null());
    }

    /// Naming a profile and spelling a tunnel out are mutually exclusive: the
    /// engine refuses a spec holding both rather than picking one, so the form
    /// has to send exactly one of them.
    #[test]
    fn a_spec_carries_a_profile_or_a_tunnel_and_never_both() {
        // What the form builds when a profile is chosen.
        let with_profile = json!({
            "driver": "mongodb",
            "params": {},
            "auth": { "kind": "none" },
            "tls": { "mode": "prefer" },
            "tunnel_profile": "bastion",
            "policy": "confirm"
        });
        assert!(with_profile.get("tunnel").is_none(), "no spelled-out tunnel beside a profile");
        assert_eq!(
            with_profile.get("tunnel_profile").and_then(Value::as_str),
            Some("bastion")
        );

        // And when it is spelled out.
        let with_tunnel = json!({
            "driver": "mongodb",
            "params": {},
            "auth": { "kind": "none" },
            "tls": { "mode": "prefer" },
            "tunnel": { "host": "b.example", "port": 22, "user": "me",
                        "auth": { "kind": "agent" } },
            "policy": "confirm"
        });
        assert!(
            with_tunnel.get("tunnel_profile").is_none(),
            "no profile name beside a spelled-out tunnel"
        );
        assert!(with_tunnel.get("tunnel").is_some());
    }

    /// A profile's one-line description is read off its own spec, so it cannot
    /// go stale the way a description stored beside it would.
    #[test]
    fn a_profile_describes_itself_from_its_spec() {
        use crate::app::ProfileRow;
        let r = ProfileRow {
            name: "bastion".into(),
            spec: json!({ "host": "b.example", "port": 2222, "user": "me",
                          "auth": { "kind": "key" } }),
        };
        assert_eq!(r.summary(), "me@b.example:2222 · key");

        // A profile with nothing useful in it says so rather than inventing.
        let empty = ProfileRow { name: "x".into(), spec: json!({}) };
        assert_eq!(empty.summary(), "\u{2014}");
    }

    /// The reported corruption, as a test on the shapes that caused it.
    ///
    /// A `key` tunnel's three values live in three different places in the
    /// spec, and the form used ONE field for them. Loading read `passphrase`
    /// (falling back to `password`) and ignored `path`; saving wrote that one
    /// field out AS `path` and hardcoded `passphrase` to `""`. So opening a
    /// working connection and pressing save moved the passphrase into the path
    /// and threw the real path away — which is exactly what happened.
    ///
    /// This asserts the reading and writing agree, key by key.
    #[test]
    fn a_key_tunnel_keeps_its_path_and_its_passphrase_apart() {
        let saved = json!({
            "host": "bastion.example",
            "port": 22u16,
            "user": "me",
            "auth": {
                "kind": "key",
                "path": "/home/me/.ssh/id_ed25519",
                "passphrase": "s3cret-phrase-with-punctuation!"
            }
        });
        let auth = saved.get("auth").cloned().unwrap();

        // What the form reads for each of its three fields.
        let path = auth.get("path").and_then(Value::as_str).unwrap_or_default();
        let passphrase = auth.get("passphrase").and_then(Value::as_str).unwrap_or_default();
        let password = auth.get("password").and_then(Value::as_str).unwrap_or_default();

        assert_eq!(path, "/home/me/.ssh/id_ed25519", "the path is read from `path`");
        assert_eq!(
            passphrase, "s3cret-phrase-with-punctuation!",
            "the passphrase is read from `passphrase`"
        );
        assert!(password.is_empty(), "a key tunnel has no password field to read");

        // The old code's single field: passphrase-or-password, never the path.
        let what_the_old_form_loaded = if passphrase.is_empty() { password } else { passphrase };
        assert_ne!(
            what_the_old_form_loaded, path,
            "the one-field form loaded the passphrase and would write it back as the path"
        );

        // And what a save produces now: each value back where it came from.
        let written = json!({
            "kind": "key",
            "path": path,
            "passphrase": passphrase,
        });
        assert_eq!(written, auth, "a load followed by a save changes nothing");
    }

    /// A value's source becomes the template the engine resolves — and comes
    /// back out of it unchanged when the connection is edited again.
    #[test]
    fn a_source_round_trips_through_its_template() {
        for (src, arg, template) in [
            (Source::Env, "PGPASSWORD", "{{ env \"PGPASSWORD\" }}"),
            (Source::Command, "aws rds token", "{{ cmd \"aws rds token\" }}"),
            (Source::Keyring, "prod/db", "{{ vault \"prod/db\" }}"),
        ] {
            assert_eq!(src.wrap(arg), template, "{} must wrap", src.label());
            let (back, got) = Source::unwrap(template);
            assert!(back == src, "{template} must read back as {}", src.label());
            assert_eq!(got, arg);
        }
    }

    #[test]
    fn a_literal_is_stored_as_typed() {
        assert_eq!(Source::Literal.wrap("hunter2"), "hunter2");
        let (src, arg) = Source::unwrap("hunter2");
        assert!(src == Source::Literal);
        assert_eq!(arg, "hunter2");
    }

    /// An empty field contributes nothing, whatever its source — otherwise an
    /// untouched Keyring box would save `{{ vault "" }}`.
    #[test]
    fn an_empty_value_stays_empty() {
        for s in Source::all() {
            assert_eq!(s.wrap(""), "", "{} must not wrap nothing", s.label());
            assert_eq!(s.wrap("   "), "");
        }
    }

    /// The wallet name follows lvim-db for the password and hangs the rest
    /// below it.
    ///
    /// The point of the first half is that the Neovim client finds the SAME
    /// secret rather than a second copy of it; the point of the second is that
    /// a connection with a password, a key passphrase and a token would
    /// otherwise store three secrets under one name and keep the last.
    #[test]
    fn a_secret_is_named_after_its_connection() {
        assert_eq!(super::vault_name("prod", "auth-password"), "db/prod");
        assert_eq!(super::vault_name("prod", "password"), "db/prod");
        assert_eq!(super::vault_name("prod", "auth-token"), "db/prod/token");
        assert_eq!(super::vault_name("prod", "auth-key-password"), "db/prod/key-password");
        assert_eq!(super::vault_name("prod", "tunnel-passphrase"), "db/prod/tunnel-passphrase");
        // Spaces around the name are the typist's, not the name's.
        assert_eq!(super::vault_name("  prod  ", "auth-password"), "db/prod");
        // Four secrets, four names.
        let names: std::collections::BTreeSet<String> = [
            "auth-password",
            "auth-key-password",
            "auth-token",
            "tunnel-passphrase",
            "tunnel-password",
        ]
        .iter()
        .map(|f| super::vault_name("one", f))
        .collect();
        assert_eq!(names.len(), 5, "{names:?}");
    }

    /// Something that merely looks like a template is not one.
    #[test]
    fn a_half_template_reads_as_a_literal() {
        for odd in ["{{ env \"X\"", "{{ nope \"X\" }}", "{{ env X }}"] {
            let (src, arg) = Source::unwrap(odd);
            assert!(src == Source::Literal, "{odd} is not a template we resolve");
            assert_eq!(arg, odd);
        }
    }
}
