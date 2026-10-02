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
mod theme;

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
