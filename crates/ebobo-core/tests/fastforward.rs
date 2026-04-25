//! Fast-forward is a host setting: it must not change emulated state, and frames rendered
//! with it off must be pixel-identical to a run that never used it.

mod common;

use ebobo_core::Nes;
use std::path::Path;

const ROMS: &[&str] = &[
    "nestest/nestest.nes",
    "mmc3_test_2/rom_singles/3-A12_clocking.nes",
    "mmc3_test_2/rom_singles/4-scanline_timing.nes",
    "sprite_hit_tests_2005.10.05/09.timing_basics.nes",
    "sprite_overflow_tests/3.Timing.nes",
];

const FRAMES: u32 = 600;

fn check(roms: &Path, rel: &str) {
    let path = roms.join(rel);
    let rom = std::fs::read(&path).unwrap_or_else(|e| panic!("{rel}: {e}"));
    let mut plain = Nes::new(&rom, None).unwrap();
    let mut ff = Nes::new(&rom, None).unwrap();
    let mut rng = 0x9E37_79B9_7F4A_7C15u64 ^ rel.len() as u64;
    let mut rendered = 0;
    for frame in 0..FRAMES {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let on = rng & 1 != 0;
        // Some input so games react; start/select pressed now and then.
        let pads = [(rng >> 8) as u8 & 0xF0 | ((rng >> 16) as u8 & 0x0C), 0, 0, 0];
        ff.set_fast_forward(on);
        plain.run_frame(pads);
        ff.run_frame(pads);
        assert_eq!(plain.state_hash(), ff.state_hash(), "{rel}: state diverged at frame {frame} (ff={on})");
        if !on {
            assert!(plain.frame() == ff.frame(), "{rel}: picture differs at frame {frame}");
            rendered += 1;
        }
    }
    assert!(rendered > FRAMES / 4, "{rel}: too few frames rendered with fast-forward off");
    let (mut a, mut b) = (Vec::new(), Vec::new());
    plain.drain_audio(&mut a);
    ff.drain_audio(&mut b);
    assert!(b.len() < a.len(), "{rel}: fast-forward frames must not produce audio");
}

#[test]
fn fast_forward_keeps_state_and_pixels() {
    let Some(roms) = common::roms() else { return };
    for rel in ROMS {
        check(&roms, rel);
    }
}
