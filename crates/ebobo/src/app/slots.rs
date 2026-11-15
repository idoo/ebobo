//! Save-state slots on disk: `<data dir>/states/<rom stem>-<crc32 hex>.<slot>.state`.

use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ebobo_console::{Console, StateError};

use crate::ui::Text;

/// Slots per game (numbered 1..=SLOTS in the UI, 0-based in code).
pub const SLOTS: u8 = 4;

/// Why a slot did not load.
#[derive(Debug)]
pub enum LoadError {
    /// No file in this slot.
    Empty,
    State(StateError),
    Io(io::Error),
}

impl LoadError {
    /// Toast text for the failure.
    pub fn text(&self) -> Text {
        match self {
            LoadError::Empty => Text::NoQuickSave,
            LoadError::State(StateError::WrongRom) => Text::StateWrongRom,
            LoadError::State(StateError::Version(_)) => Text::StateOldVersion,
            LoadError::State(StateError::BadMagic | StateError::Corrupt) | LoadError::Io(_) => Text::StateCorrupt,
        }
    }
}

/// `dir/<stem>-<crc32 hex>.<slot>.state`; path separators in the stem are replaced.
pub fn path(dir: &Path, stem: &str, crc32: u32, slot: u8) -> PathBuf {
    let stem: String = stem.chars().map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect();
    dir.join(format!("{stem}-{crc32:08x}.{}.state", slot + 1))
}

/// Writes the state atomically: a temp file next to it, then rename over the old one.
pub fn save(console: &Console, path: &Path) -> io::Result<()> {
    write(path, &console.save_state())
}

/// Writes save-state bytes atomically (temp file `<path>.tmp`, then a rename over the old
/// file), creating the directory; a crash mid-write never leaves a cut file.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("state.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn load(console: &mut Console, path: &Path) -> Result<(), LoadError> {
    let bytes = std::fs::read(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => LoadError::Empty,
        _ => LoadError::Io(e),
    })?;
    console.load_state(&bytes).map_err(LoadError::State)
}

/// When the slot was last written (`None` = empty).
pub fn saved_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `None` (after a notice) when the test ROMs were not fetched.
    fn nestest() -> Option<Console> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ebobo-core/tests/roms/nestest/nestest.nes");
        let Ok(rom) = std::fs::read(path) else {
            println!("test ROMs missing: run scripts/fetch-test-roms.sh");
            return None;
        };
        Some(Console::new(&rom, None).unwrap())
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-slots-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn app_slot_save_load_roundtrip() {
        let dir = tmp_dir("roundtrip").join("states");
        let Some(mut nes) = nestest() else { return };
        for _ in 0..30 {
            nes.run_frame([0; 4]);
        }
        let crc = nes.crc32();
        let p = path(&dir, "nestest", crc, 2);
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), format!("nestest-{crc:08x}.3.state"));
        assert!(saved_at(&p).is_none());
        save(&nes, &p).unwrap();
        assert!(p.is_file());
        assert!(!p.with_extension("state.tmp").exists());
        assert!(saved_at(&p).is_some());
        let mut fresh = nestest().unwrap();
        assert_ne!(fresh.state_hash(), nes.state_hash());
        load(&mut fresh, &p).unwrap();
        assert_eq!(fresh.state_hash(), nes.state_hash());
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn app_slot_load_errors() {
        let Some(mut nes) = nestest() else { return };
        let dir = tmp_dir("errors");
        std::fs::create_dir_all(&dir).unwrap();
        let missing = load(&mut nes, &dir.join("none.state")).unwrap_err();
        assert!(matches!(missing, LoadError::Empty));
        assert_eq!(missing.text(), Text::NoQuickSave);
        let junk = dir.join("junk.state");
        std::fs::write(&junk, b"not a state").unwrap();
        assert_eq!(load(&mut nes, &junk).unwrap_err().text(), Text::StateCorrupt);
        let mut good = nes.save_state();
        good.truncate(good.len() / 2);
        let cut = dir.join("cut.state");
        std::fs::write(&cut, &good).unwrap();
        assert_eq!(load(&mut nes, &cut).unwrap_err().text(), Text::StateCorrupt);
        assert_eq!(LoadError::State(StateError::WrongRom).text(), Text::StateWrongRom);
        assert_eq!(LoadError::State(StateError::Version(0)).text(), Text::StateOldVersion);
        assert_eq!(LoadError::State(StateError::BadMagic).text(), Text::StateCorrupt);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
