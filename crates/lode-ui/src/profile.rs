//! The window for one saved tunnel or one saved proxy.
//!
//! Its own window, and deliberately not the connection window: a profile is a
//! host, a port, a user, a method and a key. Nothing about a driver, nothing
//! about a database. Editing one through a connection form would mean filling in
//! a driver and a database that the profile has no use for, which is why
//! "save over the same name" was never really editing — it was re-creating.
//!
//! ONE window for both kinds, because they are the same window: a name, a host,
//! a port, and how to sign in. A second file would have been this one with the
//! SSH half deleted, and the two would then drift — the save, the rename
//! refusal, the dependants warning, tab order, theming and Escape are the same
//! behaviour whichever kind is being edited, and that is the part worth not
//! writing twice. What differs is a handful of fields, and `kind` decides them.
//!
//! The shapes it reads and writes are the engine's `TunnelSpec` and
//! `ProxySpec`, and the tunnel half is built by the SAME code the connection
//! window uses (`tunnel_fields`). Two builders of one shape is how a passphrase
//! ended up in a key path, and that lesson is not being relearned here.

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, SharedString, WeakEntity, Window};
use serde_json::{json, Value};

use crate::app::Workspace;
use crate::form::{path_problem, pick_file, tunnel_auth_json, Field, Source};
use crate::input::TextInput;
use crate::metrics::Metrics;
use crate::theme::Theme;
use crate::widgets::{btn, chip, Btn};

pub struct ProfileForm {
    workspace: WeakEntity<Workspace>,
    /// `"tunnel"` or `"proxy"` — which shape this window is editing. It is the
    /// engine's own word for the kind, so it goes out on the save untouched.
    kind: &'static str,
    theme: Theme,
    scale: Metrics,
    /// The interface's language — see `ConnectionForm::lang`.
    lang: crate::i18n::Lang,
    /// The main window's text font — see `ConnectionForm::font_family`.
    font_family: String,
    /// The name it was opened under, when it already existed. A new profile has
    /// none, and saving under a name that exists replaces it.
    editing: Option<String>,
    /// Which connections point at it, as the engine answered. Known BEFORE
    /// anything is deleted, which is the whole point of asking.
    users: Vec<String>,
    name: Entity<TextInput>,
    host: Entity<TextInput>,
    port: Entity<TextInput>,
    user: Entity<TextInput>,
    auth: String,
    key_path: Entity<TextInput>,
    passphrase: Field,
    password: Field,
    /// A proxy's protocol: `socks5` or `http_connect`. Unused by a tunnel.
    proxy_kind: String,
    error: Option<String>,
}

