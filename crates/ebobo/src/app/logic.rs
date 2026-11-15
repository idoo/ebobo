//! Pure helpers of the app loop: region hint, frame pacing, save naming, config merge.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ebobo_console::Region;
use ebobo_console::nes::pad;
use ebobo_net::{INPUT_TURBO_A, INPUT_TURBO_B};

use crate::config::Config;

/// Region override for solo play from the file name, `None` = keep the header's region.
/// The tags `(Japan)`, `(USA)` and `(Europe)` win over the header for any system when exactly
/// one of them is in the name (a NES machine runs `Japan` as NTSC). Otherwise: PAL when the
/// name says Europe (`(E)`, `(Europe)`, `(PAL)`) and the header says NTSC (the NES header
/// cannot say "unknown", so an NTSC header with a European name is treated as PAL).
pub fn region_hint(name: &str, header: Region) -> Option<Region> {
    let upper = name.to_uppercase();
    let tags = [("(JAPAN)", Region::Japan), ("(USA)", Region::Ntsc), ("(EUROPE)", Region::Pal)];
    let mut found = tags.iter().filter(|(tag, _)| upper.contains(tag));
    if let (Some(&(_, region)), None) = (found.next(), found.next()) {
        return (region != header).then_some(region);
    }
    if header != Region::Ntsc {
        return None;
    }
    ["(E)", "(EUROPE)", "(PAL)"].iter().any(|m| upper.contains(m)).then_some(Region::Pal)
}

/// Pacing stretch, two-sided: +2 % per frame we are ahead of the slowest peer (max +10 %),
/// -1 % per frame we are behind every peer (max -3 %, small enough not to be heard in the
/// audio pitch), 0 when level.
pub fn frame_stretch(frames_ahead: i32) -> f64 {
    let n = frames_ahead as f64;
    if frames_ahead > 0 {
        (0.02 * n).min(0.10)
    } else {
        (0.01 * n).max(-0.03)
    }
}

/// Local netplay input word: bits 0-15 = the pads of every seat merged (all local players
/// drive one netplay pad), plus the turbo buttons as held bits. Turbo never becomes A/B
/// presses here: every peer expands the held bits from the simulated frame number, so a held
/// turbo button is a constant word that "repeat the last input" predicts right.
pub fn net_input(pads: &[u16; 4], turbo: &[u8; 4]) -> u32 {
    let held = pads.iter().fold(0u16, |acc, p| acc | p);
    let turbo = turbo.iter().fold(0u8, |acc, t| acc | t);
    let mut word = u32::from(held);
    if turbo & pad::A != 0 {
        word |= INPUT_TURBO_A;
    }
    if turbo & pad::B != 0 {
        word |= INPUT_TURBO_B;
    }
    word
}

/// Frame period at `fps`, stretched (ahead) or shortened (behind) by `frame_stretch(frames_ahead)`.
pub fn frame_period(fps: f64, frames_ahead: i32) -> Duration {
    Duration::from_secs_f64((1.0 + frame_stretch(frames_ahead)) / fps)
}

/// Next frame deadline; when more than 3 frames late, the debt is dropped (no catch-up burst).
pub fn next_deadline(prev: Instant, now: Instant, period: Duration) -> Instant {
    let next = prev + period;
    if now.saturating_duration_since(next) > period * 3 {
        now
    } else {
        next
    }
}

/// `<stem>-<crc32 hex>.sav`; path separators in the stem are replaced.
pub fn save_file_name(stem: &str, crc32: u32) -> String {
    let stem: String = stem.chars().map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect();
    format!("{stem}-{crc32:08x}.sav")
}

/// File name for a ROM a netplay host sent us. The name comes from another machine, so none
/// of it may act as a path: separators and control characters are replaced and leading dots
/// dropped.
pub fn received_rom_file_name(name: &str) -> String {
    let stem: String = name
        .strip_suffix(".nes")
        .unwrap_or(name)
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':') { '_' } else { c })
        .take(80)
        .collect();
    let stem = stem.trim().trim_start_matches('.');
    let stem = if stem.is_empty() { "game" } else { stem };
    format!("{stem}.nes")
}

