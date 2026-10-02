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
use crate::theme::Theme;

/// Where a value comes from. The engine's `secret.rs` resolves exactly these,
/// which is why the list is this and not a wish.
#[derive(Clone, Copy, PartialEq, Eq)]
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
struct Field {
    input: Entity<TextInput>,
    source: Source,
    /// Whether a source may be chosen at all. A host is a host; a password is
    /// the kind of thing that comes from a wallet.
    secret: bool,
}

pub struct ConnectionForm {
    workspace: WeakEntity<Workspace>,
    theme: Theme,
    tab: FormTab,
    editing: Option<String>,
    driver: String,
    drivers: Vec<DriverMeta>,

    name: Entity<TextInput>,
    /// Driver parameters, by key.
    params: HashMap<String, Field>,
    auth_kind: String,
    auth_user: Entity<TextInput>,
    auth_secret: Field,
    tls_mode: String,
    /// "read_only" | "confirm" | "open" — what the engine will let this
    /// connection do. It belongs on the connection, not on a preference, so it
    /// travels with the connection wherever it is opened from.
    policy: String,
    tunnel_on: bool,
    tunnel_host: Entity<TextInput>,
    tunnel_port: Entity<TextInput>,
    tunnel_user: Entity<TextInput>,
    tunnel_auth: String,
    tunnel_secret: Field,

    /// The last answer from each `conn.test` stage.
    tests: HashMap<&'static str, Result<String, String>>,
    error: Option<String>,
}

impl ConnectionForm {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        theme: Theme,
        drivers: Vec<DriverMeta>,
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

        let stored_secret = {
            let p = get(&auth, "password");
            if p.is_empty() {
                get(&auth, "token")
            } else {
                p
            }
        };
        let (secret_src, secret_arg) = Source::unwrap(&stored_secret);
        let tunnel_stored = if tunnel.is_null() {
            String::new()
        } else {
            let a = tunnel.get("auth").cloned().unwrap_or(json!({}));
            let p = get(&a, "passphrase");
            if p.is_empty() {
                get(&a, "password")
            } else {
                p
            }
        };
        let (tsrc, targ) = Source::unwrap(&tunnel_stored);

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
            auth_secret: Field {
                input: mk(cx, "", &secret_arg),
                source: secret_src,
                secret: true,
            },
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
            tunnel_on: !tunnel.is_null(),
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
            tunnel_secret: Field {
                input: mk(cx, "", &targ),
                source: tsrc,
                secret: true,
            },
            tests: HashMap::new(),
            error: None,
            theme,
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
                Field { input, source: src, secret: p.secret },
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
        let user = self.auth_user.read(cx).text().to_string();
        let secret = self.auth_secret.source.wrap(self.auth_secret.input.read(cx).text());
        let auth = match self.auth_kind.as_str() {
            "password" => json!({ "kind": "password", "user": user, "password": secret }),
            "provider" => json!({ "kind": "provider", "provider": "token", "user": user, "token": secret }),
            "client_cert" => json!({ "kind": "client_cert", "cert": "", "key": "", "key_password": secret, "user": user }),
            "kerberos" => json!({ "kind": "kerberos", "principal": Value::Null }),
            _ => json!({ "kind": "none" }),
        };
        let tunnel = if self.tunnel_on {
            let tsecret = self.tunnel_secret.source.wrap(self.tunnel_secret.input.read(cx).text());
            let tauth = match self.tunnel_auth.as_str() {
                "key" => json!({ "kind": "key", "path": self.tunnel_secret.input.read(cx).text(), "passphrase": "" }),
                "password" => json!({ "kind": "password", "password": tsecret }),
                _ => json!({ "kind": "agent" }),
            };
            json!({
                "host": self.tunnel_host.read(cx).text(),
                "port": self.tunnel_port.read(cx).text().trim().parse::<u16>().unwrap_or(22),
                "user": self.tunnel_user.read(cx).text(),
                "auth": tauth
            })
        } else {
            Value::Null
        };
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

    fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.error = Some("a connection needs a name".into());
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

fn colors_of(t: &Theme) -> InputColors {
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
        div().text_size(px(11.0)).text_color(c).child(text.into())
    }

