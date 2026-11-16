//! `Nes::set_tilt` / `Nes::frame_tilted` on a test ROM picture with background and sprites.

use crate::{pad, Nes, HEIGHT, TILT_LEFT_BLANK, TILT_REACH, WIDTH};

/// Test ROM (fetched by scripts/fetch-test-roms.sh) that shows a background and sprites.
const ROM: &str = "scrolltest/scroll.nes";

fn nes() -> Option<Nes> {
    let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms"));
    let bytes = std::fs::read(dir.join(ROM)).ok()?;
    Some(Nes::new(&bytes, None).unwrap())
}

/// Pads for frame `n`: idle, then the scroll test is moved right and down.
fn pads(n: u32) -> [u8; 4] {
    if n < 60 { [0; 4] } else { [pad::RIGHT | pad::DOWN, 0, 0, 0] }
}

fn run(nes: &mut Nes, from: u32, to: u32) {
    for n in from..to {
        nes.run_frame(pads(n));
    }
}

/// Left edge and size of the part of the picture `frame_tilted` hands out.
fn crop() -> (usize, usize, usize) {
    let (rx, ry) = TILT_REACH;
    let left = TILT_LEFT_BLANK + rx;
    (left, WIDTH - left - rx, HEIGHT - 2 * ry)
}

/// The plain picture, cropped as `frame_tilted` crops it.
fn plain_middle(nes: &Nes) -> (Vec<u8>, Vec<u8>) {
    let (mut rgba, mut layers) = (vec![0u8; WIDTH * HEIGHT * 4], vec![0u8; WIDTH * HEIGHT]);
    nes.frame_rgba(&mut rgba);
    nes.frame_layers(&mut layers);
    let ry = TILT_REACH.1;
    let (left, w, _) = crop();
    let (mut mr, mut ml) = (Vec::new(), Vec::new());
    for y in ry..HEIGHT - ry {
        mr.extend_from_slice(&rgba[(y * WIDTH + left) * 4..][..w * 4]);
        ml.extend_from_slice(&layers[y * WIDTH + left..][..w]);
    }
    (mr, ml)
}

fn tilted(nes: &Nes, shift: (i32, i32)) -> (Vec<u8>, Vec<u8>) {
    let (mut rgba, mut layers) = (Vec::new(), Vec::new());
    let (w, h) = nes.frame_tilted(shift, &mut rgba, &mut layers);
    let (_, cw, ch) = crop();
    assert_eq!((w, h), (cw, ch));
    (rgba, layers)
}

/// With no shift the tilted picture is the plain one's middle, colours and layers, on two
/// different pictures; and it really comes from the planes, not the fallback.
#[test]
fn tilt_zero_shift_matches_plain_picture() {
    let Some(mut nes) = nes() else { return };
    nes.set_tilt(true);
    let mut seen = Vec::new();
    for (from, to) in [(0, 60), (60, 100)] {
        run(&mut nes, from, to);
        assert!(nes.bus.tilt_planes().is_some(), "planes valid after frame {to}");
        let (rgba, layers) = plain_middle(&nes);
        assert!(layers.contains(&2), "background on the picture of frame {to}");
        assert!(layers.iter().any(|&l| l == 1 || l == 3), "sprites on the picture of frame {to}");
        assert!(tilted(&nes, (0, 0)) == (rgba.clone(), layers), "zero shift differs at frame {to}");
        seen.push(rgba);
    }
    assert!(seen[0] != seen[1], "the two pictures are different");
}

