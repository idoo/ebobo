//! Online saves on disk, a pair per ROM in `<data dir>/netplay/`: `<crc32 hex>.last.state`,
//! the last moment of the last online session (rewritten every 5 s once a session ran a
//! minute, and when the game is left), and `<crc32 hex>.save.state`, the newest "Save for
//! everyone". Same bytes as solo save states. The saves and the 5 s checkpoints are identical
//! on every peer of a game; the moment of leaving is each player's own.

use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ebobo_core::Nes;
use ebobo_net::{Resume, ResumeKind};
use sha2::{Digest, Sha256};

use super::cli::ResumeFrom;
use super::slots;
use crate::config;

/// Frames a session must have advanced before its checkpoints replace the last moment (a
/// minute at 60 fps): a game started by mistake and left at once keeps the real one.
pub const LAST_AFTER_FRAMES: u64 = 3600;

/// `<data dir>/netplay`.
pub fn dir() -> PathBuf {
    config::data_dir().join("netplay")
}

/// `last` / `save`: the file name part and the word on stdout.
pub fn kind_name(kind: ResumeKind) -> &'static str {
    match kind {
        ResumeKind::Last => "last",
        ResumeKind::Save => "save",
    }
}

/// `dir/<crc32 hex>.last.state` or `.save.state`.
pub fn path(dir: &Path, crc32: u32, kind: ResumeKind) -> PathBuf {
    dir.join(format!("{crc32:08x}.{}.state", kind_name(kind)))
}

/// Writes one of the two files atomically.
pub fn write(dir: &Path, crc32: u32, kind: ResumeKind, bytes: &[u8]) -> io::Result<()> {
    slots::write(&path(dir, crc32, kind), bytes)
}

/// What a host can start a game from: the last moment, then the save, each only when its
/// file loads into `scratch` (a fresh `Nes` of the ROM and region the game will run; it is
/// left in whatever state loaded last). `saved_at` is the file's modification time.
pub fn candidates(dir: &Path, scratch: &mut Nes) -> Vec<Resume> {
    let crc32 = scratch.info().crc32;
    let mut out = Vec::new();
    for kind in [ResumeKind::Last, ResumeKind::Save] {
        let path = path(dir, crc32, kind);
        let Ok(state) = std::fs::read(&path) else { continue };
        if scratch.load_state(&state).is_err() {
            continue;
        }
        let saved_at = slots::saved_at(&path)
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        out.push(Resume { kind, saved_at, state });
    }
    out
}

/// Index into `candidates` the host starts from (`None` = a new game). Without `--resume`
/// the first one, so a game played before continues from its last moment; `--resume last`
/// or `save` falls back to a new game when that file is missing.
pub fn pick(candidates: &[Resume], from: Option<ResumeFrom>) -> Option<usize> {
    let kind = match from {
        None => return (!candidates.is_empty()).then_some(0),
        Some(ResumeFrom::New) => return None,
        Some(ResumeFrom::Last) => ResumeKind::Last,
        Some(ResumeFrom::Save) => ResumeKind::Save,
    };
    candidates.iter().position(|c| c.kind == kind)
}

/// Whether a checkpoint taken after `advanced` frames of this session may replace the last
/// moment.
pub fn last_due(advanced: u64) -> bool {
    advanced >= LAST_AFTER_FRAMES
}