/// Where a received ROM is kept: `<roms_dir>/<crc32 as 8 hex>/<clean name>.nes`. One folder
/// per CRC lets a newer build of a game sit beside the old one under the host's own name, so
/// the library lists both by that name (dated) instead of a `<name>-<crc>` row.
pub fn received_rom_path(roms_dir: &Path, crc32: u32, name: &str) -> PathBuf {
    roms_dir.join(format!("{crc32:08x}")).join(received_rom_file_name(name))
}

/// Copies onto `saved` (the on-disk config) only the fields that changed from `before` to
/// `after`, so command-line overrides held in memory are never written to disk.
pub fn merge_changes(saved: &mut Config, before: &Config, after: &Config) {
    macro_rules! take {
        ($($f:ident),*) => {$(
            if before.$f != after.$f {
                saved.$f = after.$f.clone();
            }
        )*};
    }
    take!(nickname, server, rom_dirs, fullscreen, scale, screen, effect, input_delay, force_relay, direct_port, last_join_addr, lang, volume, ff_toggle, sprite_limit);
}

/// Fast-forward state after one tick. Hold mode: on while the button is held. Toggle mode:
/// each press (rising edge) flips `on`.
pub fn fast_forward(toggle: bool, held: bool, was_held: bool, on: bool) -> bool {
    if toggle {
        on ^ (held && !was_held)
    } else {
        held
    }
}

