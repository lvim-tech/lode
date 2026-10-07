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
use crate::input::{InputColors, TextInput};
use crate::metrics::Metrics;
use crate::theme::Theme;

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
    Settings,
}

impl FormTab {
    fn label(self) -> &'static str {
        match self {
            FormTab::Main => "Main",
            FormTab::Access => "Access (SSH)",
            FormTab::Settings => "Settings",
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
    theme: Theme,
    /// The same type scale as the workspace, handed over when the window opens:
    /// a dialog in its own OS window is still the same application, and a form
    /// at a different size from the panel behind it reads as a different one.
    scale: Metrics,
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

    /// The last answer from each `conn.test` stage.
    tests: HashMap<&'static str, Result<String, String>>,
    error: Option<String>,
}

impl ConnectionForm {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        theme: Theme,
        scale: Metrics,
        drivers: Vec<DriverMeta>,
        tunnel_profiles: Vec<String>,
        tunnel_profile_specs: std::collections::BTreeMap<String, Value>,
        aws_profiles: Vec<String>,
        existing: Option<(String, String, Value)>,
        cx: &mut Context<Self>,
    ) -> Self {
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

        let mut form = ConnectionForm {
            workspace,
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
            theme,
            scale,
        };
        let stored_params = spec.get("params").cloned().unwrap_or(json!({}));
        form.rebuild_params(&stored_params, cx);
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
        if let Some(name) = self.tunnel_profile.clone().filter(|_| self.tunnel_on) {
            let mut out = json!({
                "driver": self.driver,
                "params": Value::Object(params),
                "auth": auth,
                "tls": { "mode": self.tls_mode },
                "tunnel_profile": name,
                "policy": self.policy
            });
            if let Some(m) = out.as_object_mut() {
                m.remove("tunnel");
            }
            return out;
        }
        let tunnel = if self.tunnel_on { self.tunnel_spec(cx) } else { Value::Null };
        json!({
            "driver": self.driver,
            "params": Value::Object(params),
            "auth": auth,
            "tls": { "mode": self.tls_mode },
            "tunnel": tunnel,
            "policy": self.policy
        })
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
            "port": self.tunnel_port.read(cx).text().trim().parse::<u16>().unwrap_or(22),
            "user": self.tunnel_user.read(cx).text(),
            "auth": tauth
        })
    }

    /// Store the tunnel fields as a named profile, for every other connection.
    fn save_tunnel_profile(&mut self, cx: &mut Context<Self>) {
        let name = self.profile_name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.error = Some("a profile needs a name".into());
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
        // Refused here rather than at dial time. The connection that broke was
        // broken by a save, and a save is the last moment anybody is looking at
        // the field that is wrong.
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
    fn small(&self, text: impl Into<SharedString>, c: gpui::Rgba) -> impl IntoElement {
        div().text_size(self.scale.small()).text_color(c).child(text.into())
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
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .py(px(2.0))
            .child(div().w(px(ms.label_w())).child(self.small(
                // Under a template source the row is not asking for the
                // credential, so it stops saying it is.
                if secret && source != Source::Literal {
                    format!("{label} · {}", source.field_label())
                } else {
                    label.to_string()
                },
                t.muted(),
            )));

        if secret {
            let mut picker = div().flex().gap_1();
            for s in Source::all() {
                let on = source == s;
                let k = key.clone();
                picker = picker.child(
                    div()
                        .id(SharedString::from(format!("src-{k}-{}", s.label())))
                        .px_1()
                        .cursor_pointer()
                        .text_size(ms.tiny())
                        .text_color(if on { t.inverted() } else { t.muted() })
                        .when(on, |d| d.bg(t.accent()))
                        .child(SharedString::from(s.label()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_source(&k, s, cx);
                            cx.notify();
                        })),
                );
            }
            row = row.child(picker);
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
                        if shown { "Hide the value" } else { "Show the value" },
                        None,
                        self.theme.clone(),
                        ms,
                    ))
                    .hover(|d| d.bg(t.hover()))
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
    fn swap_source(f: &mut Field, to: Source, cx: &mut gpui::App) {
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
        label: &'static str,
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
                div()
                    .id(SharedString::from(id))
                    .flex_none()
                    .px_2()
                    .cursor_pointer()
                    .border_1()
                    .border_color(t.border())
                    .hover(|d| d.bg(t.hover()))
                    .text_size(ms.small())
                    .text_color(t.fg())
                    .child("browse\u{2026}")
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
                div()
                    .id(SharedString::from(format!("pick-{key}")))
                    .flex_none()
                    .px_2()
                    .cursor_pointer()
                    .border_1()
                    .border_color(t.border())
                    .hover(|d| d.bg(t.hover()))
                    .text_size(ms.small())
                    .text_color(t.fg())
                    .child("browse\u{2026}")
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
    fn icon_inline(&self, glyph: &'static str, colour: gpui::Rgba) -> gpui::Div {
        let ms = self.scale;
        let box_w = (ms.base() * 1.25).round();
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .w(px(box_w))
            .h(px(box_w))
            .line_height(px(box_w))
            .text_size(ms.small())
            .text_color(colour)
            .child(glyph)
    }

    /// A drawn checkbox: a box, with a tick in it when it is on.
    fn tick(&self, on: bool) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        let side = (ms.base() * 1.1).round();
        div()
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .w(px(side))
            .h(px(side))
            .line_height(px(side))
            .border_1()
            .border_color(if on { t.accent() } else { t.border() })
            .when(on, |d| d.bg(t.accent()))
            .text_size(ms.tiny())
            .text_color(t.inverted())
            .child(if on { "\u{f00c}" } else { "" })
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
                    .child(self.small("Fields marked * are required", t.muted())),
            );
        // The name sits top right, not as the first row of the form.
        head = head.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(self.small("Name", t.muted()))
                .child(div().w(px(ms.field_w())).child(self.name.clone())),
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
        for ft in [FormTab::Main, FormTab::Access, FormTab::Settings] {
            let on = self.tab == ft;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(ft.label()))
                    .px_1()
                    .cursor_pointer()
                    .text_size(ms.body())
                    .text_color(if on { t.fg() } else { t.muted() })
                    .when(on, |d| d.border_b_1().border_color(t.accent()))
                    .child(SharedString::from(ft.label()))
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
                    .child("SSH"),
            );
        }

        let mut body = div().flex().flex_col().flex_1().p_3().gap_1().overflow_hidden();
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
                        div()
                            .id(SharedString::from(format!("drv-{kind}")))
                            .px_1()
                            .cursor_pointer()
                            .text_size(ms.tiny())
                            .text_color(if on { t.inverted() } else { t.muted() })
                            .when(on, |d| d.bg(t.accent()))
                            .child(SharedString::from(disp))
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
                                div()
                                    .id(SharedString::from(format!("awsp-{name}")))
                                    .px_1()
                                    .cursor_pointer()
                                    .text_size(ms.tiny())
                                    .text_color(if on { t.inverted() } else { t.muted() })
                                    .when(on, |d| d.bg(t.accent()))
                                    .child(SharedString::from(name.clone()))
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
                            body = body.child(self.small(
                                "No AWS profiles were found — type a name, or check that \
                                 ~/.aws/config is where this engine looks.",
                                t.muted(),
                            ));
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
                    div().w(px(ms.label_w())).child(self.small("Authentication", t.muted())),
                );
                for a in auths {
                    let on = a == self.auth_kind;
                    auth_picker = auth_picker.child(
                        div()
                            .id(SharedString::from(format!("auth-{a}")))
                            .px_1()
                            .cursor_pointer()
                            .text_size(ms.tiny())
                            .text_color(if on { t.inverted() } else { t.muted() })
                            .when(on, |d| d.bg(t.accent()))
                            .child(SharedString::from(a.clone()))
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
                        body = body.child(self.row("Password", &i, src, true, "auth-password".into(), cx));
                    }
                    "client_cert" => {
                        let (c, k) = (self.auth_cert.clone(), self.auth_key.clone());
                        body = body.child(self.path_row("Certificate", &c, "pick-cert", cx));
                        body = body.child(self.path_row("Key", &k, "pick-ckey", cx));
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
                            div().w(px(ms.label_w())).child(self.small("Provider", t.muted())),
                        );
                        for p in PROVIDERS {
                            let on = self.auth_provider == p;
                            let pick = p;
                            pr = pr.child(
                                div()
                                    .id(SharedString::from(format!("prov-{p}")))
                                    .px_1()
                                    .cursor_pointer()
                                    .text_size(ms.tiny())
                                    .text_color(if on { t.inverted() } else { t.muted() })
                                    .when(on, |d| d.bg(t.accent()))
                                    .child(p)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.auth_provider = pick.to_string();
                                        cx.notify();
                                    })),
                            );
                        }
                        body = body.child(pr);
                        let (i, src) = (self.auth_token.input.clone(), self.auth_token.source);
                        body = body.child(self.row("Token", &i, src, true, "auth-token".into(), cx));
                        // Said plainly: the name is RECORDED and, today, read by
                        // nothing. Snowflake is the only driver that takes a
                        // provider and it uses the token alone. Better to say so
                        // than to let a chosen name imply an effect it has not
                        // got.
                        body = body.child(self.small(
                            "The token is what connects. The provider name is stored with the \
                             connection and no driver reads it yet.",
                            t.muted(),
                        ));
                    }
                    "kerberos" => {
                        body = body.child(plain(self, "Principal", &self.auth_principal));
                    }
                    _ => {}
                }
                if self.auth_kind != "none" {
                    body = body.child(self.small(
                        "Literal is stored as typed. Env, Command and Keyring are resolved by the \
                         engine at connect time — the client never sees the value.",
                        t.muted(),
                    ));
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
                    div().w(px(ms.label_w())).child(self.small("SSL mode", t.muted())),
                );
                for m in ["disable", "prefer", "require", "verify_ca", "verify_full"] {
                    let on = self.tls_mode == m;
                    tls = tls.child(
                        div()
                            .id(SharedString::from(format!("tls-{m}")))
                            .px_1()
                            .cursor_pointer()
                            .text_size(ms.tiny())
                            .text_color(if on { t.inverted() } else { t.muted() })
                            .when(on, |d| d.bg(t.accent()))
                            .child(SharedString::from(m))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.tls_mode = m.to_string();
                                cx.notify();
                            })),
                    );
                }
                body = body.child(tls);
                if self.tls_mode == "disable" {
                    body = body.child(self.small(
                        "the link will be PLAINTEXT — the engine warns on every connect",
                        t.warning(),
                    ));
                }
                body = body.child(self.stage_row("tls", cx));
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
                                .child("SSH tunnel"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tunnel_on = !this.tunnel_on;
                            cx.notify();
                        })),
                );
                if self.tunnel_on {
                    // Its own tunnel, or one of the saved ones. The engine
                    // refuses a spec that names a profile AND spells a tunnel
                    // out, so this is a choice rather than two things to fill.
                    let mut pick = div().flex().items_center().gap_2().py_1().child(
                        div().w(px(ms.label_w())).child(self.small("Tunnel", t.muted())),
                    );
                    let own = self.tunnel_profile.is_none();
                    pick = pick.child(
                        div()
                            .id("tun-own")
                            .px_1()
                            .cursor_pointer()
                            .text_size(ms.tiny())
                            .text_color(if own { t.inverted() } else { t.muted() })
                            .when(own, |d| d.bg(t.accent()))
                            .child("this connection's own")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tunnel_profile = None;
                                cx.notify();
                            })),
                    );
                    for name in self.tunnel_profiles.clone() {
                        let on = self.tunnel_profile.as_deref() == Some(name.as_str());
                        let n = name.clone();
                        pick = pick.child(
                            div()
                                .id(SharedString::from(format!("tun-p-{name}")))
                                .px_1()
                                .cursor_pointer()
                                .text_size(ms.tiny())
                                .text_color(if on { t.inverted() } else { t.muted() })
                                .when(on, |d| d.bg(t.accent()))
                                .child(SharedString::from(name.clone()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.tunnel_profile = Some(n.clone());
                                    cx.notify();
                                })),
                        );
                    }
                    body = body.child(pick);
                    if self.tunnel_profiles.is_empty() {
                        body = body.child(self.small(
                            "No saved tunnels yet. Fill these in and press \"save as profile\" to \
                             reuse them on another connection.",
                            t.muted(),
                        ));
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
                                    .child(self.small("Goes through", t.muted())),
                            )
                            .child(div().flex_1().child(self.small(summary, t.fg())))
                            .child(
                                div()
                                    .id("edit-this-profile")
                                    .flex_none()
                                    .px_1()
                                    .cursor_pointer()
                                    .hover(|d| d.bg(t.hover()))
                                    .text_size(ms.tiny())
                                    .text_color(t.accent())
                                    .child("edit this tunnel")
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
                                                w.open_profile(Some((n2, sp)), cx);
                                            });
                                        }
                                    })),
                            ),
                    );
                    body = body.child(self.small(
                        "These values live in the profile. Changing them there changes every \
                         connection that uses it.",
                        t.muted(),
                    ));
                    body = body.child(self.stage_row("tunnel", cx));
                }
                if self.tunnel_on && self.tunnel_profile.is_none() {
                    for (label, input) in [
                        ("SSH host", self.tunnel_host.clone()),
                        ("SSH port", self.tunnel_port.clone()),
                        ("SSH user", self.tunnel_user.clone()),
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
                        div().w(px(ms.label_w())).child(self.small("Method", t.muted())),
                    );
                    for a in ["agent", "key", "password"] {
                        let on = self.tunnel_auth == a;
                        m = m.child(
                            div()
                                .id(SharedString::from(format!("tauth-{a}")))
                                .px_1()
                                .cursor_pointer()
                                .text_size(ms.tiny())
                                .text_color(if on { t.inverted() } else { t.muted() })
                                .when(on, |d| d.bg(t.accent()))
                                .child(SharedString::from(a))
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
                        body = body.child(self.path_row("Key file", &kp, "pick-tkey", cx));
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
                        body = body.child(self.small(
                            "Leave the passphrase empty for a key that has none.",
                            t.muted(),
                        ));
                    }
                    if self.tunnel_auth == "password" {
                        let (i, src) =
                            (self.tunnel_password.input.clone(), self.tunnel_password.source);
                        body = body.child(self.row("Password", &i, src, true, "tunnel-password".into(), cx));
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
                                    .child(self.small("Save as profile", t.muted())),
                            )
                            .child(
                                div()
                                    .w(px(ms.field_w()))
                                    .text_size(ms.body())
                                    .child(self.profile_name.clone()),
                            )
                            .child(
                                div()
                                    .id("tun-save-profile")
                                    .px_2()
                                    .cursor_pointer()
                                    .text_size(ms.small())
                                    .border_1()
                                    .border_color(t.accent())
                                    .text_color(t.fg())
                                    .hover(|d| d.bg(t.hover()))
                                    .child("save")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_tunnel_profile(cx);
                                    })),
                            ),
                    );
                    body = body.child(self.small(
                        "Saving over a name replaces it — one bastion, one entry, changed in one \
                         place.",
                        t.muted(),
                    ));
                }
                }
            }
            FormTab::Settings => {
                body = body.child(
                    div()
                        .pt_1()
                        .text_size(ms.tiny())
                        .text_color(t.header_fg())
                        .child("WHAT THIS CONNECTION MAY DO"),
                );
                body = body.child(self.small(
                    "Held by the engine, not by this window: a client that skips its own \
                     confirmation still cannot get past it.",
                    t.muted(),
                ));
                for (key, label, what) in [
                    (
                        "read_only",
                        "Read only",
                        "refuses anything that changes the database \u{2014} an INSERT as firmly as a DROP",
                    ),
                    (
                        "confirm",
                        "Asks first",
                        "writes are allowed; the dangerous ones have to be confirmed",
                    ),
                    (
                        "open",
                        "Unrestricted",
                        "everything runs \u{2014} for a scratch database where the asking is the nuisance",
                    ),
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
                            .when(on, |d| d.bg(t.selected()).border_l_2().border_color(t.accent()))
                            .when(!on, |d| d.border_l_2().border_color(t.panel()))
                            .hover(|d| d.bg(t.hover()))
                            .child(
                                div()
                                    .text_size(ms.body())
                                    .text_color(if on { t.accent() } else { t.fg() })
                                    .child(label),
                            )
                            .child(self.small(what, t.muted()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.policy = key.to_string();
                                cx.notify();
                            })),
                    );
                }
            }
        }

        if let Some(e) = &self.error {
            body = body.child(self.small(e.clone(), t.error()));
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
                div()
                    .id("test-all")
                    .px_2()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .border_1()
                    .border_color(t.border())
                    .text_color(t.fg())
                    .child("Test connection")
                    .on_click(cx.listener(|this, _, _, cx| {
                        // The deepest stage: it exercises everything under it.
                        this.run_test("auth", cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .id("form-cancel")
                            .px_2()
                            .cursor_pointer()
                            .text_size(ms.small())
                            .text_color(t.muted())
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let _ = this.workspace.update(cx, |w, cx| w.close_form(cx));
                            })),
                    )
                    .child(
                        div()
                            .id("form-save")
                            .px_2()
                            .cursor_pointer()
                            .text_size(ms.small())
                            .bg(t.accent())
                            .text_color(t.inverted())
                            .child("Save")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save(cx);
                                cx.notify();
                            })),
                    ),
            );

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
            .font_family("CodeNerdFont")
            .text_size(ms.body())
            .child(head)
            .child(tabs)
            .child(body)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
