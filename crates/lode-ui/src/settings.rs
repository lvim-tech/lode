//! The settings WINDOW: categories down the left, their content on the right.
//!
//! It is a window and not a panel, and that is the lesson of a long day. A
//! panel is a narrow column, and almost everything that belongs in settings is
//! a LIST beside the DETAIL of whatever is selected in it — tunnels, proxies,
//! hooks, drivers, MCP policies. A column cannot hold that shape, and every
//! attempt to make it (capped boxes, inner scrollers, folding) was working
//! around the column rather than fixing it.
//!
//! The other reason is plainer. The engine has grown tunnel profiles, proxy
//! profiles, access providers, connection hooks, external drivers over RPC and
//! an MCP server with roles and approvals — and none of it has a face. They
//! were built and are unreachable. This window is where they get one.

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, SharedString, WeakEntity, Window};

use crate::app::Workspace;
use crate::input::TextInput;
use crate::metrics::Metrics;
use crate::theme::Theme;
use crate::widgets::{btn, Btn};

/// What the window shows of the MAIN window's state.
///
/// Read fresh on every paint from the workspace rather than copied in at open
/// time. A copy is a second truth: the text size can be changed from the
/// keyboard while this window is open, and a stored number would then sit
/// beside a stepper that moves a different one.
pub struct Appearance {
    pub theme_name: String,
    pub themes: Vec<String>,
    pub theme_dirs: Vec<String>,
    pub base: f32,
    pub icon: f32,
    pub lang: crate::i18n::Lang,
    pub json_indent: u8,
    pub font_family: String,
    pub icon_family: String,
}

/// One page of the window.
///
/// Grouped the way somebody looks for a thing rather than the way the code is
/// arranged: what the app looks like, what it does on the way in and out of a
/// connection, what reaches the network, and what an agent is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    General,
    Keys,
    Audit,
    Tunnels,
    Proxies,
    Access,
    Hooks,
    Drivers,
    Rpc,
    Mcp,
    About,
}

impl Category {
    pub fn all() -> [Category; 11] {
        use Category::*;
        [General, Keys, Audit, Tunnels, Proxies, Access, Hooks, Drivers, Rpc, Mcp, About]
    }

    /// The heading above a run of categories, when this one starts a new run.
    pub fn section(self) -> Option<&'static str> {
        match self {
            Category::General => Some("GENERAL"),
            Category::Tunnels => Some("NETWORK"),
            Category::Hooks => Some("CONNECTIONS"),
            Category::Mcp => Some("MCP GOVERNANCE"),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Category::General => "General",
            Category::Keys => "Keybindings",
            Category::Audit => "Audit",
            Category::Tunnels => "SSH tunnels",
            Category::Proxies => "Proxies",
            Category::Access => "Auth profiles",
            Category::Hooks => "Hooks",
            Category::Drivers => "Drivers",
            Category::Rpc => "RPC services",
            Category::Mcp => "MCP policies",
            Category::About => "About",
        }
    }

    /// One sentence saying what this page is for, shown above its content.
    pub fn blurb(self) -> &'static str {
        match self {
            Category::General => "The theme, the sizes, the language and what the window remembers.",
            Category::Keys => "What the keyboard does.",
            Category::Audit => "What the engine records, and how much of it.",
            Category::Tunnels => {
                "Reusable bastion and jump-host tunnels. Connections pick one in their Access tab."
            }
            Category::Proxies => "HTTP and SOCKS proxies. Connections pick one in their Access tab.",
            Category::Access => "Reusable sign-ins for secrets, parameters and SSO.",
            Category::Hooks => "Commands, scripts and Lua that run around connect and disconnect.",
            Category::Drivers => "What each driver can do, and the settings it declares.",
            Category::Rpc => {
                "External drivers the engine starts over RPC. Changes apply after a restart."
            }
            Category::Mcp => "What each role may run through MCP, per execution class.",
            Category::About => "Versions, paths and what is loaded.",
        }
    }

    /// The words a search should match, beyond the title — so looking for
    /// "passphrase" finds the tunnels page without it being spelled out there.
    pub fn keywords(self) -> &'static str {
        match self {
            Category::General => "theme colours density language font size icon json indent session",
            Category::Keys => "keyboard shortcut binding palette",
            Category::Audit => "log statements redaction retention history",
            Category::Tunnels => "ssh bastion jump host key passphrase agent keyring port",
            Category::Proxies => "http socks5 connect bypass cidr",
            Category::Access => "aws sso provider credentials profile token",
            Category::Hooks => "command script lua pre post connect disconnect environment timeout",
            Category::Drivers => "capabilities caps sql tls tunnel edit metrics overrides",
            Category::Rpc => "external driver socket command stdio",
            Category::Mcp => "agent role approval policy class destructive admin",
            Category::About => "version protocol paths build",
        }
    }
}