/// Screenshot file name: `<rom stem>-<YYYYMMDD-HHMMSS>.png` (UTC), separators replaced.
pub fn screenshot_file_name(stem: &str, unix_secs: u64) -> String {
    let stem: String = stem.chars().map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect();
    let days = (unix_secs / 86_400) as i64;
    let secs = unix_secs % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{stem}-{y:04}{m:02}{d:02}-{:02}{:02}{:02}.png", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Nearest-neighbour upscale of `w`x`h` RGBA8 pixels by `k` into `out` (resized to fit).
pub fn scale_nearest(rgba: &[u8], w: usize, h: usize, k: usize, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(w * h * 4 * k * k);
    for row in rgba.chunks_exact(w * 4).take(h) {
        let start = out.len();
        for px in row.chunks_exact(4) {
            for _ in 0..k {
                out.extend_from_slice(px);
            }
        }
        for _ in 1..k {
            out.extend_from_within(start..start + w * 4 * k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_region_hint() {
        assert_eq!(region_hint("Super Mario Bros. (E)", Region::Ntsc), Some(Region::Pal));
        assert_eq!(region_hint("Tetris (Europe) (Rev 1)", Region::Ntsc), Some(Region::Pal));
        assert_eq!(region_hint("Elite (pal)", Region::Ntsc), Some(Region::Pal));
        assert_eq!(region_hint("Contra (U)", Region::Ntsc), None);
        assert_eq!(region_hint("Eggs (Eu)", Region::Ntsc), None);
        // A single (Japan) / (USA) / (Europe) tag wins over the header, for any system.
        assert_eq!(region_hint("Sonic (Japan)", Region::Ntsc), Some(Region::Japan));
        assert_eq!(region_hint("Sonic (Japan)", Region::Pal), Some(Region::Japan));
        assert_eq!(region_hint("Sonic (Japan)", Region::Japan), None);
        assert_eq!(region_hint("Sonic (USA)", Region::Pal), Some(Region::Ntsc));
        assert_eq!(region_hint("Sonic (usa)", Region::Japan), Some(Region::Ntsc));
        assert_eq!(region_hint("Sonic (Europe)", Region::Japan), Some(Region::Pal));
        assert_eq!(region_hint("Sonic (Europe)", Region::Pal), None);
        // Two tags: no single winner, today's rule.
        assert_eq!(region_hint("Sonic (USA) (Europe)", Region::Ntsc), Some(Region::Pal));
        assert_eq!(region_hint("Sonic (Japan) (USA)", Region::Pal), None);
        assert_eq!(region_hint("Sonic (Japan) (USA)", Region::Ntsc), None);
    }

    #[test]
    fn app_pacing_stretch() {
        let base = frame_period(60.0, 0);
        assert!((base.as_secs_f64() - 1.0 / 60.0).abs() < 1e-9);
        assert!((frame_period(60.0, -1).as_secs_f64() - 0.99 / 60.0).abs() < 1e-9);
        assert!((frame_period(60.0, -3).as_secs_f64() - 0.97 / 60.0).abs() < 1e-9);
        assert_eq!(frame_period(60.0, -40), frame_period(60.0, -3));
        assert_eq!(frame_stretch(0), 0.0);
        assert!((frame_period(60.0, 2).as_secs_f64() - 1.04 / 60.0).abs() < 1e-9);
        assert!((frame_period(60.0, 5).as_secs_f64() - 1.10 / 60.0).abs() < 1e-9);
        assert_eq!(frame_period(60.0, 40), frame_period(60.0, 5));
        assert!((frame_period(50.0, 1).as_secs_f64() - 1.02 / 50.0).abs() < 1e-9);
    }

    #[test]
    fn app_net_input_sends_turbo_as_held_bits() {
        assert_eq!(net_input(&[0; 4], &[0; 4]), 0);
        // Every seat merges into the pad bits.
        let w = |bits: u8| u16::from(bits);
        assert_eq!(net_input(&[w(pad::A), w(pad::LEFT), 0, w(pad::START)], &[0; 4]), u32::from(pad::A | pad::LEFT | pad::START));
        // Turbo from any seat sets its own bit and never an A/B press.
        assert_eq!(net_input(&[0; 4], &[pad::A, 0, 0, 0]), INPUT_TURBO_A);
        assert_eq!(net_input(&[0; 4], &[0, 0, pad::B, 0]), INPUT_TURBO_B);
        assert_eq!(net_input(&[w(pad::UP), 0, 0, 0], &[pad::B, pad::A, 0, 0]), u32::from(pad::UP) | INPUT_TURBO_A | INPUT_TURBO_B);
        // Plain A next to turbo B stays plain A.
        assert_eq!(net_input(&[w(pad::A), 0, 0, 0], &[pad::B, 0, 0, 0]), u32::from(pad::A) | INPUT_TURBO_B);
        // Only A/B have turbo, and the word never touches the reset bit.
        assert_eq!(net_input(&[0xFFFF; 4], &[0xFF; 4]), 0xFFFF | INPUT_TURBO_A | INPUT_TURBO_B);
        assert_eq!(net_input(&[0xFFFF; 4], &[0xFF; 4]) & ebobo_net::INPUT_RESET, 0);
    }

    #[test]
    fn app_pacing_debt_dropped() {
        let t0 = Instant::now();
        let p = Duration::from_millis(16);
        assert_eq!(next_deadline(t0, t0, p), t0 + p);
        // Slightly late: keep the schedule (catch up).
        assert_eq!(next_deadline(t0, t0 + p * 3, p), t0 + p);
        // More than 3 frames late: restart from now.
        let late = t0 + p * 10;
        assert_eq!(next_deadline(t0, late, p), late);
    }

    #[test]
    fn app_save_naming() {
        assert_eq!(save_file_name("Zelda (U)", 0x0123abcd), "Zelda (U)-0123abcd.sav");
        assert_eq!(save_file_name("a/b:c", 0xFFFF_FFFF), "a_b_c-ffffffff.sav");
    }

    #[test]
    fn app_received_rom_name_stays_inside_the_roms_dir() {
        assert_eq!(received_rom_file_name("Game (USA).nes"), "Game (USA).nes");
        assert_eq!(received_rom_file_name("Game (USA)"), "Game (USA).nes");
        assert_eq!(received_rom_file_name("../../.ssh/authorized_keys"), "_.._.ssh_authorized_keys.nes");
        assert_eq!(received_rom_file_name("a\\b\0c"), "a_b_c.nes");
        assert_eq!(received_rom_file_name("C:evil"), "C_evil.nes");
        assert_eq!(received_rom_file_name(".."), "game.nes");
        assert_eq!(received_rom_file_name("  .hidden "), "hidden.nes");
        assert_eq!(received_rom_file_name(&"x".repeat(200)), format!("{}.nes", "x".repeat(80)));
    }

    #[test]
    fn app_received_rom_path_is_one_folder_per_crc() {
        let roms = Path::new("/data/ebobo/roms");
        assert_eq!(received_rom_path(roms, 0xAB, "Chip 'n Dale 4P (Europe)"), roms.join("000000ab/Chip 'n Dale 4P (Europe).nes"));
        let hostile = ["../../.ssh/authorized_keys", "/etc/passwd", "..", ".", "", "a/../../b", "\\..\\..\\x", "x\n/../y", "C:\\Windows"];
        for name in hostile {
            let path = received_rom_path(roms, 0xDEAD_BEEF, name);
            assert_eq!(path.parent(), Some(roms.join("deadbeef").as_path()), "{name:?} -> {}", path.display());
            assert!(path.components().all(|c| matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_))), "{name:?}");
            assert!(path.extension().is_some_and(|e| e == "nes"), "{name:?}");
        }
    }

    #[test]
    fn app_config_merge_keeps_cli_overrides_out() {
        let saved0 = Config::default();
        let mut before = saved0.clone();
        before.force_relay = true; // --relay
        before.server = "http://cli".into(); // --server
        let mut after = before.clone();
        after.volume = 0.3; // user edit in settings
        let mut saved = saved0.clone();
        merge_changes(&mut saved, &before, &after);
        assert_eq!(saved.volume, 0.3);
        assert!(!saved.force_relay);
        assert_eq!(saved.server, saved0.server);
    }

    #[test]
    fn app_fast_forward_hold_and_toggle() {
        // Hold: follows the button.
        assert!(fast_forward(false, true, false, false));
        assert!(!fast_forward(false, false, true, true));
        // Toggle: flips on the press only, stays while held and after release.
        let mut on = false;
        let mut was = false;
        let mut seq = Vec::new();
        for held in [true, true, false, false, true, false] {
            on = fast_forward(true, held, was, on);
            was = held;
            seq.push(on);
        }
        assert_eq!(seq, [true, true, true, true, false, false]);
    }

    #[test]
    fn app_sprite_limit_auto_rule_and_config_round_trip() {
        use crate::config::SpriteLimit;
        assert!(SpriteLimit::Auto.applies(false), "auto: 1-2 players keep the NES limit");
        assert!(!SpriteLimit::Auto.applies(true), "auto: Four Score removes it");
        assert!(SpriteLimit::Hardware.applies(true));
        assert!(!SpriteLimit::Off.applies(false));
        for v in [SpriteLimit::Auto, SpriteLimit::Hardware, SpriteLimit::Off] {
            let mut c = Config::default();
            c.sprite_limit = v;
            let back: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
            assert_eq!(back.sprite_limit, v);
        }
        // Old config files without the key get Auto.
        let old: Config = toml::from_str("volume = 0.5").unwrap();
        assert_eq!(old.sprite_limit, SpriteLimit::Auto);
        let (before, mut after, mut saved) = (Config::default(), Config::default(), Config::default());
        after.sprite_limit = SpriteLimit::Off;
        merge_changes(&mut saved, &before, &after);
        assert_eq!(saved.sprite_limit, SpriteLimit::Off);
    }

    #[test]
    fn app_screenshot_name_and_scale() {
        assert_eq!(screenshot_file_name("a/b", 0), "a_b-19700101-000000.png");
        // 2026-11-13 17:48:49 UTC
        assert_eq!(screenshot_file_name("x", 1_794_592_129), "x-20261113-174849.png");
        let px = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut out = Vec::new();
        scale_nearest(&px, 2, 1, 3, &mut out);
        assert_eq!(out.len(), 6 * 3 * 4);
        let row: Vec<u8> = [[1, 2, 3, 4]; 3].into_iter().chain([[5, 6, 7, 8]; 3]).flatten().collect();
        assert_eq!(&out[..24], row.as_slice());
        assert_eq!(&out[24..48], row.as_slice());
        assert_eq!(&out[48..], row.as_slice());
    }
}