/// A shift moves the background under the sprites and leaves the front sprites in place.
#[test]
fn tilt_moves_background_not_sprites() {
    let Some(mut nes) = nes() else { return };
    nes.set_tilt(true);
    run(&mut nes, 0, 100);
    let (rx, ry) = TILT_REACH;
    let (left, w, h) = crop();
    let shift = (3i32, -2i32);
    let (still_rgba, still_layers) = tilted(&nes, (0, 0));
    let (moved_rgba, moved_layers) = tilted(&nes, shift);
    // Clamped beyond the reach: the same as the reach itself.
    assert!(tilted(&nes, (40, -40)) == tilted(&nes, (rx as i32, -(ry as i32))));
    let (mut rgba, mut layers) = (vec![0u8; WIDTH * HEIGHT * 4], vec![0u8; WIDTH * HEIGHT]);
    nes.frame_rgba(&mut rgba);
    nes.frame_layers(&mut layers);
    let sprite = |l: u8| l == 1 || l == 3;
    let (mut background, mut front) = (0, 0);
    for y in 0..h {
        for x in 0..w {
            let o = y * w + x;
            let (px, py) = ((x + left) as i32 - shift.0, (y + ry) as i32 - shift.1);
            let src = py as usize * WIDTH + px as usize;
            if !sprite(still_layers[o]) && !sprite(moved_layers[o]) && !sprite(layers[src]) {
                background += 1;
                assert_eq!(moved_layers[o], layers[src], "layer at ({x}, {y})");
                assert_eq!(moved_rgba[o * 4..][..4], rgba[src * 4..][..4], "colour at ({x}, {y})");
            }
            if still_layers[o] == 3 {
                front += 1;
                assert_eq!(moved_layers[o], 3, "front sprite stays at ({x}, {y})");
                assert_eq!(moved_rgba[o * 4..][..4], still_rgba[o * 4..][..4], "sprite colour at ({x}, {y})");
            }
        }
    }
    assert!(background > w * h / 2, "{background} background pixels checked");
    assert!(front > 0, "front sprite pixels checked");
    assert!(moved_rgba != still_rgba, "the shift moved something");
}

/// A game may hide the background in its 8 leftmost columns (PPUMASK bit 1 off, as most
/// scrolling games do). Even at the farthest shift to the right the picture starts at the
/// background's column 8, so that blank strip never slides into view.
#[test]
fn tilt_never_shows_the_columns_a_game_may_blank() {
    let Some(mut nes) = nes() else { return };
    nes.set_tilt(true);
    run(&mut nes, 0, 100);
    let (rx, ry) = TILT_REACH;
    let (_, w, h) = crop();
    let (moved_rgba, moved_layers) = tilted(&nes, (rx as i32, 0));
    let (mut rgba, mut layers) = (vec![0u8; WIDTH * HEIGHT * 4], vec![0u8; WIDTH * HEIGHT]);
    nes.frame_rgba(&mut rgba);
    nes.frame_layers(&mut layers);
    let sprite = |l: u8| l == 1 || l == 3;
    let mut checked = 0;
    for y in 0..h {
        let src = (y + ry) * WIDTH + 8;
        if !sprite(moved_layers[y * w]) && !sprite(layers[src]) {
            checked += 1;
            assert_eq!(moved_rgba[y * w * 4..][..4], rgba[src * 4..][..4], "first column of row {y}");
        }
    }
    assert!(checked > h / 2, "{checked} rows checked");
}

/// Tilt is not emulated state: hash and plain picture match a run without it, a restore keeps
/// it on, skipped frames keep the planes and turning it off forgets them.
#[test]
fn tilt_is_not_emulated_state() {
    let (Some(mut on), Some(mut off)) = (nes(), nes()) else { return };
    on.set_tilt(true);
    run(&mut on, 0, 80);
    run(&mut off, 0, 80);
    assert_eq!(on.state_hash(), off.state_hash());
    assert!(on.frame() == off.frame(), "plain picture differs");

    let snap = on.snapshot();
    run(&mut on, 80, 90);
    on.restore(&snap);
    assert!(on.bus.tilt_planes().is_some(), "restore keeps the planes");
    run(&mut on, 80, 81);
    assert!(on.bus.tilt_planes().is_some(), "tilt still on after restore");
    assert!(tilted(&on, (0, 0)) == plain_middle(&on), "planes match the picture after restore");

    on.set_fast_forward(true);
    run(&mut on, 81, 85);
    on.set_fast_forward(false);
    assert!(on.bus.tilt_planes().is_some(), "skipped frames keep the planes");
    assert!(tilted(&on, (0, 0)) == plain_middle(&on), "planes still match the kept picture");

    on.set_tilt(false);
    assert!(on.bus.tilt_planes().is_none(), "off forgets the planes");
}

/// Rollback re-simulates with fast-forward and switches it off for the one picture it shows,
/// frame after frame: every shown picture still gets its planes.
#[test]
fn tilt_planes_follow_pictures_between_skipped_frames() {
    let Some(mut nes) = nes() else { return };
    nes.set_tilt(true);
    for n in 0..40 {
        nes.set_fast_forward(true);
        run(&mut nes, 2 * n, 2 * n + 1);
        nes.set_fast_forward(false);
        run(&mut nes, 2 * n + 1, 2 * n + 2);
    }
    assert!(nes.bus.tilt_planes().is_some(), "a picture between skipped frames has planes");
    assert!(tilted(&nes, (0, 0)) == plain_middle(&nes), "and they are that picture's");
}