pub struct SettingsWindow {
    workspace: WeakEntity<Workspace>,
    theme: Theme,
    scale: Metrics,
    category: Category,
    search: Entity<TextInput>,
    /// What the workspace knew when this opened: driver metadata and profiles.
    /// Handed over rather than fetched, so the window has no engine of its own.
    drivers: Vec<lode_proto::DriverMeta>,
    tunnels: Vec<crate::app::ProfileRow>,
    proxies: Vec<crate::app::ProfileRow>,
    /// Which profile a remove has been ASKED about. The question stands on the
    /// row, before anything happens — not in a toast afterwards.
    confirm_rm: Option<(&'static str, String)>,
    /// The theme filter.
    ///
    /// In the HEADER, never in the page. A `TextInput` shapes and measures its
    /// own line and installs an input handler while painting, and inside a
    /// scrolling list it takes the list down with it — which is the lesson the
    /// rail's settings panel paid for twice.
    theme_filter: Entity<TextInput>,
    /// The two fonts, so every window is in the same type as the main one.
    font_family: String,
    icon_family: String,
    /// Which driver's detail is shown on the Drivers page.
    driver_shown: Option<String>,
    about: Vec<(String, String)>,
}

impl SettingsWindow {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        look: crate::widgets::Look,
        drivers: Vec<lode_proto::DriverMeta>,
        tunnels: Vec<crate::app::ProfileRow>,
        proxies: Vec<crate::app::ProfileRow>,
        about: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let crate::widgets::Look { theme, scale, text_font, icon_font, lang } = look;
        let colors = crate::form::colors_of(&theme);
        let search = cx.new(|cx| TextInput::new(cx, "Search settings…", colors));
        let theme_filter = cx.new(|cx| TextInput::new(cx, lang.tr("settings.filter_themes"), colors));
        let driver_shown = drivers.first().map(|d| d.kind.clone());
        SettingsWindow {
            workspace,
            theme,
            scale,
            category: Category::General,
            search,
            drivers,
            tunnels,
            proxies,
            confirm_rm: None,
            theme_filter,
            font_family: text_font,
            icon_family: icon_font,
            driver_shown,
            about,
        }
    }

    pub fn all_inputs(&self) -> Vec<Entity<TextInput>> {
        vec![self.search.clone(), self.theme_filter.clone()]
    }

    /// The sizes changed under us — see `Workspace::poke_settings`.
    pub fn set_scale(&mut self, scale: Metrics, cx: &mut Context<Self>) {
        self.scale = scale;
        cx.notify();
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        let colors = crate::form::colors_of(&self.theme);
        for i in self.all_inputs() {
            i.update(cx, |x, cx| x.set_colors(colors, cx));
        }
        cx.notify();
    }

    pub fn profiles_arrived(
        &mut self,
        tunnels: Vec<crate::app::ProfileRow>,
        proxies: Vec<crate::app::ProfileRow>,
        cx: &mut Context<Self>,
    ) {
        self.tunnels = tunnels;
        self.proxies = proxies;
        cx.notify();
    }

