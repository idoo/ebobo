//! ebobo: NES and Mega Drive emulator with online play for 2-4 friends (macOS, Windows, Steam Deck / Linux).

// A release build on Windows is a windowed program: no console window opens with the game.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod audio;
mod canvas;
mod cards;
mod config;
mod crt;
mod enhance;
mod i18n;
mod input;
mod library;
mod osk;
mod ui;
mod video;
mod whatsnew;

fn main() {
    std::process::exit(app::run(std::env::args().skip(1).collect()));
}
