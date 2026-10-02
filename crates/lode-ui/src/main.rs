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
mod export;
mod form;
mod input;
mod session;
mod theme;
mod workspace;

use gpui::{px, size, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};

fn main() {
    Application::new().run(|cx: &mut gpui::App| {
        // The field needs its keys before any window exists.
        input::bind_keys(cx);
        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
        cx.open_window(
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
        cx.activate(true);
    });
}