impl ProfileForm {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        look: crate::widgets::Look,
        kind: &'static str,
        existing: Option<(String, Value)>,
        users: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let proxy = kind == "proxy";
        let crate::widgets::Look { theme, scale, text_font: font_family, lang, .. } = look;
        let colors = crate::form::colors_of(&theme);
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
            Some((n, s)) => (n.clone(), s.clone()),
            None => (String::new(), json!({})),
        };
        let get = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let auth_v = spec.get("auth").cloned().unwrap_or(json!({}));
        let (pass_src, pass_arg) = Source::unwrap(&get(&auth_v, "passphrase"));
        let (pw_src, pw_arg) = Source::unwrap(&get(&auth_v, "password"));

        // A proxy keeps its sign-in in `auth: { user, password }`; a tunnel
        // keeps the user at the top and the method under `auth`. Read from
        // wherever the kind puts it, so an edit shows what was saved.
        let (px_user, px_pw) = {
            let (u, p) = (get(&auth_v, "user"), get(&auth_v, "password"));
            (u, Source::unwrap(&p))
        };
        let me = ProfileForm {
            workspace,
            kind,
            theme,
            lang,
            font_family,
            scale,
            editing: existing.as_ref().map(|(n, _)| n.clone()),
            users,
            name: mk(cx, if proxy { "name this proxy" } else { "name this tunnel" }, &name_v),
            host: mk(cx, if proxy { "socks.example" } else { "bastion.example" }, &get(&spec, "host")),
            port: mk(
                cx,
                if proxy { "1080" } else { "22" },
                &spec
                    .get("port")
                    .and_then(Value::as_u64)
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
            user: mk(cx, "user", &if proxy { px_user } else { get(&spec, "user") }),
            auth: {
                let k = get(&auth_v, "kind");
                if k.is_empty() { "agent".into() } else { k }
            },
            key_path: mk(cx, "~/.ssh/id_ed25519", &get(&auth_v, "path")),
            passphrase: Field::secret(mk(cx, pass_src.hint(), &pass_arg), pass_src),
            password: if proxy {
                Field::secret(mk(cx, px_pw.0.hint(), &px_pw.1), px_pw.0)
            } else {
                Field::secret(mk(cx, pw_src.hint(), &pw_arg), pw_src)
            },
            proxy_kind: {
                let k = get(&spec, "kind");
                // SOCKS5 by default, because it is the one that tunnels any TCP
                // connection; HTTP CONNECT only does what the proxy allows.
                if k.is_empty() { "socks5".into() } else { k }
            },
            error: None,
        };
        // Enter is the dialog's default action, from any field.
        for field in me.all_inputs() {
            cx.subscribe(&field, |this, _f, ev: &crate::input::InputEvent, cx| {
                if matches!(ev, crate::input::InputEvent::Submit) {
                    this.save(cx);
                }
            })
            .detach();
        }
        me
    }

    /// The default port for the protocol in front of somebody.
    ///
    /// Only used when the box is EMPTY. A proxy's port is not optional in the
    /// engine's shape, and 1080 and 8080 are what the two protocols are reached
    /// on when nobody says otherwise.
    fn default_port(&self) -> u16 {
        match (self.kind, self.proxy_kind.as_str()) {
            ("proxy", "http_connect") => 8080,
            ("proxy", _) => 1080,
            _ => 22,
        }
    }

    /// The `TunnelSpec` or `ProxySpec` this window describes.
    fn spec(&self, cx: &Context<Self>) -> Value {
        if self.kind == "proxy" {
            let user = self.user.read(cx).text().trim().to_string();
            let pw = self.password.source.wrap(self.password.input.read(cx).text());
            return json!({
                "kind": self.proxy_kind,
                "host": self.host.read(cx).text().trim(),
                "port": crate::workspace::parse_port(
                    &self.port.read(cx).text(),
                    self.default_port(),
                )
                .unwrap_or_else(|_| self.default_port()),
                // ABSENT, not an empty pair: the engine's `auth` is an Option,
                // and a proxy with a blank user and a blank password is a proxy
                // with no sign-in, not one whose sign-in is two empty strings.
                "auth": if user.is_empty() && pw.is_empty() {
                    Value::Null
                } else {
                    json!({ "user": user, "password": pw })
                },
            });
        }
        json!({
            "host": self.host.read(cx).text(),
            // Checked by `port_problem` before a save gets this far; the
            // fallback here is only so `spec` stays infallible.
            "port": crate::workspace::parse_port(&self.port.read(cx).text(), 22).unwrap_or(22),
            "user": self.user.read(cx).text(),
            "auth": tunnel_auth_json(
                &self.auth,
                self.key_path.read(cx).text(),
                &self.passphrase.source.wrap(self.passphrase.input.read(cx).text()),
                &self.password.source.wrap(self.password.input.read(cx).text()),
            ),
        })
    }

    /// A SECRET row: where the value comes from, the value, and an eye.
    ///
    /// The same three parts the connection window has, because this window
    /// already stores the same shape — it reads a `{{ env … }}` back through
    /// `Source::unwrap` and writes one through `source.wrap`. What it did not
    /// do was SHOW any of it: a passphrase typed here was drawn in clear, with
    /// no way to say it lives in the environment or in the keyring. A window
    /// that writes a thing and hides it is a window that lies about what it
    /// saved.
    fn secret_row(
        &self,
        label: SharedString,
        key: &'static str,
        f: &Field,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let t = self.theme.clone();
        let ms = self.scale;
        let mut row = div().flex().items_center().gap_2().py(px(2.0)).child(
            div()
                .w(px(ms.label_w()))
                .flex_none()
                .text_size(ms.small())
                .text_color(t.muted())
                .child(label),
        );
        for src in Source::all() {
            let on = f.source == src;
            row = row.child(chip(&t, ms, format!("psrc-{key}-{}", src.label()), src.label(), on)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_source(key, src, cx);
                    cx.notify();
                })));
        }
        // Dots only under `Literal`. Under the other three the field holds the
        // NAME of a variable, a command line or a keyring path — public text
        // somebody has to be able to read and check, and masking it hides
        // nothing while making it unusable.
        let hide = f.source == Source::Literal;
        let shown = f.input.read(cx).revealed();
        f.input.update(cx, |i, cx| i.set_mask(hide, cx));
        row = row.child(div().flex_1().text_size(ms.body()).child(f.input.clone()));
        if hide {
            let input = f.input.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("peye-{key}")))
                    .flex_none()
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover_mark()))
                    // Open eye while it is showing, struck-through while it is
                    // not: the icon says what you are looking at, not what
                    // pressing it would do.
                    .child(
                        div()
                            .font_family(SharedString::from(self.font_family.clone()))
                            .text_size(ms.small())
                            .text_color(if shown { t.warning() } else { t.muted() })
                            .child(if shown { "\u{f06e}" } else { "\u{f070}" }),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        input.update(cx, |i, cx| i.toggle_reveal(cx));
                        cx.notify();
                    })),
            );
        }
        row
    }

    /// Move one of the two secrets to a different source, keeping what was
    /// typed under the old one — the same swap the connection window makes,
    /// from the same function.
    fn set_source(&mut self, key: &str, to: Source, cx: &mut gpui::App) {
        let f = match key {
            "passphrase" => &mut self.passphrase,
            "password" => &mut self.password,
            _ => return,
        };
        crate::form::ConnectionForm::swap_source(f, to, cx);
    }

    /// Close this window, from INSIDE it.
    ///
    /// `window.remove_window()` on its own window, and then the workspace is
    /// told to drop the handle. Asking the workspace to close it instead means
    /// `WindowHandle::update` on the window that is already being updated —
    /// the re-entrant update gpui refuses — and the `let _ =` on the result
    /// meant Cancel and Escape silently did nothing at all.
    fn leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |w, cx| w.forget_profile(cx));
        window.remove_window();
    }

    /// Tab and shift-tab between the fields — see `ConnectionForm::step_focus`.
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

    /// What is wrong with the typed port, if anything.
    fn port_problem(&self, cx: &Context<Self>) -> Option<String> {
        crate::workspace::parse_port(&self.port.read(cx).text(), self.default_port()).err()
    }

    fn key_problem(&self, cx: &Context<Self>) -> Option<String> {
        // A proxy has no SSH key — the question does not arise.
        if self.kind == "proxy" || self.auth != "key" {
            return None;
        }
        let raw = self.key_path.read(cx).text();
        let p = raw.trim();
        if p.is_empty() {
            return Some("a key file is needed, or choose another method".into());
        }
        path_problem(p)
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.error = Some("a profile needs a name".into());
            cx.notify();
            return;
        }
        if let Some(why) = self.port_problem(cx) {
            self.error = Some(why);
            cx.notify();
            return;
        }
        if let Some(why) = self.key_problem(cx) {
            self.error = Some(format!("SSH key: {why}"));
            cx.notify();
            return;
        }
        // A RENAME would orphan every connection pointing at the old name — the
        // engine resolves a profile by name and a name that is not there fails
        // the connect. Silently leaving them broken is the worst of the three
        // options, so it is refused with the list and the two ways out.
        if let Some(old) = self.editing.clone() {
            if old != name && !self.users.is_empty() {
                self.error = Some(format!(
                    "{} connection(s) point at \"{old}\": {}. Renaming would leave them pointing \
                     at a profile that is not there. Save under \"{old}\" to change its values, \
                     or save under \"{name}\" as a NEW profile and move those connections to it.",
                    self.users.len(),
                    self.users.join(", ")
                ));
                cx.notify();
                return;
            }
        }
        // A proxy with no HOST is a proxy that resolves to nothing, and the
        // engine would refuse it at connect time with the connection's name in
        // the error instead of the profile's.
        if self.host.read(cx).text().trim().is_empty() {
            self.error = Some(format!("a {} needs a host", self.kind));
            cx.notify();
            return;
        }
        let spec = self.spec(cx);
        let kind = self.kind;
        let _ = self.workspace.update(cx, |w, cx| {
            w.save_profile(kind, name, spec);
            w.close_profile(cx);
        });
    }
}

