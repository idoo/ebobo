//! ebobo-art: pictures and proper names for the library's game cards.
//!
//! For each game (system + CRC-32) the best picture there is:
//! 1. its box art from the libretro thumbnails collection, found through the No-Intro names
//!    database by CRC (so the file's own name does not matter);
//! 2. without a match (hacks, homebrew), offline, or with downloads switched off: the game's
//!    own title screen, captured by running it for a few seconds.
//!
//! Everything is cached on disk under one folder; nothing here blocks the caller: the work
//! happens on one background thread and [`Art::poll`] takes in what it finished.
//!
//! Module ownership (one owner per file):
//! - `names.rs`: the names database (parse, lookup, where to download it and the pictures).
//! - `image.rs`: PNG in and out, shrinking. `net.rs`: downloading through the system `curl`.
//! - `title.rs`: capturing a title screen from a ROM.
//! - `shelf.rs`: favourites, play history, and the filtered, sorted view of the library.
//! - `lib.rs`: the [`Art`] service: queue, background thread, disk and memory caches.

use std::path::{Path, PathBuf};

pub use ebobo_console::System;

pub mod image;
pub mod names;
pub mod net;
pub mod shelf;
pub mod title;

/// Largest side of a cached picture, in pixels: a card is never drawn larger.
pub const MAX_SIDE: u32 = 320;

/// A picture: `w`×`h` RGBA8888, row by row, alpha not premultiplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// A game: the system and the CRC-32 the library knows it by (`ebobo_console::RomId::crc32`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub system: System,
    pub crc32: u32,
}

/// What a picture shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The box, any proportions (most are upright).
    BoxArt,
    /// The game's own screen, 4:3.
    TitleScreen,
}

/// The pictures and names of the library.
pub struct Art {
    _todo: (),
}

impl Art {
    /// `dir`: the cache folder (created when needed). `online`: downloads are allowed.
    pub fn new(_dir: PathBuf, _online: bool) -> Art {
        todo!("owner: service slice")
    }

    /// Switches downloads on or off (the setting). Off: only what is on disk already, and
    /// title screens.
    pub fn set_online(&mut self, _online: bool) {
        todo!("owner: service slice")
    }

    /// Asks for the picture and name of a game. Cheap and idempotent: the library calls it
    /// for every card on screen, every frame. `rom` is the game's file (a `.zip` works), read
    /// on the background thread when the title screen has to be captured.
    pub fn want(&mut self, _key: Key, _rom: &Path) {
        todo!("owner: service slice")
    }

    /// The picture of a game, once it is ready, and what it shows.
    pub fn get(&self, _key: Key) -> Option<(&Picture, Kind)> {
        todo!("owner: service slice")
    }

    /// The game's proper name from the names database ("Chip 'n Dale - Rescue Rangers
    /// (Europe)"), once known; `None` for a game the database does not have.
    pub fn name(&self, _key: Key) -> Option<&str> {
        todo!("owner: service slice")
    }

    /// Takes in what the background thread finished. True when a picture or a name arrived
    /// since the last call: the library should be redrawn.
    pub fn poll(&mut self) -> bool {
        todo!("owner: service slice")
    }
}
