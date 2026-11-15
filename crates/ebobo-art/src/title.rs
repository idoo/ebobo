//! Capturing a game's title screen by running it. Owner: title slice.
//!
//! The game runs without input for up to [`FRAMES`] frames. Every [`EVERY`]th frame from
//! [`FIRST`] on is looked at, shrunk [`SHRINK`] times: how much of it is not the background,
//! how many colours it has, how much it moved since the last look. A title screen waits for
//! Start, so the answer is the best picture that stood still; a logo in many colours beats a
//! line of legal text on black, and of two screens about as good the later one wins (company
//! logos come first). A picture that waited [`SETTLED_LOOKS`] looks is the title screen and
//! the run stops there: after it comes the attract demo, busier and paused often enough to
//! look still, but not the title.

use crate::Picture;
use ebobo_console::Console;

/// Frames run: 20 seconds of a 60 Hz game.
const FRAMES: u32 = 1200;
/// The first frame looked at; before it consoles show nothing worth keeping.
const FIRST: u32 = 60;
/// Frames between two looks.
const EVERY: u32 = 20;
/// One cell of the small copy stands for a square of this many pixels a side (its centre).
const SHRINK: usize = 4;
/// Two looks that differ in at most this many cells per thousand show a picture standing
/// still: a blinking "PRESS START" is about ten.
const STILL: u32 = 20;
/// Looks in a row a picture has to stand still for (one second with the first of them).
const STILL_LOOKS: u32 = 3;
/// Looks in a row (six seconds) after which a still picture is the title screen and the run
/// ends: company logos and legal text go on after two or three seconds, a title screen waits
/// ten or more before the attract demo starts.
const SETTLED_LOOKS: u32 = 18;
/// A still picture with less than this many cells per thousand off the background is as good
/// as blank: a cursor, a copyright sign.
const MIN_COVERAGE: u32 = 30;
/// Coverage above this, per thousand, does not make a picture better: half the screen.
const COVERAGE_CAP: u32 = 500;
/// More colours than this do not make a picture better.
const COLOURS_CAP: u32 = 16;

/// What one look found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Look {
    /// The frame's number, from 1.
    frame: u32,
    /// Cells per thousand that are not the most common colour.
    coverage: u32,
    /// Distinct colours.
    colours: u32,
    /// Cells per thousand that changed since the look before (1000 for the first).
    moved: u32,
    /// Looks in a row, this one included, that stood still.
    still: u32,
}

impl Look {
    /// How much there is to see, 0 for a blank picture: coverage times colours, both capped.
    fn score(&self) -> u32 {
        self.coverage.min(COVERAGE_CAP) * self.colours.min(COLOURS_CAP)
    }
}

/// The best picture so far of one kind; `score` 0 while there is none.
#[derive(Default)]
struct Best {
    score: u32,
    w: usize,
    h: usize,
    rgba: Vec<u8>,
}

impl Best {
    fn keep(&mut self, score: u32, w: usize, h: usize, rgba: &[u8]) {
        self.score = score;
        self.w = w;
        self.h = h;
        self.rgba.clear();
        self.rgba.extend_from_slice(rgba);
    }

    fn picture(self) -> Option<Picture> {
        (self.score > 0).then_some(Picture { w: self.w as u32, h: self.h as u32, rgba: self.rgba })
    }
}

/// Runs the ROM without input for up to about 20 emulated seconds and returns the frame that
/// looks most like a title screen, at the console's own resolution: a picture that stands
/// still, is not blank, and has more going on than a line of text on black. `None` for a
/// file that is not a ROM ebobo can run, or a game that shows nothing in that time.
///
/// Deterministic: the same ROM always gives the same picture.
pub fn capture(rom: &[u8]) -> Option<Picture> {
    capture_with(rom, true, |_| {})
}