impl ProfileForm {
    /// Every live field in this window.
    pub fn all_inputs(&self) -> Vec<Entity<TextInput>> {
        vec![
            self.name.clone(),
            self.host.clone(),
            self.port.clone(),
            self.user.clone(),
            self.key_path.clone(),
            self.passphrase.input.clone(),
            self.password.input.clone(),
        ]
    }

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

impl Render for ProfileForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme.clone();
        let ms = self.scale;
        let small = |s: SharedString, c: gpui::Rgba| {
            div().text_size(ms.small()).text_color(c).child(s)
        };
        let row = |label: SharedString, input: Entity<TextInput>| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .py(px(2.0))
                .child(
                    div()
                        .w(px(ms.label_w()))
                        .flex_none()
                        .text_size(ms.small())
                        .text_color(t.muted())
                        .child(label),
                )
                .child(div().flex_1().text_size(ms.body()).child(input))
        };

        // Scrolls, for the same reason the connection window's body does: with
        // a key path, a passphrase and a warning the content is taller than the
        // window at a large text size, and what fell off the bottom was Save.
        let mut body = crate::widgets::scroll("profile-body")
            .outer(|d| d.p_3())
            .rows(|d| d.gap_1());
        body = body.child(row(self.tr("form.name"), self.name.clone()));

        if self.kind == "proxy" {
            let mut kinds = div().flex().items_center().gap_2().py_1().child(
                div()
                    .w(px(ms.label_w()))
                    .flex_none()
                    .text_size(ms.small())
                    .text_color(t.muted())
                    .child(self.tr("form.proxy_kind")),
            );
            for (k, label) in [("socks5", "SOCKS5"), ("http_connect", "HTTP CONNECT")] {
                let on = self.proxy_kind == k;
                kinds = kinds.child(chip(&t, ms, format!("pk-{k}"), label, on).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.proxy_kind = k.to_string();
                        cx.notify();
                    }),
                ));
            }
            body = body
                .child(kinds)
                .child(row(self.tr("form.proxy_host"), self.host.clone()))
                .child(row(self.tr("form.proxy_port"), self.port.clone()));
            if let Some(why) = self.port_problem(cx) {
                body = body.child(small(why.into(), t.error()));
            }
            // The sign-in is OPTIONAL, and saying so is the point: an empty
            // pair here means no sign-in, not a sign-in with empty values.
            body = body
                .child(small(self.tr("form.proxy_auth_opt"), t.muted()))
                .child(row(self.tr("form.ssh_user"), self.user.clone()))
                .child(self.secret_row(self.tr("form.password"), "password", &self.password, cx));
            if !self.users.is_empty() {
                body = body.child(small(
                    format!(
                        "{} connection(s) use this profile: {}",
                        self.users.len(),
                        self.users.join(", ")
                    )
                    .into(),
                    t.muted(),
                ));
            }
            if let Some(e) = &self.error {
                body = body.child(small(e.clone().into(), t.error()));
            }
            return self.frame(body, cx);
        }

        body = body
            .child(row(self.tr("form.ssh_host"), self.host.clone()))
            .child(row(self.tr("form.ssh_port"), self.port.clone()))
            .child(row(self.tr("form.ssh_user"), self.user.clone()));

        let mut methods = div().flex().items_center().gap_2().py_1().child(
            div()
                .w(px(ms.label_w()))
                .flex_none()
                .text_size(ms.small())
                .text_color(t.muted())
                .child(self.tr("form.method")),
        );
        for a in ["agent", "key", "password"] {
            let on = self.auth == a;
            methods = methods.child(
                chip(&t, ms, format!("pauth-{a}"), a, on).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.auth = a.to_string();
                        cx.notify();
                    },
                )),
            );
        }
        body = body.child(methods);

        if self.auth == "key" {
            // The same system chooser the connection window offers, and the
            // same check on what it returns.
            let target = self.key_path.clone();
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py(px(2.0))
                    .child(
                        div()
                            .w(px(ms.label_w()))
                            .flex_none()
                            .text_size(ms.small())
                            .text_color(t.muted())
                            .child(self.tr("form.key_file")),
                    )
                    .child(div().flex_1().text_size(ms.body()).child(self.key_path.clone()))
                    .child(
                        btn(&t, ms, "pick-profile-key", "browse\u{2026}", Btn::Secondary)
                            .on_click(cx.listener(move |_this, _, _, cx| {
                                let me = cx.entity().downgrade();
                                pick_file(
                                    target.clone(),
                                    move |why, cx| {
                                        let _ = me.update(cx, |f: &mut ProfileForm, cx| {
                                            f.error = Some(why);
                                            cx.notify();
                                        });
                                    },
                                    cx,
                                );
                                cx.notify();
                            })),
                    ),
            );
            if let Some(why) = self.key_problem(cx) {
                body = body.child(small(why.into(), t.error()));
            }
            body = body.child(self.secret_row(
                self.tr("form.passphrase"),
                "passphrase",
                &self.passphrase,
                cx,
            ));
        }
        if self.auth == "password" {
            body = body.child(self.secret_row(
                self.tr("form.password"),
                "password",
                &self.password,
                cx,
            ));
        }

        // WHO depends on this, before anything is changed rather than after.
        if !self.users.is_empty() {
            body = body.child(small(
                format!(
                    "{} connection(s) use this profile: {}",
                    self.users.len(),
                    self.users.join(", ")
                )
                .into(),
                t.muted(),
            ));
        }
        if let Some(e) = &self.error {
            body = body.child(small(e.clone().into(), t.error()));
        }

        self.frame(body, cx)
    }
}