    /// A labelled row: label, source chooser where one applies, field.
    fn row(
        &self,
        label: &str,
        field: &Field,
        key: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = &self.theme;
        let mut row = div()
            .flex()
            .items_center()
            .gap_2()
            .py(px(2.0))
            .child(div().w(px(120.0)).child(self.small(label.to_string(), t.muted())));

        if field.secret {
            let mut picker = div().flex().gap_1();
            for s in Source::all() {
                let on = field.source == s;
                let k = key.clone();
                picker = picker.child(
                    div()
                        .id(SharedString::from(format!("src-{k}-{}", s.label())))
                        .px_1()
                        .cursor_pointer()
                        .text_size(px(10.0))
                        .text_color(if on { t.inverted() } else { t.muted() })
                        .when(on, |d| d.bg(t.accent()))
                        .child(SharedString::from(s.label()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_source(&k, s);
                            cx.notify();
                        })),
                );
            }
            row = row.child(picker);
        }
        row.child(div().flex_1().child(field.input.clone()))
    }

    fn set_source(&mut self, key: &str, s: Source) {
        if key == "auth-secret" {
            self.auth_secret.source = s;
        } else if key == "tunnel-secret" {
            self.tunnel_secret.source = s;
        } else if let Some(f) = self.params.get_mut(key) {
            f.source = s;
        }
    }

    fn stage_row(&self, stage: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let t = &self.theme;
        let mut row = div().flex().items_center().gap_2().py_1().child(
            div()
                .id(SharedString::from(format!("test-{stage}")))
                .px_2()
                .cursor_pointer()
                .text_size(px(11.0))
                .border_1()
                .border_color(t.border())
                .text_color(t.fg())
                .child(SharedString::from(format!("Test {stage}")))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_test(stage, cx);
                    cx.notify();
                })),
        );
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
                .child(div().w(px(220.0)).child(self.name.clone())),
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
                    .text_size(px(12.0))
                    .text_color(if on { t.fg() } else { t.muted() })
                    .when(on, |d| d.border_b_1().border_color(t.accent()))
                    .child(SharedString::from(ft.label()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = ft;
                        cx.notify();
                    })),
            );
        }
        // The tunnel is worth seeing from the outside of its tab.
        if self.tunnel_on {
            tabs = tabs.child(self.small("SSH", t.accent()));
        }

        let mut body = div().flex().flex_col().flex_1().p_3().gap_1().overflow_hidden();
        match self.tab {
            FormTab::Main => {
                let keys: Vec<(String, String)> = self
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
                            .text_size(px(10.0))
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

                for (key, label) in keys {
                    if let Some(f) = self.params.get(&key) {
                        let f = Field { input: f.input.clone(), source: f.source, secret: f.secret };
                        body = body.child(self.row(&label, &f, SharedString::from(key.clone()), cx));
                    }
                }

                let auths: Vec<String> = self.meta().map(|m| m.auth.clone()).unwrap_or_default();
                let mut auth_picker = div().flex().items_center().gap_2().py_1().child(
                    div().w(px(120.0)).child(self.small("Authentication", t.muted())),
                );
                for a in auths {
                    let on = a == self.auth_kind;
                    auth_picker = auth_picker.child(
                        div()
                            .id(SharedString::from(format!("auth-{a}")))
                            .px_1()
                            .cursor_pointer()
                            .text_size(px(10.0))
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
                if self.auth_kind != "none" {
                    body = body.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py(px(2.0))
                            .child(div().w(px(120.0)).child(self.small("User", t.muted())))
                            .child(div().flex_1().child(self.auth_user.clone())),
                    );
                    let f = Field {
                        input: self.auth_secret.input.clone(),
                        source: self.auth_secret.source,
                        secret: true,
                    };
                    body = body.child(self.row("Secret", &f, "auth-secret".into(), cx));
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
                let mut tls = div().flex().items_center().gap_2().py_1().child(
                    div().w(px(120.0)).child(self.small("SSL mode", t.muted())),
                );
                for m in ["disable", "prefer", "require", "verify_ca", "verify_full"] {
                    let on = self.tls_mode == m;
                    tls = tls.child(
                        div()
                            .id(SharedString::from(format!("tls-{m}")))
                            .px_1()
                            .cursor_pointer()
                            .text_size(px(10.0))
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

                body = body.child(
                    div()
                        .id("tunnel-toggle")
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .cursor_pointer()
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(if self.tunnel_on { t.accent() } else { t.muted() })
                                .child(SharedString::from(if self.tunnel_on {
                                    "[x] SSH tunnel"
                                } else {
                                    "[ ] SSH tunnel"
                                })),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.tunnel_on = !this.tunnel_on;
                            cx.notify();
                        })),
                );
                if self.tunnel_on {
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
                                .child(div().w(px(120.0)).child(self.small(label, t.muted())))
                                .child(div().flex_1().child(input)),
                        );
                    }
                    let mut m = div().flex().items_center().gap_2().py_1().child(
                        div().w(px(120.0)).child(self.small("Method", t.muted())),
                    );
                    for a in ["agent", "key", "password"] {
                        let on = self.tunnel_auth == a;
                        m = m.child(
                            div()
                                .id(SharedString::from(format!("tauth-{a}")))
                                .px_1()
                                .cursor_pointer()
                                .text_size(px(10.0))
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
                    if self.tunnel_auth != "agent" {
                        let f = Field {
                            input: self.tunnel_secret.input.clone(),
                            source: self.tunnel_secret.source,
                            secret: true,
                        };
                        let label = if self.tunnel_auth == "key" { "Key file" } else { "Password" };
                        body = body.child(self.row(label, &f, "tunnel-secret".into(), cx));
                    }
                    body = body.child(self.stage_row("tunnel", cx));
                }
            }
            FormTab::Settings => {
                body = body.child(
                    div()
                        .pt_1()
                        .text_size(px(10.0))
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
                                    .text_size(px(12.0))
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
                    .text_size(px(11.0))
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
                            .text_size(px(11.0))
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
                            .text_size(px(11.0))
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
            .text_size(px(12.0))
            .child(head)
            .child(tabs)
            .child(body)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