/// [`capture`]. `skip_drawing`: the frames nobody looks at run in fast-forward (the picture is
/// the same either way, a test holds the core to it). `note` hears of every look.
fn capture_with(rom: &[u8], skip_drawing: bool, mut note: impl FnMut(Look)) -> Option<Picture> {
    let mut console = Console::new(rom, None).ok()?;
    let (mut rgba, mut cells, mut before, mut sorted) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // The best picture that stood still, and the best of all for a game where none did.
    let (mut title, mut any) = (Best::default(), Best::default());
    // Looks in a row that stood still, and whether `title` was taken in this row.
    let (mut still, mut held) = (0, false);
    for frame in 1..=FRAMES {
        let looked_at = frame >= FIRST && frame % EVERY == 0;
        console.set_fast_forward(skip_drawing && !looked_at);
        console.run_frame([0; 4]);
        if !looked_at {
            continue;
        }
        let (w, h) = console.frame_rgba(&mut rgba);
        shrink(&rgba, w, h, &mut cells);
        let (coverage, colours) = coverage_and_colours(&cells, &mut sorted);
        let moved = moved(&cells, &before);
        std::mem::swap(&mut cells, &mut before);
        if moved > STILL {
            (still, held) = (0, false);
        }
        still += 1;
        let look = Look { frame, coverage, colours, moved, still };
        note(look);
        let score = look.score();
        if score > any.score {
            any.keep(score, w, h, &rgba);
        }
        // Within one row of still looks only a better one replaces (the blinking text is on);
        // a new row wins when it is about as good.
        let better = if held { score > title.score } else { score * 10 >= title.score * 9 };
        if still >= STILL_LOOKS && coverage >= MIN_COVERAGE && better {
            title.keep(score, w, h, &rgba);
            held = true;
        }
        if held && still >= SETTLED_LOOKS {
            break;
        }
    }
    title.picture().or_else(|| any.picture())
}

/// The small copy of a `w`×`h` RGBA picture: the centre pixel of every [`SHRINK`]-pixel
/// square as 0xRRGGBB, row by row. Picked, not averaged: an average would invent colours.
fn shrink(rgba: &[u8], w: usize, h: usize, cells: &mut Vec<u32>) {
    cells.clear();
    for y in (SHRINK / 2..h).step_by(SHRINK) {
        for x in (SHRINK / 2..w).step_by(SHRINK) {
            let p = &rgba[(y * w + x) * 4..][..3];
            cells.push(u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]));
        }
    }
}

/// Cells per thousand that are not the most common colour, and the number of distinct
/// colours. `sorted` is a reusable buffer.
fn coverage_and_colours(cells: &[u32], sorted: &mut Vec<u32>) -> (u32, u32) {
    sorted.clear();
    sorted.extend_from_slice(cells);
    sorted.sort_unstable();
    let (mut colours, mut most) = (0, 0);
    for run in sorted.chunk_by(|a, b| a == b) {
        colours += 1;
        most = most.max(run.len());
    }
    let coverage = if cells.is_empty() { 0 } else { (cells.len() - most) * 1000 / cells.len() };
    (coverage as u32, colours)
}