impl ProfileForm {
    /// The title, the buttons and the key handling — the same for both kinds,
    /// which is why they are in one place rather than in each branch.
    fn frame(&self, body: crate::widgets::Scroll, cx: &mut Context<Self>) -> gpui::Div {
        let t = self.theme.clone();
        let ms = self.scale;
        let head = div()
            .flex()
            .items_center()
            .w_full()
            .px_3()
            .py_2()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border())
            .text_color(t.fg())
            .child(SharedString::from(match &self.editing {
                Some(n) => format!("Edit the {} \"{n}\"", self.kind),
                None => format!("New {}", self.kind),
            }));

        // Cancel and Save to the RIGHT, Save last — the arrangement every
        // dialog has, and the one the connection window already uses. These
        // were at the left edge, which puts the thing you press most where the
        // eye leaves rather than where it arrives.
        let footer = div()
            .flex()
            .items_center()
            .justify_end()
            .gap_2()
            .w_full()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(t.border())
            .child(
                btn(&t, ms, "profile-cancel", "Cancel", Btn::Quiet).on_click(cx.listener(
                    |this, _, window: &mut Window, cx| this.leave(window, cx),
                )),
            )
            .child(
                btn(&t, ms, "profile-save", "Save", Btn::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            );

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
            // The main window's type. Without this a separate window takes the
            // platform default, so this one was in a different face from the
            // window that opened it.
            .font_family(SharedString::from(self.font_family.clone()))
            .text_size(ms.body())
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
            .child(body)
            .child(footer)
    }
}
