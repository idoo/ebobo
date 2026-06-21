//! ebobo: NES emulator with online play for 2-4 friends (macOS, Steam Deck / Linux).

mod app;
mod audio;
mod canvas;
mod config;
mod i18n;
mod input;
mod library;
mod ui;
mod video;

fn main() {
    std::process::exit(app::run(std::env::args().skip(1).collect()));
}
