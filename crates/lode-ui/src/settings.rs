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
/// HANDED OVER, and handed over again whenever it changes — never read from the
/// workspace while this window draws.
///
/// It WAS read fresh on every paint, which is the better shape and does not
/// work: `App::open_window` draws the new window once before it returns ("allow
/// a window to draw at least once", in its own words), and `open_settings` runs
/// in a listener — so that draw happens while the workspace is still inside its
/// own update, and reading an entity that is being updated is a panic in gpui.
/// The panic was the gear button closing the whole application.
///
/// What keeps this in step is that every setter calls
/// `Workspace::poke_settings`, which pushes a fresh one in. A setter that
/// forgets leaves a stale number beside the stepper that moves it.
#[derive(Clone)]
pub struct Appearance {
    /// A font that was asked for and is not installed, so the page can say
    /// what was used instead rather than showing a choice nobody made.
    pub font_asked: Option<String>,
    pub icon_asked: Option<String>,
    /// Whether the connection window refuses to save a password as typed.
    ///
    /// Off by default, because turning it on without a wallet would leave
    /// somebody unable to save a connection at all. On, the only ways to give
    /// a password are the wallet, the environment and a command — and the
    /// store cannot hold one even by accident.
    pub secrets_only_from_a_source: bool,
    /// Keys somebody has changed, so the page can mark them and say what the
    /// default was.
    pub keys: std::collections::BTreeMap<String, String>,
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

/// What the ENGINE has said, for the pages that report rather than set.
///
/// Separate from `Appearance` because it changes for a different reason and at
/// a different time: the look changes when somebody moves a stepper, and these
/// change when the engine answers. Pushed the same way, for the same reason —
/// see `Appearance`.
/// What the WALLET said, pushed in like everything else this window knows.
///
/// `reachable` and `why` are separate from `exists` because they are different
/// facts about different things: no agent is a machine with nothing installed,
/// and no vault is an agent with nothing in it. A page that showed one as the
/// other would tell somebody to install what they have.
#[derive(Clone, Default)]
pub struct Wallet {
    pub reachable: bool,
    pub exists: bool,
    pub locked: bool,
    pub proto: u32,
    /// Why it is not reachable, in the engine's words.
    pub why: Option<String>,
}

#[derive(Clone, Default)]
pub struct Facts {
    pub proto: u32,
    pub version: String,
    pub engine_binary: String,
    pub store_path: String,
    pub store_ok: bool,
    /// How much of a statement the audit keeps: `full`, `shape` or `off`.
    pub audit_mode: String,
    /// Each registered driver program: name, kind, whether it loaded, why not.
    pub external: Vec<(String, String, bool, String)>,
    /// Each sign-in the engine found: name, kind, whether it can be used, why
    /// not.
    pub access: Vec<(String, String, bool, String)>,
    /// The MCP approval queue: id, statement, what it is waiting for.
    pub approvals: Vec<(String, String, String)>,
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
    /// The wallet: whether this machine has one, and whether it is open.
    Keyring,
    Hooks,
    Drivers,
    Rpc,
    Mcp,
    About,
}

impl Category {
    pub fn all() -> [Category; 12] {
        use Category::*;
        [
            General, Keys, Audit, Tunnels, Proxies, Access, Keyring, Hooks, Drivers, Rpc, Mcp,
            About,
        ]
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
            Category::Keyring => "Keyring",
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
            Category::Keyring => {
                "The wallet this machine keeps its secrets in \u{2014} the same one the Neovim \
                 client uses."
            }
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
            Category::Keyring => {
                "vault wallet passphrase unlock lock master secret lvim-keyring agent"
            }
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
    /// Which font row is open: `"text"`, `"icon"`, or neither.
    font_pick: Option<&'static str>,
    /// The filter over the installed fonts. Its own box: a machine with six
    /// hundred fonts is a list nobody scrolls.
    font_filter: Entity<TextInput>,
    /// Which key is being changed, by action name.
    editing_key: Option<String>,
    /// The field the new keystroke is typed into.
    key_input: Entity<TextInput>,
    /// What the wallet said when last asked.
    wallet: Wallet,
    /// What came back from the last unlock or create.
    wallet_said: Option<String>,
    /// The passphrase box. Masked from the moment it is built: this one is
    /// never anything but a secret, so there is no source to pick and no eye.
    pass_input: Entity<TextInput>,
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
    /// What the main window looks like — see `Appearance`.
    look: Appearance,
    /// Which connections point at each profile, as the engine answered.
    users: std::collections::BTreeMap<String, Vec<String>>,
    /// What the engine has said — see `Facts`.
    facts: Facts,
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
        look_now: Appearance,
        users: std::collections::BTreeMap<String, Vec<String>>,
        facts: Facts,
        cx: &mut Context<Self>,
    ) -> Self {
        let crate::widgets::Look { theme, scale, text_font, icon_font, lang } = look;
        let colors = crate::form::colors_of(&theme);
        let search = cx.new(|cx| TextInput::new(cx, lang.tr("ph.search_settings"), colors));
        let theme_filter = cx.new(|cx| TextInput::new(cx, lang.tr("settings.filter_themes"), colors));
        let key_input = cx.new(|cx| TextInput::new(cx, "ctrl-shift-k", colors));
        let font_filter = cx.new(|cx| TextInput::new(cx, lang.tr("ph.filter_fonts"), colors));
        let pass_input = cx.new(|cx| {
            let mut i = TextInput::new(cx, lang.tr("ph.passphrase"), colors);
            i.set_mask(true, cx);
            i
        });
        cx.subscribe(&key_input, |this, input, ev: &crate::input::InputEvent, cx| {
            if matches!(ev, crate::input::InputEvent::Submit) {
                let typed = input.read(cx).text().trim().to_string();
                this.commit_key(typed, cx);
            }
        })
        .detach();
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
            editing_key: None,
            font_pick: None,
            font_filter,
            key_input,
            pass_input,
            wallet: Wallet::default(),
            wallet_said: None,
            theme_filter,
            font_family: text_font,
            icon_family: icon_font,
            driver_shown,
            about,
            look: look_now,
            users,
            facts,
        }
    }

