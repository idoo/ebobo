//! Pure helpers of the app loop: region hint, frame pacing, save naming, config merge.

use std::time::{Duration, Instant};

use ebobo_core::Region;

use crate::config::Config;

/// Region override for solo play: PAL when the file name says Europe and the header does not.
/// (The header cannot say "unknown", so an NTSC header with a European name is treated as PAL.)
pub fn region_hint(name: &str, header: Region) -> Option<Region> {
    if header != Region::Ntsc {
        return None;
    }
    let upper = name.to_uppercase();
    ["(E)", "(EUROPE)", "(PAL)"].iter().any(|m| upper.contains(m)).then_some(Region::Pal)
}

/// Pacing stretch: 2 % per frame we are ahead of the slowest peer (max 10 %).
pub fn frame_stretch(frames_ahead: i32) -> f64 {
    (0.02 * frames_ahead.max(0) as f64).min(0.10)
}

/// Frame period at `fps`, stretched by `frame_stretch(frames_ahead)`.
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
/// dropped. `crc32` is appended when the plain name is already taken by another game.
pub fn received_rom_file_name(name: &str, crc32: Option<u32>) -> String {
    let stem: String = name
        .strip_suffix(".nes")
        .unwrap_or(name)
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':') { '_' } else { c })
        .take(80)
        .collect();
    let stem = stem.trim().trim_start_matches('.');
    let stem = if stem.is_empty() { "game" } else { stem };
    match crc32 {
        Some(crc) => format!("{stem}-{crc:08x}.nes"),
        None => format!("{stem}.nes"),
    }
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
        assert_eq!(region_hint("Excitebike (E)", Region::Pal), None);
        assert_eq!(region_hint("Eggs (Eu)", Region::Ntsc), None);
    }

    #[test]
    fn app_pacing_stretch() {
        let base = frame_period(60.0, 0);
        assert!((base.as_secs_f64() - 1.0 / 60.0).abs() < 1e-9);
        assert_eq!(frame_period(60.0, -3), base);
        assert!((frame_period(60.0, 2).as_secs_f64() - 1.04 / 60.0).abs() < 1e-9);
        assert!((frame_period(60.0, 5).as_secs_f64() - 1.10 / 60.0).abs() < 1e-9);
        assert_eq!(frame_period(60.0, 40), frame_period(60.0, 5));
        assert!((frame_period(50.0, 1).as_secs_f64() - 1.02 / 50.0).abs() < 1e-9);
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
        assert_eq!(received_rom_file_name("Game (USA).nes", None), "Game (USA).nes");
        assert_eq!(received_rom_file_name("../../.ssh/authorized_keys", None), "_.._.ssh_authorized_keys.nes");
        assert_eq!(received_rom_file_name("a\\b\0c", None), "a_b_c.nes");
        assert_eq!(received_rom_file_name("..", Some(0xAB)), "game-000000ab.nes");
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
