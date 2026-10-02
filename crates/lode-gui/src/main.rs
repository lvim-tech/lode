//! lode — the desktop database client.
//!
//! A client of the `loded` engine and nothing else: it spawns the engine as a
//! child process and speaks the same JSON-RPC the Neovim plugin speaks. It holds
//! no driver, opens no socket to a database, and resolves no credential. If it
//! crashes the engine's connections go with it and nothing else; if the engine
//! crashes this window survives and says so.
//!
//! The saved connections, saved queries and history are the ENGINE's, so what
//! is saved here appears in the editor and the other way round.

mod app;
mod export;
mod font;
mod theme;
// The workspace MODEL: live connections, result tabs, background tasks, and the
// rules around them — which view a driver opens in, how a statement is built
// from the engine's template, what a document behind a row is. It is framework
// independent on purpose: the client is moving from egui to GPUI, and these
// rules do not change when the widgets do. Compiled and tested here while the
// egui client is still the one that runs; the GPUI client is its first real
// consumer.
#[allow(dead_code)]
mod workspace;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("lode"),
        ..Default::default()
    };
    eframe::run_native(
        "lode",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