    /// Which categories match what is typed in the search box.
    ///
    /// The point of the box is that a long page stops being a problem: somebody
    /// who knows the word "passphrase" does not have to know it lives under
    /// SSH tunnels.
    pub fn matching(&self, needle: &str) -> Vec<Category> {
        let n = needle.trim().to_lowercase();
        Category::all()
            .into_iter()
            .filter(|c| {
                n.is_empty()
                    || c.title().to_lowercase().contains(&n)
                    || c.blurb().to_lowercase().contains(&n)
                    || c.keywords().contains(&n)
            })
            .collect()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.theme.clone();
        let ms = self.scale;
        let needle = self.search.read(cx).text().to_string();
        let shown = self.matching(&needle);

        // ── the categories, down the left ───────────────────────────────────
        let mut side = div()
            .id("cat-list")
            .flex()
            .flex_col()
            .w(px(ms.base() * 15.0))
            .flex_none()
            .h_full()
            .min_h_0()
            .overflow_y_scroll()
            .bg(t.panel())
            .border_r_1()
            .border_color(t.border());
        for c in Category::all() {
            if !shown.contains(&c) {
                continue;
            }
            if let Some(sec) = c.section() {
                side = side.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_size(ms.tiny())
                        .text_color(t.title())
                        .child(sec),
                );
            }
            let on = self.category == c;
            side = side.child(
                div()
                    .id(SharedString::from(c.title()))
                    .w_full()
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .text_size(ms.small())
                    .text_color(if on { t.fg() } else { t.muted() })
                    .when(on, |d| d.bg(t.selected_mark()).border_l_3().border_color(t.accent()))
                    .when(!on, |d| d.border_l_3().border_color(t.panel()))
                    .hover(|d| d.bg(t.hover_mark()))
                    .child(c.title())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.category = c;
                        cx.notify();
                    })),
            );
        }
        if shown.is_empty() {
            side = side.child(
                div()
                    .px_2()
                    .py_1()
                    .text_size(ms.small())
                    .text_color(t.muted())
                    .child("nothing matches"),
            );
        }

        let body = div()
            .id("cat-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_3()
            .gap_1()
            .child(
                div()
                    .text_size(ms.heading())
                    .text_color(t.fg())
                    .child(self.category.title()),
            )
            .child(
                div()
                    .text_size(ms.small())
                    .text_color(t.muted())
                    .child(self.category.blurb()),
            )
            .child(self.page(cx));

        let head = div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .px_3()
            .py_2()
            .flex_none()
            .bg(t.panel())
            .border_b_1()
            .border_color(t.border())
            .child(div().flex_1().text_size(ms.body()).child(self.search.clone()))
            // The theme filter lives up here with it, on the one page that has
            // a list long enough to need filtering. Up here because a field
            // cannot go in the scrolling page — see `theme_filter`.
            .when(self.category == Category::General, |d| {
                d.child(
                    div()
                        .w(px(ms.base() * 14.0))
                        .flex_none()
                        .text_size(ms.body())
                        .child(self.theme_filter.clone()),
                )
            });

        let foot = div()
            .flex()
            .items_center()
            .justify_end()
            .gap_2()
            .w_full()
            .px_3()
            .py_2()
            .flex_none()
            .border_t_1()
            .border_color(t.border())
            .child(
                btn(&t, ms, "settings-close", "Close", Btn::Secondary).on_click(cx.listener(
                    |this, _, _, cx| {
                        let _ = this.workspace.update(cx, |w, cx| w.close_settings(cx));
                    },
                )),
            );

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
            // The SAME type as the main window. A separate window gets the
            // platform default unless it is told, and `$LODE_FONT` chose the
            // other one — so the settings window was in a different face from
            // the window it belongs to.
            .font_family(SharedString::from(self.font_family.clone()))
            .line_height(ms.line_h())
            .text_size(ms.body())
            .child(head)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(side)
                    .child(body),
            )
            .child(foot)
    }
}

