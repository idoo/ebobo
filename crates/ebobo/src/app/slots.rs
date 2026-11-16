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

/// A game's name as it goes into a file name: path separators are replaced.
pub fn clean_stem(stem: &str) -> String {
    stem.chars().map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect()
}

/// `dir/<stem>-<crc32 hex>.<slot>.state`; path separators in the stem are replaced.
pub fn path(dir: &Path, stem: &str, crc32: u32, slot: u8) -> PathBuf {
    dir.join(format!("{}-{crc32:08x}.{}.state", clean_stem(stem), slot + 1))
}

/// Writes the state atomically: a temp file next to it, then rename over the old one.
pub fn save(console: &Console, path: &Path) -> io::Result<()> {
    write(path, &console.save_state())
}

/// Writes a file atomically (temp file `<path>.tmp`, then a rename over the old file),
/// creating the directory; a crash mid-write never leaves a cut file. Save states, battery
/// saves and received ROMs all go through it.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path)).inspect_err(|_| {
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

/// Size of a slot's picture, pixels.
pub const THUMB_W: usize = 64;
pub const THUMB_H: usize = 48;

/// The picture file of a slot: the state's name with `thumb` in place of `state`.
pub fn thumb_path(state_path: &Path) -> PathBuf {
    state_path.with_extension("thumb")
}

/// Shrinks a `w`x`h` straight-RGBA frame to `THUMB_W`x`THUMB_H` opaque RGBA: every thumbnail
/// pixel is the average of the block of frame pixels it covers. A frame with no pixels (or a
/// buffer shorter than `w * h * 4`) gives a black picture.
pub fn shrink(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(THUMB_W * THUMB_H * 4);
    let usable = w > 0 && h > 0 && rgba.len() >= w * h * 4;
    for oy in 0..THUMB_H {
        for ox in 0..THUMB_W {
            let mut sum = [0u32; 3];
            let mut n = 0u32;
            if usable {
                // The block never is empty, also when the frame is smaller than the thumbnail.
                let (x0, y0) = (ox * w / THUMB_W, oy * h / THUMB_H);
                let x1 = ((ox + 1) * w / THUMB_W).max(x0 + 1);
                let y1 = ((oy + 1) * h / THUMB_H).max(y0 + 1);
                for y in y0..y1 {
                    for p in rgba[(y * w + x0) * 4..(y * w + x1) * 4].chunks_exact(4) {
                        sum[0] += p[0] as u32;
                        sum[1] += p[1] as u32;
                        sum[2] += p[2] as u32;
                        n += 1;
                    }
                }
            }
            let n = n.max(1);
            out.extend_from_slice(&[(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8, 255]);
        }
    }
    out
}

/// Writes a slot's picture (atomically, like the state).
pub fn save_thumb(path: &Path, thumb: &[u8]) -> io::Result<()> {
    write(path, thumb)
}

/// A slot's picture; `None` when there is none or the file is not a whole picture.
pub fn load_thumb(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok().filter(|b| b.len() == THUMB_W * THUMB_H * 4)
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

    #[test]
    fn app_slot_thumb_shrinks_both_frame_sizes() {
        for (w, h) in [(256usize, 240usize), (320, 224)] {
            let mut frame = Vec::with_capacity(w * h * 4);
            for _ in 0..h {
                for x in 0..w {
                    frame.extend_from_slice(if x < w / 2 { &[255, 0, 0, 7] } else { &[0, 0, 255, 7] });
                }
            }
            let t = shrink(&frame, w, h);
            assert_eq!(t.len(), THUMB_W * THUMB_H * 4, "{w}x{h}");
            for y in 0..THUMB_H {
                for x in 0..THUMB_W {
                    let i = (y * THUMB_W + x) * 4;
                    let want = if x < THUMB_W / 2 { [255, 0, 0, 255] } else { [0, 0, 255, 255] };
                    assert_eq!(t[i..i + 4], want, "{w}x{h} at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn app_slot_thumb_file_must_be_whole() {
        let dir = tmp_dir("thumb");
        let p = thumb_path(&path(&dir, "game", 0xAB, 0));
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "game-000000ab.1.thumb");
        assert!(load_thumb(&p).is_none());
        let thumb = shrink(&[9; 8 * 8 * 4], 8, 8);
        save_thumb(&p, &thumb).unwrap();
        assert_eq!(load_thumb(&p), Some(thumb.clone()));
        std::fs::write(&p, &thumb[..100]).unwrap();
        assert!(load_thumb(&p).is_none(), "a cut file is no picture");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
