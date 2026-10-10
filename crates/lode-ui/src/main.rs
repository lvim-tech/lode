//! lode — the desktop database client.
//!
//! A client of the `loded` engine and nothing else: it spawns the engine as a
//! child process and speaks the same JSON-RPC the Neovim plugin speaks. It holds
//! no driver, opens no socket to a database, and resolves no credential.
//!
//! Built on GPUI, Zed's framework — the same one dbflux uses, which is the
//! measure for this client. The engine, the protocol, `lode-proto` and the
//! themes were untouched by that change: splitting the engine out was done
//! precisely so that changing the face would cost the engine nothing, and it
//! cost it nothing.
//!
//! One rule carried over from the previous client and worth repeating: **the UI
//! thread never waits on a database.** Every request is sent and its answer is
//! collected in a later frame. The only blocking call is the first `rpc.hello`,
//! where there is nothing else to do yet.

mod app;
mod board;
mod editor;
mod editor_el;
mod export;
mod form;
mod i18n;
mod input;
mod menu;
mod metrics;
mod session;
mod settings;
mod theme;
mod profile;
mod tip;
mod widgets;
mod workspace;

use gpui::{px, size, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};

fn main() {
    // A window is not the only way to answer "which font, and has it the
    // glyphs". A picture of the rail proves the rail; this prints the numbers
    // the rail is drawn from, and can be run where there is no screen.
    let check_icons = std::env::args().any(|a| a == "--check-icons");
    let check_panels = std::env::args().any(|a| a == "--check-panels");
    Application::new().run(move |cx: &mut gpui::App| {
        if check_panels {
            app::report_panels(cx);
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        if check_icons {
            app::report_icons(cx);
            // `cx.quit()` asks the run loop to stop, and with no window open it
            // did not — the process sat there. Down a terminal that looks fine,
            // because the lines are already on screen; down a PIPE it looks like
            // a hang, because the reader waits for an end that never comes. A
            // diagnostic that hangs in a pipeline is a diagnostic nobody puts in
            // a test, so it ends itself: flush what was printed, then go.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
        // The field needs its keys before any window exists.
        input::bind_keys(cx);
        app::bind_workspace_keys(cx);
        editor_el::bind_editor_keys(cx);
        // From the same scale as everything else, at the default text size:
        // 1288 x 812, which is where the literal 1280 x 820 came from anyway.
        // The session's own size is not known yet — the workspace reads it —
        // so this is the default, and the window manager has the last word.
        let (w, h) = metrics::Metrics::default().window(metrics::Window::Main);
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("lode".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(app::Workspace::new),
        )
        .expect("a window");
        // CLOSING THE WINDOW is also a way to end a running statement, so it
        // asks the same question the disconnect does — and it is the only
        // moment that can be vetoed, which is why it is wired here rather than
        // in `on_app_quit`: a quit handler cannot say no.
        if let Ok(h) = handle.update(cx, |_, window, cx| {
            let entity = cx.entity();
            window.on_window_should_close(cx, move |window, cx| {
                entity.update(cx, |w: &mut app::Workspace, cx| w.may_close(window, cx))
            });
        }) {
            let _ = h;
        }
        cx.activate(true);
    });
}