/// Cells per thousand that differ between two small copies; 1000 when their sizes differ or
/// there is nothing to compare.
fn moved(cells: &[u32], before: &[u32]) -> u32 {
    if cells.len() != before.len() || cells.is_empty() {
        return 1000;
    }
    let changed = cells.iter().zip(before).filter(|(a, b)| a != b).count();
    (changed * 1000 / cells.len()) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A NES test ROM that ends on a screen of text, fetched by `scripts/fetch-test-roms.sh`.
    const KNOWN_ROM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ebobo-core/tests/roms/sprite_hit_tests_2005.10.05/01.basics.nes");

    fn look(coverage: u32, colours: u32) -> Look {
        Look { coverage, colours, ..Look::default() }
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(capture(&[]), None);
        let noise: Vec<u8> = (0..40_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        assert_eq!(capture(&noise), None);
        assert_eq!(capture(b"NES\x1a\x02\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"), None, "a header without the game");
    }

    #[test]
    fn small_copy_counts() {
        // 8x8: black, with the 4x4 square at the right bottom red.
        let mut rgba = vec![0u8; 8 * 8 * 4];
        for y in 4..8 {
            for x in 4..8 {
                rgba[(y * 8 + x) * 4] = 255;
            }
        }
        let (mut cells, mut sorted) = (Vec::new(), Vec::new());
        shrink(&rgba, 8, 8, &mut cells);
        assert_eq!(cells, [0, 0, 0, 0xFF0000]);
        assert_eq!(coverage_and_colours(&cells, &mut sorted), (250, 2));
        assert_eq!(coverage_and_colours(&[7; 9], &mut sorted), (0, 1));
        assert_eq!(coverage_and_colours(&[], &mut sorted), (0, 0));
        assert_eq!(moved(&cells, &cells), 0);
        assert_eq!(moved(&cells, &[0, 0, 0, 0]), 250);
        assert_eq!(moved(&cells, &[0, 0]), 1000);
        assert_eq!(moved(&[], &[]), 1000);
    }

    #[test]
    fn a_logo_beats_legal_text() {
        let (legal, logo, full) = (look(120, 2), look(350, 9), look(900, 40));
        assert_eq!(look(0, 1).score(), 0, "blank");
        assert!(logo.score() > 10 * legal.score());
        assert_eq!(full.score(), COVERAGE_CAP * COLOURS_CAP);
    }

    #[test]
    fn known_rom_gives_its_screen() {
        let Ok(rom) = std::fs::read(KNOWN_ROM) else { return };
        let picture = capture(&rom).expect("a picture");
        assert_eq!((picture.w, picture.h, picture.rgba.len()), (256, 240, 256 * 240 * 4));
        assert!(picture.rgba.chunks_exact(4).any(|p| p != &picture.rgba[..4]), "blank");
        assert_eq!(capture(&rom).as_ref(), Some(&picture), "the same picture twice");
        assert_eq!(capture_with(&rom, false, |_| {}), Some(picture), "fast-forward changed the picture");
    }

    /// Writes `/tmp/ebobo-title-<name>.png` for every `.nes` file of the folder named by
    /// `EBOBO_TITLE_ROMS`, and prints what every look found: for tuning the rule by eye.
    /// A PPM instead while `image::encode_png` is not written.
    #[test]
    #[ignore = "needs games: EBOBO_TITLE_ROMS=<folder>"]
    fn write_title_pictures() {
        let dir = std::env::var("EBOBO_TITLE_ROMS").expect("EBOBO_TITLE_ROMS");
        let mut roms: Vec<_> = std::fs::read_dir(&dir).expect("the folder").map(|e| e.unwrap().path()).collect();
        roms.retain(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("nes")));
        roms.sort();
        for path in roms {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            let rom = std::fs::read(&path).unwrap();
            let picture = capture_with(&rom, true, |l| {
                println!("{name}: frame {:4} coverage {:4} colours {:2} moved {:4} still {:2} score {:5}", l.frame, l.coverage, l.colours, l.moved, l.still, l.score())
            });
            let Some(picture) = picture else {
                println!("{name}: no picture");
                continue;
            };
            let out = format!("/tmp/ebobo-title-{name}");
            let file = match std::panic::catch_unwind(|| crate::image::encode_png(&picture)) {
                Ok(png) => (format!("{out}.png"), png),
                Err(_) => {
                    let mut ppm = format!("P6\n{} {}\n255\n", picture.w, picture.h).into_bytes();
                    ppm.extend(picture.rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]));
                    (format!("{out}.ppm"), ppm)
                }
            };
            std::fs::write(Path::new(&file.0), file.1).unwrap();
            println!("{name}: {}x{} -> {}", picture.w, picture.h, file.0);
        }
    }
}
