//! The sprite limit is a display-only host setting: with it off, emulated state (hash and
//! RAM) must stay identical to the hardware run every frame, including across restores.

mod common;

use ebobo_core::Nes;

const ROMS: &[&str] = &[
    "nestest/nestest.nes",
    "mmc3_test_2/rom_singles/3-A12_clocking.nes",
    "mmc3_test_2/rom_singles/4-scanline_timing.nes",
    "mmc3_test_2/rom_singles/5-MMC3.nes",
    "sprite_overflow_tests/1.Basics.nes",
    "sprite_overflow_tests/3.Timing.nes",
    "sprite_overflow_tests/4.Obscure.nes",
    "sprite_hit_tests_2005.10.05/09.timing_basics.nes",
    "oam_stress/oam_stress.nes",
];

const FRAMES: u32 = 600;

fn check(name: &str, rom: &[u8]) {
    let mut hw = Nes::new(rom, None).unwrap();
    let mut free = Nes::new(rom, None).unwrap();
    free.set_sprite_limit(false);
    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ name.len() as u64;
    for frame in 0..FRAMES {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let p = (rng >> 8) as u8;
        let pads = [p & 0xF0 | ((rng >> 16) as u8 & 0x0C), p, (rng >> 24) as u8, (rng >> 32) as u8];
        hw.run_frame(pads);
        free.run_frame(pads);
        assert_eq!(hw.state_hash(), free.state_hash(), "{name}: state diverged at frame {frame}");
        for a in 0..0x800u16 {
            assert_eq!(hw.peek(a), free.peek(a), "{name}: RAM ${a:04X} differs at frame {frame}");
        }
    }
    // The setting survives a state round trip and is not part of the blob.
    let blob = free.save_state();
    assert_eq!(blob, hw.save_state(), "{name}: save states differ");
    free.load_state(&blob).unwrap();
    hw.run_frame([0; 4]);
    free.run_frame([0; 4]);
    assert_eq!(hw.state_hash(), free.state_hash(), "{name}: diverged after load");
}

#[test]
fn sprite_limit_off_keeps_state_identical() {
    let Some(roms) = common::roms() else { return };
    for rel in ROMS {
        let rom = std::fs::read(roms.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        check(rel, &rom);
    }
}
