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
use serde_json::Value;

use crate::app::Workspace;
use crate::input::TextInput;
use crate::metrics::Metrics;
use crate::theme::Theme;

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
            Category::General => "How the window looks and what it remembers.",
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
    tunnels: Vec<(String, Value)>,
    proxies: Vec<(String, Value)>,
    /// Which driver's detail is shown on the Drivers page.
    driver_shown: Option<String>,
    about: Vec<(String, String)>,
}

impl SettingsWindow {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        theme: Theme,
        scale: Metrics,
        drivers: Vec<lode_proto::DriverMeta>,
        tunnels: Vec<(String, Value)>,
        proxies: Vec<(String, Value)>,
        about: Vec<(String, String)>,
        cx: &mut Context<Self>,
    ) -> Self {
        let colors = crate::form::colors_of(&theme);
        let search = cx.new(|cx| TextInput::new(cx, "Search settings…", colors));
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
            driver_shown,
            about,
        }
    }

    pub fn all_inputs(&self) -> Vec<Entity<TextInput>> {
        vec![self.search.clone()]
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
        tunnels: Vec<(String, Value)>,
        proxies: Vec<(String, Value)>,
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
                    .when(on, |d| d.bg(t.selected()).border_l_3().border_color(t.accent()))
                    .when(!on, |d| d.border_l_3().border_color(t.panel()))
                    .hover(|d| d.bg(t.hover()))
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
            .child(div().flex_1().text_size(ms.body()).child(self.search.clone()));

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
                div()
                    .id("settings-close")
                    .px_2()
                    .cursor_pointer()
                    .hover(|d| d.bg(t.hover()))
                    .text_size(ms.small())
                    .text_color(t.muted())
                    .child("Close")
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _ = this.workspace.update(cx, |w, cx| w.close_settings(cx));
                    })),
            );

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(t.base())
            .text_color(t.fg())
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

    fn note(&self, text: impl Into<SharedString>) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        div().py_1().text_size(ms.small()).text_color(t.muted()).child(text.into())
    }

    /// A capability chip. Faded when the driver does not have it, because what
    /// a driver CANNOT do is as much worth seeing as what it can — and a list
    /// that only shows the yeses cannot be scanned for a no.
    fn chip(&self, label: &'static str, on: bool) -> gpui::Div {
        let (t, ms) = (&self.theme, self.scale);
        div()
            .px_1()
            .text_size(ms.tiny())
            .text_color(if on { t.inverted() } else { t.muted() })
            .when(on, |d| d.bg(t.accent()))
            .when(!on, |d| d.border_1().border_color(t.border()))
            .child(label)
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
                        div()
                            .id(SharedString::from(format!("drv-{}", d.kind)))
                            .px_1()
                            .cursor_pointer()
                            .text_size(ms.tiny())
                            .text_color(if on { t.inverted() } else { t.muted() })
                            .when(on, |d2| d2.bg(t.accent()))
                            .hover(|d2| d2.bg(t.hover()))
                            .child(SharedString::from(d.display.clone()))
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
            Category::Tunnels | Category::Proxies => {
                let (rows, what) = if self.category == Category::Tunnels {
                    (&self.tunnels, "tunnel")
                } else {
                    (&self.proxies, "proxy")
                };
                if rows.is_empty() {
                    out = out.child(self.note(format!("No saved {what}s yet.")));
                }
                for (name, spec) in rows {
                    let g = |k: &str| spec.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    let port = spec.get("port").and_then(Value::as_u64).unwrap_or(22);
                    let kind = spec
                        .get("auth")
                        .and_then(|a| a.get("kind"))
                        .and_then(Value::as_str)
                        .or_else(|| spec.get("kind").and_then(Value::as_str))
                        .unwrap_or("agent");
                    out = out.child(self.row(
                        name.clone(),
                        format!("{}@{}:{port} · {kind}", g("user"), g("host")),
                    ));
                }
                out = out.child(self.note(
                    "Edited in their own window — from the palette, or from a connection's \
                     Access tab.",
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
                    Category::General => {
                        "Theme, text and icon sizes, language and JSON indent are still in the \
                         rail's panel. They move here next."
                    }
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
