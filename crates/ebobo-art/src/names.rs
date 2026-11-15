//! The names database: which game a CRC is, and where its pictures are. Owner: names slice.
//!
//! The database is the No-Intro list as libretro publishes it (one text file per system in
//! `libretro/libretro-database`, `metadat/no-intro/`), and the pictures are the libretro
//! thumbnails collection, named after the same game names.

use crate::System;

/// CRC-32 → proper game name, for one system.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    _todo: (),
}

impl Names {
    /// Reads a database file as downloaded from [`database_url`]. Entries it cannot read are
    /// skipped; garbage gives an empty list.
    pub fn parse(_text: &str) -> Names {
        todo!("owner: names slice")
    }

    /// Reads the compact form [`Names::to_cache`] wrote.
    pub fn from_cache(_text: &str) -> Names {
        todo!("owner: names slice")
    }

    /// A compact text form for the disk cache: one `crc<TAB>name` line per game.
    pub fn to_cache(&self) -> String {
        todo!("owner: names slice")
    }

    /// The game with this CRC-32 (as the library computes it: without a copier or iNES
    /// header).
    pub fn get(&self, _crc32: u32) -> Option<&str> {
        todo!("owner: names slice")
    }

    pub fn len(&self) -> usize {
        todo!("owner: names slice")
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Where the names database of `system` is downloaded from.
pub fn database_url(_system: System) -> String {
    todo!("owner: names slice")
}

/// Where the box art of the game `name` (a name from the database) is downloaded from: the
/// collection's own file-name rule applied (the characters it replaces with `_`) and the URL
/// percent-encoded.
pub fn box_art_url(_system: System, _name: &str) -> String {
    todo!("owner: names slice")
}

/// A name as a card shows it: without the trailing groups in brackets ("(Europe)", "(Rev
/// 1)", "[b]"), an article moved to the end brought back to the front ("Legend of Zelda,
/// The" → "The Legend of Zelda").
pub fn display_name(_name: &str) -> String {
    todo!("owner: names slice")
}
