//! ebobo: NES emulator with online play for 2-4 friends (macOS, Steam Deck / Linux).

mod app;
mod audio;
mod canvas;
mod cards;
mod config;
mod crt;
#[allow(dead_code)]
mod enhance;
mod i18n;
mod input;
mod library;
mod osk;
mod ui;
mod video;

fn main() {
    std::process::exit(app::run(std::env::args().skip(1).collect()));
}
