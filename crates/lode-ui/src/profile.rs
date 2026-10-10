//! The window for one saved tunnel.
//!
//! Its own window, and deliberately not the connection window: a profile is a
//! host, a port, a user, a method and a key. Nothing about a driver, nothing
//! about a database. Editing one through a connection form would mean filling in
//! a driver and a database that the profile has no use for, which is why
//! "save over the same name" was never really editing — it was re-creating.
//!
//! The shape it reads and writes is the engine's `TunnelSpec`, built by the
//! SAME code the connection window uses (`tunnel_fields`). Two builders of one
//! shape is how a passphrase ended up in a key path, and that lesson is not
//! being relearned here.

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
    error: Option<String>,
}

impl ProfileForm {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        look: crate::widgets::Look,
        existing: Option<(String, Value)>,
        users: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Self {
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

        ProfileForm {
            workspace,
            theme,
            lang,
            font_family,
            scale,
            editing: existing.as_ref().map(|(n, _)| n.clone()),
            users,
            name: mk(cx, "name this tunnel", &name_v),
            host: mk(cx, "bastion.example", &get(&spec, "host")),
            port: mk(
                cx,
                "22",
                &spec
                    .get("port")
                    .and_then(Value::as_u64)
                    .map(|p| p.to_string())
                    .unwrap_or_default(),
            ),
            user: mk(cx, "user", &get(&spec, "user")),
            auth: {
                let k = get(&auth_v, "kind");
                if k.is_empty() { "agent".into() } else { k }
            },
            key_path: mk(cx, "~/.ssh/id_ed25519", &get(&auth_v, "path")),
            passphrase: Field::secret(mk(cx, pass_src.hint(), &pass_arg), pass_src),
            password: Field::secret(mk(cx, pw_src.hint(), &pw_arg), pw_src),
            error: None,
        }
    }

    /// The `TunnelSpec` this window describes.
    fn spec(&self, cx: &Context<Self>) -> Value {
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

    /// One phrase of the interface, in the chosen language.
    fn tr(&self, key: &str) -> SharedString {
        SharedString::from(self.lang.tr(key))
    }

    /// What is wrong with the typed port, if anything.
    fn port_problem(&self, cx: &Context<Self>) -> Option<String> {
        crate::workspace::parse_port(&self.port.read(cx).text(), 22).err()
    }

    fn key_problem(&self, cx: &Context<Self>) -> Option<String> {
        if self.auth != "key" {
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
        let spec = self.spec(cx);
        let _ = self.workspace.update(cx, |w, cx| {
            w.save_profile("tunnel", name, spec);
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
        let mut body = div()
            .id("profile-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_3()
            .gap_1();
        body = body
            .child(row(self.tr("form.name"), self.name.clone()))
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
            body = body.child(row(self.tr("form.passphrase"), self.passphrase.input.clone()));
        }
        if self.auth == "password" {
            body = body.child(row(self.tr("form.password"), self.password.input.clone()));
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
                Some(n) => format!("Edit the tunnel \"{n}\""),
                None => "New tunnel".to_string(),
            }));

        let footer = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(t.border())
            .child(
                btn(&t, ms, "profile-save", "Save", Btn::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
            .child(
                btn(&t, ms, "profile-cancel", "Cancel", Btn::Quiet).on_click(cx.listener(
                    |this, _, _, cx| {
                        let _ = this.workspace.update(cx, |w, cx| w.close_profile(cx));
                    },
                )),
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
            .child(head)
            .child(body)
            .child(footer)
    }
}