impl SettingsWindow {
    /// A line of label and value, which most of these pages are made of.
    fn row(&self, label: impl Into<SharedString>, value: impl Into<SharedString>) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        div()
            .flex()
            .gap_2()
            .py(px(1.0))
            .child(
                div()
                    .w(px(ms.base() * 13.0))
                    .flex_none()
                    .text_size(ms.small())
                    .text_color(t.muted())
                    .child(label.into()),
            )
            .child(div().flex_1().text_size(ms.small()).text_color(t.fg()).child(value.into()))
    }

    /// A heading inside a page. ONE style of heading in this window: the rail's
    /// settings panel had two (a fold header and a section line) and the two
    /// were never told apart by anybody reading it.
    fn head(&self, label: impl Into<SharedString>) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        div()
            .pt_2()
            .text_size(ms.tiny())
            .text_color(t.title())
            .child(label.into())
    }

    /// A glyph in the ICON font. The eye and the padlock are in the icon font
    /// and nowhere else; drawn in the text font they come out as a box.
    fn icon(&self, glyph: &'static str, colour: gpui::Rgba) -> gpui::Div {
        let ms = self.scale;
        div()
            .font_family(SharedString::from(self.icon_family.clone()))
            .text_size(px(ms.icon_size()))
            .text_color(colour)
            .child(glyph)
    }

    /// One square button of a stepper.
    fn step(
        &self,
        id: String,
        label: &'static str,
        on_press: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (t, ms) = (&self.theme, self.scale);
        // A stepper's button is square — it holds one sign, not a word — so it
        // is the shared control with its width pinned to its height.
        crate::widgets::btn(t, ms, id, label, Btn::Secondary)
            .w(px(ms.control_h()))
            .on_click(cx.listener(move |this, _, window, cx| on_press(this, window, cx)))
    }

    /// One of a row of mutually exclusive choices.
    fn pick(
        &self,
        id: String,
        label: impl Into<SharedString>,
        on: bool,
        on_press: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (t, ms) = (&self.theme, self.scale);
        crate::widgets::chip(t, ms, id, label, on)
            .on_click(cx.listener(move |this, _, window, cx| on_press(this, window, cx)))
    }

    /// What the main window currently looks like. `None` only if it has gone.
    fn appearance(&self, cx: &gpui::App) -> Option<Appearance> {
        Some(self.workspace.upgrade()?.read(cx).appearance())
    }

    /// One phrase of the interface.
    ///
    /// The language comes off the SNAPSHOT rather than being stored here, so
    /// there is nothing to keep in step: this window is where the language is
    /// changed, and the row that changes it is drawn from the same read.
    fn tr(&self, key: &str, cx: &gpui::App) -> SharedString {
        let lang = self.appearance(cx).map(|a| a.lang).unwrap_or_default();
        SharedString::from(lang.tr(key))
    }

    fn note(&self, text: impl Into<SharedString>) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        div().py_1().text_size(ms.small()).text_color(t.muted()).child(text.into())
    }

    /// A capability chip. Faded when the driver does not have it, because what
    /// a driver CANNOT do is as much worth seeing as what it can — and a list
    /// that only shows the yeses cannot be scanned for a no.
    fn chip(&self, label: &'static str, on: bool) -> gpui::Div {
        crate::widgets::flag(&self.theme, self.scale, label, on)
    }

    fn page(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (t, ms) = (self.theme.clone(), self.scale);
        let mut out = div().flex().flex_col().pt_2().gap_1();
        match self.category {
            Category::Drivers => {
                // Nothing new is written here: every one of these is already
                // declared by the driver and has never been shown anywhere.
                out = out.child(self.note(format!("{} drivers are registered.", self.drivers.len())));
                let mut names = div().flex().flex_wrap().gap_1().py_1();
                for d in &self.drivers {
                    let on = self.driver_shown.as_deref() == Some(d.kind.as_str());
                    let pick = d.kind.clone();
                    names = names.child(
                        crate::widgets::chip(
                            &t,
                            ms,
                            format!("drv-{}", d.kind),
                            d.display.clone(),
                            on,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.driver_shown = Some(pick.clone());
                            cx.notify();
                        })),
                    );
                }
                out = out.child(names);
                if let Some(d) = self
                    .driver_shown
                    .as_ref()
                    .and_then(|k| self.drivers.iter().find(|d| &d.kind == k))
                {
                    out = out.child(self.row("Driver", d.kind.clone()));
                    out = out.child(self.row(
                        "Default port",
                        d.default_port.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
                    ));
                    out = out.child(self.row("Parameters", format!("{}", d.params.len())));
                    out = out.child(
                        div().pt_2().text_size(ms.tiny()).text_color(t.title()).child("CAPABILITIES"),
                    );
                    let c = &d.caps;
                    let mut chips = div().flex().flex_wrap().gap_1().py_1();
                    for (label, on) in [
                        ("SQL", c.sql),
                        ("Schemas", c.schemas),
                        ("Multiple databases", c.multi_db),
                        ("Key/value", c.kv),
                        ("SSH tunnel", c.tunnel),
                        ("SSL/TLS", c.tls),
                        ("Cancel a query", c.cancel),
                        ("Indexes", c.indexes),
                        ("Show DDL", c.ddl),
                        ("Edit a row", c.edit),
                        ("Instance metrics", c.metrics),
                    ] {
                        chips = chips.child(self.chip(label, on));
                    }
                    out = out.child(chips);
                    out = out.child(self.note(
                        "Faded means the driver says it cannot. The engine refuses those rather \
                         than letting the window offer them.",
                    ));
                }
            }
            Category::General => {
                let Some(a) = self.appearance(cx) else {
                    return out.child(self.note("the main window has gone")).into_any_element();
                };

                // ── the theme ───────────────────────────────────────────────
                out = out.child(self.head(self.tr("settings.theme", cx).to_uppercase()));
                out = out.child(
                    btn(&t, ms, "themes-rescan", self.tr("panel.rescan", cx), Btn::Quiet).on_click(cx.listener(
                        |this, _, _, cx| {
                            let _ = this.workspace.update(cx, |w, cx| w.rescan_themes(cx));
                            cx.notify();
                        },
                    )),
                );
                // WHICH folders were read. "No themes" on its own is something
                // nobody can act on.
                for d in &a.theme_dirs {
                    out = out.child(
                        div().text_size(ms.tiny()).text_color(t.muted()).child(d.clone()),
                    );
                }
                if a.themes.is_empty() {
                    out = out.child(
                        div()
                            .text_size(ms.small())
                            .text_color(t.warning())
                            .child(self.tr("settings.no_themes", cx)),
                    );
                    out = out.child(self.note("the built-in theme is the generated LvimBase_dark"));
                }
                let needle = self.theme_filter.read(cx).text().trim().to_lowercase();
                let mut names: Vec<String> = a
                    .themes
                    .iter()
                    .filter(|n| needle.is_empty() || n.to_lowercase().contains(&needle))
                    .cloned()
                    .collect();
                names.sort();
                // Every one of them, in the page's own scrolling. No cap and no
                // inner scroller: a window is wide and tall enough that the
                // thing the rail's panel kept fighting — thirty-six themes in a
                // narrow column — is not a problem here.
                let mut list = div().flex().flex_col().py_1();
                for name in &names {
                    let on = *name == a.theme_name;
                    let pick = name.clone();
                    list = list.child(
                        div()
                            .id(SharedString::from(format!("theme-{name}")))
                            .flex()
                            .items_center()
                            .gap_1()
                            .px_1()
                            .py(px(1.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover_mark()))
                            .when(on, |d| d.bg(t.selected_mark()))
                            .child(
                                div()
                                    .w(px(ms.base()))
                                    .flex_none()
                                    .text_size(ms.tiny())
                                    .text_color(t.accent())
                                    .child(if on { "\u{f00c}" } else { " " }),
                            )
                            .child(
                                div()
                                    .text_size(ms.small())
                                    .text_color(if on { t.fg() } else { t.muted() })
                                    .child(SharedString::from(name.clone())),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let pick = pick.clone();
                                let _ = this.workspace.update(cx, |w, cx| w.use_theme(&pick, cx));
                                cx.notify();
                            })),
                    );
                }
                out = out.child(list);
                if names.is_empty() && !a.themes.is_empty() {
                    out = out.child(self.note(format!(
                        "no theme matches \"{}\"",
                        self.theme_filter.read(cx).text().trim()
                    )));
                }

                // ── the type ────────────────────────────────────────────────
                out = out.child(self.head(self.tr("settings.text", cx).to_uppercase()));
                out = out.child(self.row(self.tr("settings.text_font", cx), a.font_family.clone()));
                out = out.child(self.row(self.tr("settings.icon_font", cx), a.icon_family.clone()));
                out = out.child(self.note(self.tr("settings.font_env", cx)));

                // One number, and it moves everything: the steps, the grid row
                // height, the rail. Adjusted while looking at the result, which
                // is why it is here and not only an environment variable.
                let base = a.base;
                out = out.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .w(px(ms.base() * 13.0))
                                .flex_none()
                                .text_size(ms.small())
                                .text_color(t.muted())
                                .child(self.tr("settings.text_size", cx)),
                        )
                        .child(self.step(
                            "text-less".into(),
                            "\u{2212}",
                            move |this, _, cx| {
                                let _ = this
                                    .workspace
                                    .update(cx, |w, cx| w.set_base(base - 1.0, cx));
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(
                            div()
                                .min_w(px(ms.base() * 3.0))
                                .text_size(ms.body())
                                .text_color(t.fg())
                                .child(SharedString::from(format!("{base} px"))),
                        )
                        .child(self.step(
                            "text-more".into(),
                            "+",
                            move |this, _, cx| {
                                let _ = this
                                    .workspace
                                    .update(cx, |w, cx| w.set_base(base + 1.0, cx));
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(btn(&t, ms, "size-reset", self.tr("settings.reset", cx), Btn::Quiet).on_click(
                            cx.listener(|this, _, _, cx| {
                                let _ = this.workspace.update(cx, |w, cx| w.reset_scale(cx));
                                cx.notify();
                            }),
                        )),
                );
                // The rail is navigation and the text is content; enlarging one
                // is not a request for the other, so they have their own rows.
                let icon = a.icon;
                out = out.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            div()
                                .w(px(ms.base() * 13.0))
                                .flex_none()
                                .text_size(ms.small())
                                .text_color(t.muted())
                                .child(self.tr("settings.icon_size", cx)),
                        )
                        .child(self.step(
                            "icon-less".into(),
                            "\u{2212}",
                            move |this, _, cx| {
                                let _ = this
                                    .workspace
                                    .update(cx, |w, cx| w.set_icon(icon - 2.0, cx));
                                cx.notify();
                            },
                            cx,
                        ))
                        .child(
                            div()
                                .min_w(px(ms.base() * 3.0))
                                .text_size(ms.body())
                                .text_color(t.fg())
                                .child(SharedString::from(format!("{icon} px"))),
                        )
                        .child(self.step(
                            "icon-more".into(),
                            "+",
                            move |this, _, cx| {
                                let _ = this
                                    .workspace
                                    .update(cx, |w, cx| w.set_icon(icon + 2.0, cx));
                                cx.notify();
                            },
                            cx,
                        ))
                        // Beside the number, so the effect can be seen before
                        // anything is pressed.
                        .child(self.icon("\u{f1c0}", t.muted()))
                        .child(self.icon("\u{f013}", t.accent())),
                );
                out = out.child(self.note(format!(
                    "every size comes off the text size — headings {}, body {}, small {}, a grid \
                     row {}, and the rail is {} px wide",
                    f32::from(ms.heading()),
                    f32::from(ms.body()),
                    f32::from(ms.small()),
                    ms.row_h(),
                    ms.rail_w()
                )));

                // ── the language ────────────────────────────────────────────
                out = out.child(self.head(self.tr("settings.language", cx).to_uppercase()));
                let mut langs = div().flex().items_center().gap_2().py_1();
                for l in crate::i18n::Lang::ALL.iter().copied() {
                    // Its own name, not its English one: somebody looking for
                    // their language looks for the word they would write.
                    langs = langs.child(self.pick(
                        format!("lang-{}", l.tag()),
                        l.label(),
                        a.lang == l,
                        move |this, _, cx| {
                            let _ = this.workspace.update(cx, |w, cx| w.set_lang(l, cx));
                            cx.notify();
                        },
                        cx,
                    ));
                }
                out = out.child(langs);
                out = out.child(self.note(
                    "the engine's refusals and the audit log stay in English — they are the \
                     record, not the interface",
                ));

                // ── what the window remembers ───────────────────────────────
                out = out.child(self.head(self.tr("settings.json_indent", cx).to_uppercase()));
                let mut inds = div().flex().items_center().gap_2().py_1().child(
                    div()
                        .w(px(ms.base() * 13.0))
                        .flex_none()
                        .text_size(ms.small())
                        .text_color(t.muted())
                        .child(self.tr("settings.json_indent", cx)),
                );
                for n in [2u8, 3, 4] {
                    inds = inds.child(self.pick(
                        format!("jsonind-{n}"),
                        n.to_string(),
                        a.json_indent == n,
                        move |this, _, cx| {
                            let _ = this.workspace.update(cx, |w, cx| w.set_json_indent(n, cx));
                            cx.notify();
                        },
                        cx,
                    ));
                }
                out = out.child(inds);
                out = out.child(self.note(self.tr("settings.remembers", cx)));
            }
            Category::Tunnels | Category::Proxies => {
                let (rows, kind) = if self.category == Category::Tunnels {
                    (self.tunnels.clone(), "tunnel")
                } else {
                    (self.proxies.clone(), "proxy")
                };
                if rows.is_empty() {
                    out = out.child(self.note(format!(
                        "No saved {kind}s yet — build one in a connection's Access tab and press \
                         \"save as profile\"."
                    )));
                }
                out = out.child(
                    btn(&t, ms, format!("prof-new-{kind}"), format!("+  new {kind}"), Btn::Secondary)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let _ = this.workspace.update(cx, |w, cx| w.open_profile(None, cx));
                            cx.notify();
                        })),
                );
                for r in &rows {
                    let users = self
                        .workspace
                        .upgrade()
                        .map(|w| w.read(cx).profile_users_of(&r.name))
                        .unwrap_or_default();
                    let (name, spec) = (r.name.clone(), r.spec.clone());
                    out = out.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py(px(1.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_size(ms.small())
                                            .text_color(t.fg())
                                            .child(SharedString::from(r.name.clone())),
                                    )
                                    // WHAT it is, read off its own spec.
                                    .child(
                                        div()
                                            .text_size(ms.tiny())
                                            .text_color(t.muted())
                                            .child(SharedString::from(r.summary_of(kind))),
                                    ),
                            )
                            .child({
                                let (n, sp) = (name.clone(), spec.clone());
                                btn(&t, ms, format!("prof-ed-{kind}-{n}"), "edit", Btn::Link)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let (n, sp) = (n.clone(), sp.clone());
                                        let _ = this.workspace.update(cx, |w, cx| {
                                            w.open_profile(Some((n, sp)), cx)
                                        });
                                        cx.notify();
                                    }))
                            })
                            .child({
                                let n = name.clone();
                                // ASK first. The list of dependants is already
                                // here, so the warning can stand where the row
                                // is rather than arriving as a toast once it is
                                // too late.
                                btn(&t, ms, format!("prof-rm-{kind}-{n}"), "remove", Btn::Quiet)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.confirm_rm = Some((kind, n.clone()));
                                        cx.notify();
                                    }))
                            }),
                    );
                    if !users.is_empty() {
                        out = out.child(self.note(format!(
                            "used by {}: {}",
                            users.len(),
                            users.join(", ")
                        )));
                    }
                    if self.confirm_rm.as_ref() == Some(&(kind, r.name.clone())) {
                        let why = if users.is_empty() {
                            format!("Remove \"{}\"? Nothing points at it.", r.name)
                        } else {
                            format!(
                                "Remove \"{}\"? {} connection(s) point at it and would stop \
                                 connecting: {}",
                                r.name,
                                users.len(),
                                users.join(", ")
                            )
                        };
                        out = out.child(
                            div().text_size(ms.small()).text_color(t.warning()).child(why),
                        );
                        let n = name.clone();
                        out = out.child(
                            div()
                                .flex()
                                .gap_2()
                                .py_1()
                                .child(
                                    btn(&t, ms, format!("prof-yes-{n}"), "remove it", Btn::Danger)
                                        .on_click(cx.listener({
                                            let n = n.clone();
                                            move |this, _, _, cx| {
                                                let n = n.clone();
                                                let _ = this.workspace.update(cx, |w, _| {
                                                    w.remove_profile(kind, &n)
                                                });
                                                this.confirm_rm = None;
                                                cx.notify();
                                            }
                                        })),
                                )
                                .child(
                                    btn(&t, ms, format!("prof-no-{n}"), "keep it", Btn::Quiet)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_rm = None;
                                            cx.notify();
                                        })),
                                ),
                        );
                    }
                }
                out = out.child(self.note(
                    "A connection picks one by name in its Access tab. A connection whose \
                     profile is missing FAILS rather than falling back to a direct link.",
                ));
            }
            Category::About => {
                for (k, v) in &self.about {
                    out = out.child(self.row(k.clone(), v.clone()));
                }
            }
            // Pages whose content is still in the engine and not yet given a
            // face. Named rather than hidden: a category that exists and says
            // what it will hold is a promise; one that is missing is a feature
            // nobody knows about — which is what these have been.
            other => {
                out = out.child(self.note(match other {
                    Category::Keys => {
                        "The palette is ctrl-p and the grid has its own keys. None of the rail's \
                         panels has a key yet, which is the gap this page is for."
                    }
                    Category::Audit => {
                        "The engine records every statement, with the grammar-based redaction, \
                         and LODE_AUDIT_STATEMENTS chooses how much. It belongs here as a \
                         setting rather than an environment variable."
                    }
                    Category::Access => {
                        "The engine reads ~/.aws/config and reports what is usable. This page is \
                         where that belongs, including saying plainly which provider kinds are \
                         not implemented."
                    }
                    Category::Hooks => {
                        "The engine runs commands, scripts and Lua around connect and disconnect. \
                         They can only be set per connection today."
                    }
                    Category::Rpc => {
                        "The engine starts external drivers over RPC and they register like any \
                         other. There is no way to add one from here yet."
                    }
                    Category::Mcp => {
                        "The engine has roles, approvals and an audit of every tool call. \
                         Execution classes — metadata, read, write, destructive, admin — are a \
                         better shape than the present yes/no and are not built."
                    }
                    _ => "",
                }));
            }
        }
        out.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The search is what makes a long settings window usable: somebody who
    /// knows the word does not have to know which page it lives on.
    #[test]
    fn a_search_finds_a_page_by_what_it_is_about() {
        let all = Category::all();
        assert_eq!(all.len(), 11);

        let find = |needle: &str| -> Vec<&'static str> {
            let n = needle.trim().to_lowercase();
            all.iter()
                .filter(|c| {
                    n.is_empty()
                        || c.title().to_lowercase().contains(&n)
                        || c.blurb().to_lowercase().contains(&n)
                        || c.keywords().contains(&n)
                })
                .map(|c| c.title())
                .collect()
        };

        // The word somebody actually has in mind, not the page's name.
        assert!(find("passphrase").contains(&"SSH tunnels"), "{:?}", find("passphrase"));
        assert!(find("socks").contains(&"Proxies"));
        assert!(find("sso").contains(&"Auth profiles"));
        assert!(find("lua").contains(&"Hooks"));
        assert!(find("capabilities").contains(&"Drivers"));
        assert!(find("approval").contains(&"MCP policies"));
        assert!(find("redaction").contains(&"Audit"));
        assert!(find("shortcut").contains(&"Keybindings"));

        // Empty shows everything; nonsense shows nothing — and nothing is a
        // real answer, as long as the window says so rather than looking broken.
        assert_eq!(find("").len(), 11);
        assert!(find("zzzz-not-a-thing").is_empty());

        // Every category carries words to be found by. One that does not is one
        // nobody can reach except by reading the whole list.
        for c in all {
            assert!(!c.keywords().is_empty(), "{} has nothing to search on", c.title());
            assert!(!c.blurb().is_empty(), "{} does not say what it is for", c.title());
        }
    }

    /// A proxy is not a tunnel, and its line must not invent a port 22 or a
    /// `user@` for it.
    #[test]
    fn a_proxy_row_does_not_pretend_to_be_a_tunnel() {
        use crate::app::ProfileRow;
        use serde_json::json;

        let tunnel = ProfileRow {
            name: "bastion".into(),
            spec: json!({ "host": "jump.example", "user": "ops", "auth": { "kind": "agent" } }),
        };
        assert_eq!(tunnel.summary_of("tunnel"), "ops@jump.example:22 · agent");

        // The same spec read as a proxy: no user, no 22.
        let proxy = ProfileRow {
            name: "office".into(),
            spec: json!({ "host": "socks.example", "port": 1080, "kind": "socks5" }),
        };
        assert_eq!(proxy.summary_of("proxy"), "socks5 · socks.example:1080");

        // A proxy saved without a port SAYS so. Showing `:1080` would hide an
        // unfinished profile behind a plausible number.
        let no_port = ProfileRow {
            name: "half".into(),
            spec: json!({ "host": "socks.example", "kind": "http" }),
        };
        assert_eq!(no_port.summary_of("proxy"), "http · socks.example (no port)");

        // Nothing at all is still an answer.
        let empty = ProfileRow { name: "x".into(), spec: json!({}) };
        assert_eq!(empty.summary_of("proxy"), "—");
        assert_eq!(empty.summary_of("tunnel"), "—");
    }

    /// The four section headings group the eleven pages; every page sits under
    /// one, so none of them floats above the first heading.
    #[test]
    fn every_page_falls_under_a_heading() {
        let mut seen = None;
        for c in Category::all() {
            if let Some(s) = c.section() {
                seen = Some(s);
            }
            assert!(seen.is_some(), "{} comes before any heading", c.title());
        }
    }
}