/// First 16 hex digits of the SHA-256 of `bytes` (stdout lines for automated checks).
pub fn sha16(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16 KiB NROM image that counts in RAM forever; `tag` changes its crc32.
    fn rom(tag: u8) -> Vec<u8> {
        let mut rom = vec![0u8; 16 + 0x4000 + 0x2000];
        rom[..8].copy_from_slice(&[b'N', b'E', b'S', 0x1A, 1, 1, 0, 0]);
        let prg = &mut rom[16..16 + 0x4000];
        prg[..10].copy_from_slice(&[0xE6, 0x10, 0xD0, 0xFC, 0xE6, 0x11, 0x4C, 0x00, 0x80, 0x40]);
        prg[0x100] = tag;
        prg[0x3FFA..].copy_from_slice(&[0x09, 0x80, 0x00, 0x80, 0x09, 0x80]);
        rom
    }

    fn nes(tag: u8, frames: u32) -> Nes {
        let mut nes = Nes::new(&rom(tag), None).unwrap();
        for _ in 0..frames {
            nes.run_frame([0; 4]);
        }
        nes
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ebobo-netsave-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn netsave_candidates_last_first_and_only_loadable() {
        let dir = tmp_dir("candidates");
        let mut scratch = nes(1, 0);
        assert!(candidates(&dir, &mut scratch).is_empty(), "missing dir: nothing to continue from");

        let crc = scratch.info().crc32;
        let (saved, last) = (nes(1, 5), nes(1, 9));
        write(&dir, crc, ResumeKind::Save, &saved.save_state()).unwrap();
        write(&dir, crc, ResumeKind::Last, &last.save_state()).unwrap();
        assert_eq!(path(&dir, crc, ResumeKind::Last).file_name().unwrap().to_str().unwrap(), format!("{crc:08x}.last.state"));
        assert!(!dir.join(format!("{crc:08x}.last.state.tmp")).exists());

        let found = candidates(&dir, &mut scratch);
        assert_eq!(found.iter().map(|c| c.kind).collect::<Vec<_>>(), [ResumeKind::Last, ResumeKind::Save]);
        assert_eq!(found[0].state, last.save_state());
        assert_eq!(found[1].state, saved.save_state());
        let mtime = slots::saved_at(&path(&dir, crc, ResumeKind::Save)).unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(found[1].saved_at, mtime);
        assert!(mtime > 1_700_000_000);

        // A damaged last moment and another ROM's state under this ROM's name are not offered.
        std::fs::write(path(&dir, crc, ResumeKind::Last), b"EBST garbage").unwrap();
        write(&dir, crc, ResumeKind::Save, &nes(2, 5).save_state()).unwrap();
        assert!(candidates(&dir, &mut scratch).is_empty());
        let mut cut = last.save_state();
        cut.truncate(cut.len() - 40);
        write(&dir, crc, ResumeKind::Last, &cut).unwrap();
        assert!(candidates(&dir, &mut scratch).is_empty());
        write(&dir, crc, ResumeKind::Save, &saved.save_state()).unwrap();
        assert_eq!(candidates(&dir, &mut scratch).iter().map(|c| c.kind).collect::<Vec<_>>(), [ResumeKind::Save]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn netsave_pick_default_and_resume_flag() {
        let c = |kind| Resume { kind, saved_at: 1, state: Vec::new() };
        let both = [c(ResumeKind::Last), c(ResumeKind::Save)];
        let save_only = [c(ResumeKind::Save)];
        assert_eq!(pick(&both, None), Some(0), "continues from the last moment");
        assert_eq!(pick(&save_only, None), Some(0));
        assert_eq!(pick(&[], None), None, "nothing saved: new game");
        assert_eq!(pick(&both, Some(ResumeFrom::New)), None);
        assert_eq!(pick(&both, Some(ResumeFrom::Save)), Some(1));
        assert_eq!(pick(&save_only, Some(ResumeFrom::Last)), None, "missing file: new game");
        assert_eq!(pick(&[], Some(ResumeFrom::Save)), None);
    }

    #[test]
    fn netsave_last_moment_waits_a_minute() {
        assert!(!last_due(0));
        assert!(!last_due(300));
        assert!(!last_due(LAST_AFTER_FRAMES - 1));
        assert!(last_due(LAST_AFTER_FRAMES));
        assert!(last_due(LAST_AFTER_FRAMES * 10));
    }

    #[test]
    fn netsave_sha16() {
        assert_eq!(sha16(b"abc"), "ba7816bf8f01cfea");
    }
}
