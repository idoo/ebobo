//! A solo game's moment: the state it was left at, on disk as
//! `<data dir>/moments/<rom stem>-<crc32 hex>.state`. Starting the game continues from it.
//! The bytes are read and written with `slots::load` and `slots::save`.

use std::path::{Path, PathBuf};

use super::slots;
use crate::config;

/// The folder of all moments.
pub fn dir() -> PathBuf {
    config::data_dir().join("moments")
}

/// `dir/<stem>-<crc32 hex>.state`; the stem is cleaned as for save slots.
pub fn path(dir: &Path, stem: &str, crc32: u32) -> PathBuf {
    dir.join(format!("{}-{crc32:08x}.state", slots::clean_stem(stem)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_moment_path_is_one_file_per_game() {
        let dir = Path::new("/data/moments");
        assert_eq!(path(dir, "Contra (U)", 0xAB), Path::new("/data/moments/Contra (U)-000000ab.state"));
        // A name with separators stays one file inside the folder.
        assert_eq!(path(dir, "a/b\\c:d", 0xDEADBEEF), Path::new("/data/moments/a_b_c_d-deadbeef.state"));
        // Two dumps of one title keep separate moments, and no moment is a save slot's file.
        assert_ne!(path(dir, "Contra", 1), path(dir, "Contra", 2));
        for slot in 0..slots::SLOTS {
            assert_ne!(path(dir, "Contra", 1), slots::path(dir, "Contra", 1, slot));
        }
    }
}