    /// The engine answered something one of these pages reports.
    pub fn facts_arrived(&mut self, facts: Facts, cx: &mut Context<Self>) {
        self.facts = facts;
        cx.notify();
    }

    /// The main window's look changed — see `Appearance`.
    pub fn look_changed(&mut self, look: Appearance, cx: &mut Context<Self>) {
        self.look = look;
        cx.notify();
    }

    /// Who points at each profile, as the engine answered.
    pub fn users_arrived(
        &mut self,
        users: std::collections::BTreeMap<String, Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        self.users = users;
        cx.notify();
    }

    pub fn all_inputs(&self) -> Vec<Entity<TextInput>> {
        vec![
            self.search.clone(),
            self.theme_filter.clone(),
            self.key_input.clone(),
            self.pass_input.clone(),
            self.font_filter.clone(),
        ]
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
        let mut side = crate::widgets::scroll("cat-list").outer(|d| {
            d.w(px(ms.base() * 15.0))
                .flex_none()
                .h_full()
                .bg(t.panel())
                .border_r_1()
                .border_color(t.border())
        });
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
                    .child(self.tr("ui.nothing_matches")),
            );
        }

        let body = crate::widgets::scroll("cat-body")
            .outer(|d| d.p_3())
            .rows(|d| d.gap_1())
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
                btn(&t, ms, "settings-close", self.tr("settings.close"), Btn::Secondary).on_click(
                    cx.listener(|this, _, window: &mut Window, cx| this.leave(window, cx)),
                ),
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
            // The size of the text it sits beside, not the RAIL's number: this
            // is in a settings row, and the rail's size is a separate thing a
            // person sets for the rail.
            .text_size(ms.icon_beside(ms.small()))
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

    /// A font row: what is being used, and a way to change it.
    ///
    /// Says what was ASKED FOR when that is not what is being used. A name in
    /// the session or in `$LODE_FONT` that is not installed falls back
    /// through a list, and a window that showed the fallback as though it had
    /// been chosen would leave somebody searching their config for a setting
    /// that was already right.
    fn font_row(
        &self,
        label: SharedString,
        which: &'static str,
        now: &str,
        asked: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        let open = self.font_pick == Some(which);
        let mut row = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(SharedString::from(format!("font-{which}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .py(px(1.0))
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover_mark()))
                    .child(
                        div()
                            .w(px(ms.base() * 11.0))
                            .flex_none()
                            .text_size(ms.small())
                            .text_color(t.muted())
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(ms.small())
                            .text_color(t.fg())
                            .child(SharedString::from(now.to_string())),
                    )
                    .child(self.icon(if open { "\u{f0d7}" } else { "\u{f0da}" }, t.muted()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.font_pick = if open { None } else { Some(which) };
                        cx.notify();
                    })),
            );
        if let Some(wanted) = asked {
            row = row.child(
                div().text_size(ms.tiny()).text_color(t.warning()).child(SharedString::from(
                    format!("{wanted} is not installed \u{2014} using {now}"),
                )),
            );
        }
        row
    }

    /// Every installed font, to choose from.
    ///
    /// The ICON list marks the ones that do not carry the marks this window
    /// draws. Marked rather than hidden: a font that covers the four glyphs
    /// somebody actually looks at is a reasonable choice, and refusing it
    /// would be this window deciding for them. What it must not do is let
    /// them pick one and then show boxes with no explanation.
    fn font_list(&self, which: &'static str, cx: &mut Context<Self>) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        let needle = self.font_filter.read(cx).text().trim().to_lowercase();
        let all = cx.text_system().all_font_names();
        let mut names: Vec<String> = all
            .into_iter()
            .filter(|n| needle.is_empty() || n.to_lowercase().contains(&needle))
            .collect();
        names.sort();
        names.dedup();
        let shown = names.len();
        let now = if which == "icon" {
            self.look.icon_family.clone()
        } else {
            self.look.font_family.clone()
        };
        let mut list = crate::widgets::scroll(SharedString::from(format!("fonts-{which}")))
            .outer(|d| d.max_h(px(ms.base() * 18.0)).border_1().border_color(t.border()));
        for n in names.into_iter().take(400) {
            let on = n == now;
            // Only for the ICON list, and only then: asking the text system
            // about every installed font is a measurement per font, and the
            // text list does not need the answer.
            let bare = which == "icon" && !crate::app::covers_all_glyphs(cx, &n);
            let name = n.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("f-{which}-{n}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(1.0))
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover_mark()))
                    .when(on, |d| d.bg(t.selected_mark()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(ms.small())
                            .text_color(if on { t.accent() } else { t.fg() })
                            // DRAWN IN ITSELF: a list of font names in one
                            // face is a list nobody can choose from.
                            .font_family(SharedString::from(n.clone()))
                            .child(SharedString::from(n.clone())),
                    )
                    .children(bare.then(|| {
                        div()
                            .flex_none()
                            .text_size(ms.tiny())
                            .text_color(t.warning())
                            .child(self.tr("ui.no_icons"))
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let n = name.clone();
                        let _ = this.workspace.update(cx, |w, cx| {
                            if which == "icon" {
                                w.set_icon_font(n, cx);
                            } else {
                                w.set_text_font(n, cx);
                            }
                        });
                        this.font_pick = None;
                        cx.notify();
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .py_1()
            .child(div().text_size(ms.body()).child(self.font_filter.clone()))
            .child(self.note(format!("{shown} installed")))
            .child(list)
    }

    /// Close this window, from INSIDE it.
    ///
    /// Its own `&mut Window`. Asking the workspace to close it is
    /// `WindowHandle::update` on the window already being updated, which gpui
    /// refuses and the `let _ =` hid — the third place in this client with
    /// the same fault, after the connection window's Cancel and the profile
    /// window's.
    fn leave(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.workspace.update(cx, |w, cx| w.forget_settings(cx));
        window.remove_window();
    }

    /// The wallet answered. Pushed, like everything else here.
    pub fn wallet_arrived(&mut self, w: Wallet, said: Option<String>, cx: &mut Context<Self>) {
        self.wallet = w;
        if said.is_some() {
            self.wallet_said = said;
            // The passphrase is not kept a moment longer than the request that
            // carried it.
            self.pass_input.update(cx, |i, cx| i.set_text(String::new(), cx));
        }
        cx.notify();
    }

    /// Unlock, create or lock — the passphrase comes out of the box and goes
    /// straight to the engine.
    fn wallet_do(&mut self, what: &'static str, cx: &mut Context<Self>) {
        let pass = self.pass_input.read(cx).text().to_string();
        if what != "lock" && pass.is_empty() {
            self.wallet_said = Some("a passphrase is needed".into());
            cx.notify();
            return;
        }
        self.wallet_said = None;
        let _ = self.workspace.update(cx, |w, _| w.wallet_do(what, pass));
        cx.notify();
    }

    /// A SETTING that is on or off: a drawn box, its name, and a sentence
    /// saying what turning it on does.
    ///
    /// A chip would do for one of several choices; this is a yes or a no, and
    /// a yes/no drawn as a chip reads as "one of one".
    fn tickbox(
        &self,
        label: impl Into<SharedString>,
        why: impl Into<SharedString>,
        on: bool,
        press: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (t, ms) = (&self.theme, self.scale);
        let label: SharedString = label.into();
        div()
            .id(SharedString::from(format!("tick-{label}")))
            .flex()
            .flex_col()
            .py_1()
            .cursor_pointer()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::widgets::tick(t, ms, on))
                    .child(
                        div()
                            .text_size(ms.small())
                            .text_color(if on { t.fg() } else { t.muted() })
                            .child(label),
                    ),
            )
            .child(self.note(why.into()))
            .on_click(cx.listener(move |this, _, window, cx| press(this, window, cx)))
    }

    /// The new keystroke was typed. Hand it to the workspace, which owns the
    /// bindings — an empty answer puts the default back.
    fn commit_key(&mut self, typed: String, cx: &mut Context<Self>) {
        let Some(name) = self.editing_key.take() else { return };
        let _ = self
            .workspace
            .update(cx, |w, cx| w.set_key_override(&name, &typed, cx));
        cx.notify();
    }

    /// Tab and shift-tab between the two fields — see
    /// `ConnectionForm::step_focus`. Two is still worth it: the search box and
    /// the theme filter are at opposite ends of the header.
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

    /// One phrase of the interface, in the language the main window is in.
    fn tr(&self, key: &str) -> SharedString {
        SharedString::from(self.look.lang.tr(key))
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
                    out = out.child(self.row(self.tr("ui.driver"), d.kind.clone()));
                    out = out.child(self.row(
                        "Default port",
                        d.default_port.map(|p| p.to_string()).unwrap_or_else(|| "—".into()),
                    ));
                    out = out.child(self.row(self.tr("ui.parameters"), format!("{}", d.params.len())));
                    out = out.child(
                        div().pt_2().text_size(ms.tiny()).text_color(t.title()).child(self.tr("ui.capabilities")),
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
                        self.tr("settings.faded_means"),
                    ));
                }
            }
            Category::General => {
                let a = self.look.clone();

                // ── the theme ───────────────────────────────────────────────
                out = out.child(self.head(self.tr("settings.theme").to_uppercase()));
                out = out.child(
                    btn(&t, ms, "themes-rescan", self.tr("panel.rescan"), Btn::Quiet).on_click(cx.listener(
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
                            .child(self.tr("settings.no_themes")),
                    );
                    out = out.child(self.note(self.tr("ui.builtin_theme")));
                }
                let needle = self.theme_filter.read(cx).text().trim().to_lowercase();
                let mut names: Vec<String> = a
                    .themes
                    .iter()
                    .filter(|n| needle.is_empty() || n.to_lowercase().contains(&needle))
                    .cloned()
                    .collect();
                names.sort();
                // A SCROLLER OF ITS OWN. The note above said a window is tall
                // enough for all of them; forty-nine installed says otherwise
                // — they filled the page from the heading to the bottom and
                // pushed the fonts, the sizes and the language off it, so the
                // page about everything was a page about themes.
                //
                // Bounded by what the WINDOW gives: tall enough to pick from
                // without hunting, short enough that what is under it is
                // still on screen.
                let mut list = crate::widgets::scroll("theme-list")
                    .outer(|d| d.max_h(px(ms.base() * 20.0)).border_1().border_color(t.border()))
                    .rows(|d| d.py_1());
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
                out = out.child(self.head(self.tr("settings.text").to_uppercase()));
                out = out.child(self.font_row(
                    self.tr("settings.text_font"),
                    "text",
                    &a.font_family,
                    a.font_asked.as_deref(),
                    cx,
                ));
                out = out.child(self.font_row(
                    self.tr("settings.icon_font"),
                    "icon",
                    &a.icon_family,
                    a.icon_asked.as_deref(),
                    cx,
                ));
                if let Some(open) = self.font_pick {
                    out = out.child(self.font_list(open, cx));
                }
                out = out.child(self.note(self.tr("settings.font_env")));

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
                                .child(self.tr("settings.text_size")),
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
                        .child(btn(&t, ms, "size-reset", self.tr("settings.reset"), Btn::Quiet).on_click(
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
                                .child(self.tr("settings.icon_size")),
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
                out = out.child(self.head(self.tr("settings.language").to_uppercase()));
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
                    self.tr("settings.lang_record"),
                ));

                // ── what the window remembers ───────────────────────────────
                out = out.child(self.head(self.tr("settings.json_indent").to_uppercase()));
                let mut inds = div().flex().items_center().gap_2().py_1().child(
                    div()
                        .w(px(ms.base() * 13.0))
                        .flex_none()
                        .text_size(ms.small())
                        .text_color(t.muted())
                        .child(self.tr("settings.json_indent")),
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
                out = out.child(self.note(self.tr("settings.remembers")));
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
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let _ =
                                this.workspace.update(cx, |w, cx| w.open_profile(kind, None, cx));
                            cx.notify();
                        })),
                );
                for r in &rows {
                    let users = self.users.get(&r.name).cloned().unwrap_or_default();
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
                                btn(&t, ms, format!("prof-ed-{kind}-{n}"), self.tr("ui.edit"), Btn::Link)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let (n, sp) = (n.clone(), sp.clone());
                                        let _ = this.workspace.update(cx, |w, cx| {
                                            w.open_profile(kind, Some((n, sp)), cx)
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
                                btn(&t, ms, format!("prof-rm-{kind}-{n}"), self.tr("ui.remove"), Btn::Quiet)
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
                                    btn(&t, ms, format!("prof-yes-{n}"), self.tr("ui.remove_it"), Btn::Danger)
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
                                    btn(&t, ms, format!("prof-no-{n}"), self.tr("ui.keep_it"), Btn::Quiet)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.confirm_rm = None;
                                            cx.notify();
                                        })),
                                ),
                        );
                    }
                }
                out = out.child(self.note(
                    self.tr("settings.profile_by_name"),
                ));
            }
            Category::About => {
                for (k, v) in &self.about {
                    out = out.child(self.row(k.clone(), v.clone()));
                }
                // WHERE things are. "Where is my saved connection" and "which
                // binary is this" had no answer anywhere in the window.
                let f = &self.facts;
                out = out.child(self.head(self.tr("ui.the_engine")));
                out = out.child(self.row(self.tr("ui.version"), f.version.clone()));
                out = out.child(self.row(self.tr("ui.protocol"), f.proto.to_string()));
                out = out.child(self.row(self.tr("ui.binary"), f.engine_binary.clone()));
                out = out.child(self.head(self.tr("ui.the_store")));
                out = out.child(self.row(
                    "State",
                    if f.store_ok { "open" } else { "NOT available" },
                ));
                out = out.child(self.row(self.tr("ui.path"), f.store_path.clone()));
                out = out.child(self.note(
                    self.tr("settings.one_store"),
                ));
            }
            Category::Keys => {
                out = out.child(self.note(
                    self.tr("settings.keys_howto"),
                ));
                // From the SAME table the bindings come from — it was written
                // out twice, with a test to stop the two disagreeing, and one
                // table cannot disagree with itself.
                let mut group = "";
                for (g, label, name, default) in crate::app::KEYMAP {
                    if *g != group {
                        group = g;
                        out = out.child(self.head(*g));
                    }
                    let now = self
                        .look
                        .keys
                        .get(*name)
                        .map(String::as_str)
                        .unwrap_or(default);
                    let changed = now != *default;
                    let editing = self.editing_key.as_deref() == Some(*name);
                    if editing {
                        out = out.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .py(px(1.0))
                                .child(
                                    div()
                                        .w(px(ms.base() * 13.0))
                                        .flex_none()
                                        .text_size(ms.small())
                                        .text_color(t.accent())
                                        .child(SharedString::from(*label)),
                                )
                                .child(
                                    div()
                                        .w(px(ms.base() * 12.0))
                                        .text_size(ms.small())
                                        .child(self.key_input.clone()),
                                ),
                        );
                        continue;
                    }
                    let whose = name.to_string();
                    out = out.child(
                        div()
                            .id(SharedString::from(format!("key-{name}")))
                            .flex()
                            .items_center()
                            .gap_2()
                            .py(px(1.0))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.hover_mark()))
                            .child(
                                div()
                                    .w(px(ms.base() * 13.0))
                                    .flex_none()
                                    .text_size(ms.small())
                                    .text_color(t.muted())
                                    .child(SharedString::from(*label)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(ms.small())
                                    // A changed key is marked, and its default
                                    // is said: "why does this not match the
                                    // documentation" needs an answer in the
                                    // place somebody is looking.
                                    .text_color(if changed { t.accent() } else { t.fg() })
                                    .child(SharedString::from(if changed {
                                        format!("{now}   (was {default})")
                                    } else {
                                        now.to_string()
                                    })),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.editing_key = Some(whose.clone());
                                this.key_input.update(cx, |i, cx| i.set_text("", cx));
                                cx.notify();
                            })),
                    );
                }
            }
            Category::Audit => {
                let f = &self.facts;
                out = out.child(self.row(self.tr("ui.keeping"), match f.audit_mode.as_str() {
                    "shape" => "the SHAPE of each statement, not its text",
                    "off" => "no statement text at all",
                    _ => "the whole statement, with credentials redacted",
                }));
                out = out.child(self.note(
                    "$LODE_AUDIT_STATEMENTS chooses this, and the engine reads it at startup: \
                     `full` (the default), `shape`, or `off`. It is the ENGINE's setting, not this \
                     window's — a client that could turn the record down would not be a record.",
                ));
                out = out.child(self.note(
                    self.tr("settings.audit_what"),
                ));
                out = out.child(self.note(
                    self.tr("settings.audit_panel"),
                ));
            }
            Category::Rpc => {
                let f = &self.facts;
                if f.external.is_empty() {
                    out = out.child(self.note(
                        self.tr("settings.no_drivers"),
                    ));
                }
                for (name, kind, ok, why) in &f.external {
                    out = out.child(self.row(
                        name.clone(),
                        if *ok {
                            format!("loaded as `{kind}`")
                        } else {
                            // WHY it is absent. Without this an installed
                            // driver that failed to describe itself is simply
                            // missing, which is a mystery rather than a fault.
                            format!("NOT loaded \u{2014} {why}")
                        },
                    ));
                }
                out = out.child(self.note(self.tr("ui.engine_restart")));
                out = out.child(self.head(self.tr("ui.older_store")));
                out = out.child(self.note(
                    self.tr("settings.import_store"),
                ));
                out = out.child(
                    btn(&t, ms, "store-import", self.tr("ui.choose_file"), Btn::Secondary).on_click(
                        cx.listener(|this, _, _, cx| {
                            let _ = this.workspace.update(cx, |w, cx| w.import_store(cx));
                        }),
                    ),
                );
            }
            Category::Keyring => {
                let w = &self.wallet;
                // WHAT IS THERE, before what to do about it. "Unlock" over a
                // machine with no wallet is a button for a thing that does not
                // exist, and "set a passphrase" over one that has a wallet is
                // an offer to lose it.
                match (w.reachable, w.exists, w.locked) {
                    (false, _, _) => {
                        out = out.child(self.row(
                            "The agent",
                            w.why.clone().unwrap_or_else(|| "not reachable".into()),
                        ));
                        out = out.child(self.note(
                            self.tr("settings.wallet_agent"),
                        ));
                    }
                    (true, false, _) => {
                        out = out.child(self.row(self.tr("ui.the_agent"), "running, with no wallet yet"));
                        out = out.child(self.note(
                            self.tr("settings.wallet_passphrase"),
                        ));
                        out = out.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .py_1()
                                .child(
                                    div()
                                        .w(px(ms.base() * 14.0))
                                        .text_size(ms.body())
                                        .child(self.pass_input.clone()),
                                )
                                .child(
                                    btn(&t, ms, "vault-create", self.tr("ui.make_wallet"), Btn::Primary)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.wallet_do("create", cx)
                                        })),
                                ),
                        );
                    }
                    (true, true, true) => {
                        out = out.child(self.row(self.tr("ui.the_wallet"), "locked"));
                        out = out.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .py_1()
                                .child(
                                    div()
                                        .w(px(ms.base() * 14.0))
                                        .text_size(ms.body())
                                        .child(self.pass_input.clone()),
                                )
                                .child(
                                    btn(&t, ms, "vault-unlock", self.tr("ui.unlock"), Btn::Primary).on_click(
                                        cx.listener(|this, _, _, cx| this.wallet_do("unlock", cx)),
                                    ),
                                ),
                        );
                    }
                    (true, true, false) => {
                        out = out.child(self.row(self.tr("ui.the_wallet"), "open"));
                        out = out.child(self.note(
                            self.tr("settings.wallet_idle"),
                        ));
                        out = out.child(
                            btn(&t, ms, "vault-lock", self.tr("ui.lock_now"), Btn::Secondary).on_click(
                                cx.listener(|this, _, _, cx| this.wallet_do("lock", cx)),
                            ),
                        );
                    }
                }
                if w.proto > 0 {
                    out = out.child(self.row(self.tr("ui.agent_proto"), w.proto.to_string()));
                }
                if let Some(said) = &self.wallet_said {
                    out = out.child(self.note(said.clone()));
                }
                out = out.child(self.head(self.tr("ui.where_secrets")));
                out = out.child(self.note(
                    self.tr("settings.secret_as_typed"),
                ));
                out = out.child(self.tickbox(
                    "Refuse to save a secret as plain text",
                    "The connection window will not save a literal password \u{2014} it has to \
                     come from this wallet, the environment, or a command.",
                    self.look.secrets_only_from_a_source,
                    |this, _, cx| {
                        let _ = this.workspace.update(cx, |w, cx| w.toggle_literal_secrets(cx));
                        cx.notify();
                    },
                    cx,
                ));
            }
            Category::Access => {
                let f = &self.facts;
                if f.access.is_empty() {
                    out = out.child(self.note(
                        self.tr("settings.no_aws"),
                    ));
                }
                for (name, kind, usable, why) in f.access.clone() {
                    let (test_n, imp_n) = (name.clone(), name.clone());
                    out = out.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py(px(1.0))
                            .child(
                                div()
                                    .w(px(ms.base() * 13.0))
                                    .flex_none()
                                    .text_size(ms.small())
                                    .text_color(t.muted())
                                    .child(SharedString::from(name.clone())),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(ms.small())
                                    .text_color(if usable { t.fg() } else { t.warning() })
                                    .child(SharedString::from(if usable {
                                        format!("{kind} \u{2014} usable")
                                    } else {
                                        format!("{kind} \u{2014} {why}")
                                    })),
                            )
                            // TEST asks the engine to resolve it for real: a
                            // provider that reads as usable and fails when
                            // picked is the thing this page exists to prevent.
                            .children(usable.then(|| {
                                btn(&t, ms, format!("acc-test-{name}"), self.tr("ui.test"), Btn::Secondary)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let n = test_n.clone();
                                        let _ = this
                                            .workspace
                                            .update(cx, |w, _| w.access_test(&n));
                                    }))
                            }))
                            // IMPORT stores the profile's NAME, never its key.
                            .children(usable.then(|| {
                                btn(&t, ms, format!("acc-imp-{name}"), self.tr("ui.import"), Btn::Link)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let n = imp_n.clone();
                                        let _ = this
                                            .workspace
                                            .update(cx, |w, _| w.access_import(&n));
                                    }))
                            })),
                    );
                }
                out = out.child(self.note(
                    self.tr("settings.aws_whole_list"),
                ));
            }
            Category::Mcp => {
                let f = &self.facts;
                if f.approvals.is_empty() {
                    out = out.child(self.note(self.tr("ui.nothing_waiting")));
                }
                for (id, statement, state) in &f.approvals {
                    out = out.child(self.row(format!("{id} \u{2014} {state}"), statement.clone()));
                }
                out = out.child(self.note(
                    self.tr("settings.mcp_same_engine"),
                ));
                out = out.child(self.note(self.tr("ui.decide_not_built")));
            }
            Category::Hooks => {
                out = out.child(self.note(
                    self.tr("settings.hooks_what"),
                ));
                out = out.child(self.note(
                    self.tr("settings.hooks_env"),
                ));
                out = out.child(self.note(
                    self.tr("settings.hooks_per_conn"),
                ));
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
        // Not a magic number: the point is that `all()` and the enum agree,
        // and a page added without a line here would be a page the search
        // never sees.
        assert_eq!(all.len(), 12);

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
        assert_eq!(find("").len(), all.len());
        assert!(find("zzzz-not-a-thing").is_empty());

        // Every category carries words to be found by. One that does not is one
        // nobody can reach except by reading the whole list.
        for c in all {
            assert!(!c.keywords().is_empty(), "{} has nothing to search on", c.title());
            assert!(!c.blurb().is_empty(), "{} does not say what it is for", c.title());
        }
    }

    /// Every default in the table is a keystroke gpui can parse.
    ///
    /// The test that used to live here compared a hand-written table against
    /// the bindings' own file, because the two were written out twice. They are
    /// one table now, so that test has nothing to compare — and what is left
    /// worth checking is that a default cannot take the window down:
    /// `KeyBinding::new` PANICS on a keystroke it cannot parse, so a typo in
    /// the table would be a window that does not open.
    #[test]
    fn every_default_key_parses() {
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for (group, label, name, key) in crate::app::KEYMAP {
            assert!(
                crate::app::valid_keystroke(key),
                "{name} ({label}) has {key:?}, which gpui cannot parse"
            );
            assert!(!group.is_empty() && !label.is_empty(), "{name} has no label");
            assert!(name.starts_with("lode::"), "{name} is not an action name");
            // And no two rows CLASH by default: two actions on one key is a key
            // that does whichever gpui resolves first.
            if let Some((other, _)) = seen.iter().find(|(_, k)| k == key) {
                panic!("{name} and {other} both default to {key:?}");
            }
            seen.push((name, key));
        }
        assert!(seen.len() > 20, "only {} changeable keys?", seen.len());
    }

    /// The wallet page is found by the words somebody would actually type.
    ///
    /// "passphrase" already finds the tunnels page, so the test is that it
    /// finds BOTH — a search that stops at the first match sends somebody to
    /// the page about an SSH key when they were looking for the wallet.
    #[test]
    fn the_wallet_page_is_findable() {
        let hits = |needle: &str| -> Vec<&'static str> {
            let n = needle.to_lowercase();
            Category::all()
                .iter()
                .filter(|c| {
                    c.title().to_lowercase().contains(&n)
                        || c.keywords().contains(&n)
                        || c.blurb().to_lowercase().contains(&n)
                })
                .map(|c| c.title())
                .collect()
        };
        assert!(hits("wallet").contains(&"Keyring"), "{:?}", hits("wallet"));
        assert!(hits("unlock").contains(&"Keyring"));
        assert!(hits("vault").contains(&"Keyring"));
        let pass = hits("passphrase");
        assert!(pass.contains(&"Keyring"), "{pass:?}");
        assert!(pass.contains(&"SSH tunnels"), "{pass:?}");
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
