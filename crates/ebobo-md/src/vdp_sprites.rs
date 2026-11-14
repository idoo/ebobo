//! Sprites of one screen row. Owner: sprites slice.
//!
//! A pure function of the chip's memory: no state of its own, so the caller can skip the
//! pixels (fast-forward) and still get the same flags.

use crate::vdp_mem::{Mem, SpriteFlags, SpriteRow};

/// What the mode registers fix for a row of sprites.
struct Mode {
    /// Screen columns, and the sprite pixels the chip draws on a row: 320 or 256.
    width: u32,
    /// Entries of the sprite list the chip can walk: 80 or 64.
    list_len: usize,
    /// Sprites drawn on one row: 20 or 16.
    row_sprites: u32,
    /// The Y coordinate of screen row 0: 128, or 256 in interlace mode 2.
    y_origin: u32,
    /// The bits of the Y word that count: 9, or 10 in interlace mode 2.
    y_mask: u32,
    /// Rows of one pattern: 8, or 16 in interlace mode 2.
    cell_h: u32,
    /// Video RAM address of the sprite attribute table.
    base: usize,
}

impl Mode {
    fn of(m: &Mem) -> Mode {
        let h40 = m.regs[12] & 0x01 != 0;
        let interlace2 = m.regs[12] & 0x06 == 0x06;
        Mode {
            width: if h40 { 320 } else { 256 },
            list_len: if h40 { 80 } else { 64 },
            row_sprites: if h40 { 20 } else { 16 },
            y_origin: if interlace2 { 256 } else { 128 },
            y_mask: if interlace2 { 0x3FF } else { 0x1FF },
            cell_h: if interlace2 { 16 } else { 8 },
            base: ((m.regs[5] & if h40 { 0x7E } else { 0x7F }) as usize) << 9,
        }
    }
}

/// What one walk of the sprite list finds for a line.
struct Scan {
    flags: SpriteFlags,
    /// The pixel limit of the line was hit with sprite pixels left over.
    dot_overflow: bool,
}

/// Walks the sprite list for `line` (in sprite Y units: screen row + `Mode::y_origin`) and draws
/// into `out` if given. `prev_dot_overflow`: the line before hit the pixel limit, which lets a
/// sprite at x = 0 mask even as the first one of this line.
///
/// `dot_overflow` and `overflow` do not depend on `prev_dot_overflow` or on `out`; `collision`
/// is only found when drawing.
fn scan(m: &Mem, mode: &Mode, line: u32, prev_dot_overflow: bool, mut out: Option<&mut SpriteRow>) -> Scan {
    let mut flags = SpriteFlags::default();
    let mut dot_overflow = false;
    let mut on_row = 0;
    let mut budget = mode.width;
    let mut seen_nonzero_x = false;
    let mut masked = false;
    let mut index = 0usize;
    for _ in 0..mode.list_len {
        let entry = mode.base + index * 8;
        let byte = |offset: usize| m.vram[(entry + offset) & 0xFFFF] as u32;
        let y = (byte(0) << 8 | byte(1)) & mode.y_mask;
        let w_cells = ((byte(2) >> 2) & 3) + 1;
        let h_cells = (byte(2) & 3) + 1;
        let h_px = h_cells * mode.cell_h;
        if line >= y && line < y + h_px {
            if on_row == mode.row_sprites {
                flags.overflow = true;
                break;
            }
            on_row += 1;
            if budget == 0 {
                flags.overflow = true;
                dot_overflow = true;
                break;
            }
            let w_px = w_cells * 8;
            let drawn = w_px.min(budget);
            budget -= drawn;
            let x = (byte(6) << 8 | byte(7)) & 0x1FF;
            if x != 0 {
                seen_nonzero_x = true;
            } else if seen_nonzero_x || prev_dot_overflow {
                masked = true;
            }
            if !masked && let Some(out) = out.as_deref_mut() {
                let attr = byte(4) << 8 | byte(5);
                let high = ((attr >> 8) & 0x80 | (attr >> 9) & 0x30) as u8;
                let hflip = attr & 0x0800 != 0;
                let dy = if attr & 0x1000 != 0 { h_px - 1 - (line - y) } else { line - y };
                let pattern_bytes = mode.cell_h * 4;
                // The row `dy` of the sprite's first column of patterns; each further column is `h_cells` patterns on.
                let row_addr = ((attr & 0x7FF) + dy / mode.cell_h) * pattern_bytes + (dy % mode.cell_h) * 4;
                for cx in 0..drawn {
                    let column = (x + cx).wrapping_sub(128);
                    if column >= mode.width {
                        continue;
                    }
                    let sx = if hflip { w_px - 1 - cx } else { cx };
                    let addr = row_addr + (sx >> 3) * h_cells * pattern_bytes + ((sx & 7) >> 1);
                    let pair = m.vram[addr as usize & 0xFFFF];
                    let colour = if sx & 1 == 0 { pair >> 4 } else { pair & 0x0F };
                    if colour == 0 {
                        continue;
                    }
                    let pixel = &mut out[column as usize];
                    if *pixel != 0 {
                        flags.collision = true;
                    } else {
                        *pixel = high | colour;
                    }
                }
            }
            if drawn < w_px {
                flags.overflow = true;
                dot_overflow = true;
                break;
            }
        }
        let link = (byte(3) & 0x7F) as usize;
        if link == 0 || link >= mode.list_len {
            break;
        }
        index = link;
    }
    Scan { flags, dot_overflow }
}

/// Fills `row_out` with the sprite pixels of screen row `row` and returns the flags the row
/// raises. `row` counts from the first displayed line; in interlace mode 2 rows are half
/// lines (0..448 or 0..480) and patterns are 16 rows tall.
///
/// As the chip does: the sprite list is walked by its link fields from sprite 0 (at most 80
/// entries in 320-pixel mode, 64 in 256-pixel mode); the first 20 (16) sprites on the row are
/// drawn, up to 320 (256) sprite pixels; a sprite at x = 0 masks the lower-priority ones on
/// the row under the chip's rules; earlier sprites in the list are in front.
///
/// The details: Y counts 9 bits (10 in interlace mode 2) with screen row 0 at 128 (256), X 9
/// bits with column 0 at 128. Every sprite on the row takes its full width from the pixel
/// limit, on screen or not, masked or not; the one that crosses the limit is cut at it.
/// `overflow` is set when a sprite or part of one is left over at either limit. A sprite at
/// x = 0 masks the rest of the row if an earlier sprite of the row had another x, or if the
/// row before was cut at the pixel limit. `collision` is two non-transparent pixels meeting in
/// a screen column. Columns from the screen width on are left 0.
pub fn sprite_row(m: &Mem, row: u32, row_out: &mut SpriteRow) -> SpriteFlags {
    row_out.fill(0);
    let mode = Mode::of(m);
    let line = row.wrapping_add(mode.y_origin);
    let prev = scan(m, &mode, line.wrapping_sub(1), false, None);
    scan(m, &mode, line, prev.dot_overflow, Some(row_out)).flags
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: SpriteFlags = SpriteFlags { overflow: false, collision: false };
    const OVERFLOW: SpriteFlags = SpriteFlags { overflow: true, collision: false };

    struct Chip {
        regs: [u8; 24],
        vram: Vec<u8>,
        cram: [u16; 64],
        vsram: [u16; 40],
        /// Where `sprite` writes the table.
        sat: usize,
    }

    impl Chip {
        /// Table at 0xE000; pattern `n` (1..=15) filled with colour `n`, pattern 0 transparent.
        fn new(h40: bool) -> Chip {
            let mut regs = [0u8; 24];
            regs[12] = if h40 { 0x81 } else { 0x00 };
            regs[5] = 0x70;
            let mut chip = Chip { regs, vram: vec![0; 0x10000], cram: [0; 64], vsram: [0; 40], sat: 0xE000 };
            for n in 1..16 {
                chip.vram[n * 32..n * 32 + 32].fill(n as u8 * 0x11);
            }
            chip
        }

        /// Sprite `i` with its top left at screen (`x`, `y`), `w` by `h` cells.
        #[allow(clippy::too_many_arguments)]
        fn sprite(&mut self, i: usize, x: i32, y: i32, w: u8, h: u8, link: u8, attr: u16) {
            self.raw(i, (x + 128) as u16, (y + 128) as u16, w, h, link, attr);
        }

        /// Sprite `i` with the X and Y words as given.
        #[allow(clippy::too_many_arguments)]
        fn raw(&mut self, i: usize, x: u16, y: u16, w: u8, h: u8, link: u8, attr: u16) {
            let e = self.sat + i * 8;
            self.vram[e..e + 2].copy_from_slice(&y.to_be_bytes());
            self.vram[e + 2] = (w - 1) << 2 | (h - 1);
            self.vram[e + 3] = link;
            self.vram[e + 4..e + 6].copy_from_slice(&attr.to_be_bytes());
            self.vram[e + 6..e + 8].copy_from_slice(&x.to_be_bytes());
        }

        fn row(&self, row: u32) -> (SpriteRow, SpriteFlags) {
            let mut out = [0xFF; 320];
            let m = Mem { regs: &self.regs, vram: &self.vram, cram: &self.cram, vsram: &self.vsram };
            let flags = sprite_row(&m, row, &mut out);
            (out, flags)
        }
    }

    #[test]
    fn one_cell_lands_on_its_rows_and_columns() {
        let mut c = Chip::new(true);
        c.sprite(0, 20, 10, 1, 1, 0, 0x8000 | 0x2000 | 5);
        for row in [9, 18] {
            let (out, flags) = c.row(row);
            assert!(out.iter().all(|&p| p == 0), "row {row}");
            assert_eq!(flags, NONE);
        }
        for row in 10..18 {
            let (out, flags) = c.row(row);
            for (x, &p) in out.iter().enumerate() {
                assert_eq!(p, if (20..28).contains(&x) { 0x95 } else { 0 }, "row {row} column {x}");
            }
            assert_eq!(flags, NONE);
        }
    }

    #[test]
    fn patterns_run_down_then_right() {
        let mut c = Chip::new(true);
        c.sprite(0, 0, 0, 2, 3, 0, 1);
        for (row, left, right) in [(0, 1, 4), (7, 1, 4), (8, 2, 5), (15, 2, 5), (16, 3, 6), (23, 3, 6)] {
            let (out, _) = c.row(row);
            assert_eq!(out[..8], [left; 8], "row {row}");
            assert_eq!(out[8..16], [right; 8], "row {row}");
            assert_eq!(out[16], 0);
        }
        assert!(c.row(24).0.iter().all(|&p| p == 0));
    }

    #[test]
    fn flips_within_a_pattern() {
        let mut c = Chip::new(true);
        c.vram[32..36].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        c.vram[60..64].copy_from_slice(&[0x9A, 0xBC, 0xDE, 0xF1]);
        let cases: [(u16, [u8; 8]); 4] = [
            (0x0000, [1, 2, 3, 4, 5, 6, 7, 8]),
            (0x0800, [8, 7, 6, 5, 4, 3, 2, 1]),
            (0x1000, [9, 10, 11, 12, 13, 14, 15, 1]),
            (0x1800, [1, 15, 14, 13, 12, 11, 10, 9]),
        ];
        for (flip, want) in cases {
            c.sprite(0, 0, 0, 1, 1, 0, flip | 1);
            assert_eq!(c.row(0).0[..8], want, "flip {flip:04X}");
        }
    }

    #[test]
    fn flips_turn_the_whole_sprite() {
        let mut c = Chip::new(true);
        // 2x2: patterns 1 2 down the left column, 3 4 down the right.
        let cases = [(0x0000, [1, 3], [2, 4]), (0x0800, [3, 1], [4, 2]), (0x1000, [2, 4], [1, 3]), (0x1800, [4, 2], [3, 1])];
        for (flip, top, bottom) in cases {
            c.sprite(0, 0, 0, 2, 2, 0, flip | 1);
            let (out, _) = c.row(0);
            assert_eq!([out[0], out[8]], top, "flip {flip:04X}");
            let (out, _) = c.row(8);
            assert_eq!([out[0], out[8]], bottom, "flip {flip:04X}");
        }
    }

    #[test]
    fn link_order_decides_who_is_in_front() {
        let mut c = Chip::new(true);
        c.sprite(0, 100, 100, 1, 1, 2, 3);
        c.sprite(1, 4, 0, 1, 1, 0, 0x8000 | 1);
        c.sprite(2, 0, 0, 1, 1, 1, 0x4000 | 2);
        c.sprite(3, 40, 0, 1, 1, 0, 4);
        let (out, flags) = c.row(0);
        assert_eq!(out[..8], [0x22; 8]);
        assert_eq!(out[8..12], [0x81; 4]);
        assert_eq!(out[12], 0);
        assert_eq!(out[40], 0, "sprite 3 is not in the list");
        assert_eq!(flags, SpriteFlags { overflow: false, collision: true });
    }

    #[test]
    fn transparent_pixels_show_what_is_behind_and_do_not_collide() {
        let mut c = Chip::new(true);
        c.sprite(0, 0, 0, 1, 1, 1, 0);
        c.sprite(1, 0, 0, 1, 1, 0, 7);
        let (out, flags) = c.row(0);
        assert_eq!(out[..8], [7; 8]);
        assert_eq!(flags, NONE);
    }

    /// `n` 1x1 sprites side by side on row 0, colour 1.
    fn side_by_side(c: &mut Chip, n: usize) {
        for i in 0..n {
            c.sprite(i, i as i32 * 8, 0, 1, 1, if i + 1 == n { 0 } else { i as u8 + 1 }, 1);
        }
    }

    #[test]
    fn sprite_limit() {
        for (h40, limit) in [(true, 20), (false, 16)] {
            let mut c = Chip::new(h40);
            side_by_side(&mut c, limit);
            let (out, flags) = c.row(0);
            assert_eq!(out[limit * 8 - 1], 1);
            assert_eq!(flags, NONE);

            side_by_side(&mut c, limit + 1);
            let (out, flags) = c.row(0);
            assert_eq!(out[limit * 8 - 1], 1);
            assert_eq!(out[limit * 8], 0);
            assert_eq!(flags, OVERFLOW);
            assert_eq!(c.row(8), ([0; 320], NONE));
        }
    }

    /// `n` sprites 4 cells wide left of the screen on row 0, then one 4 cells wide at column 0.
    fn off_screen_then_one(c: &mut Chip, n: usize) {
        for i in 0..n {
            c.raw(i, 1, 128, 4, 1, i as u8 + 1, 2);
        }
        c.sprite(n, 0, 0, 4, 1, 0, 1);
    }

    #[test]
    fn pixel_limit() {
        for (h40, fit) in [(true, 10), (false, 8)] {
            let mut c = Chip::new(h40);
            off_screen_then_one(&mut c, fit - 1);
            let (out, flags) = c.row(0);
            assert_eq!([out[0], out[8], out[16], out[24], out[31]], [1, 2, 3, 4, 4], "a 4x1 sprite shows patterns 1-4");
            assert_eq!(flags, NONE, "exactly the limit is not an overflow");

            off_screen_then_one(&mut c, fit);
            let (out, flags) = c.row(0);
            assert!(out.iter().all(|&p| p == 0));
            assert_eq!(flags, OVERFLOW);
        }
    }

    #[test]
    fn the_sprite_crossing_the_pixel_limit_is_cut() {
        let mut c = Chip::new(true);
        // 9 * 32 + 24 = 312 pixels before the last sprite: 8 of its 32 are drawn.
        off_screen_then_one(&mut c, 10);
        c.raw(9, 1, 128, 3, 1, 10, 2);
        let (out, flags) = c.row(0);
        assert_eq!(out[..8], [1; 8]);
        assert_eq!(out[8..32], [0; 24]);
        assert_eq!(flags, OVERFLOW);
    }

    #[test]
    fn x_zero_masks_after_another_sprite() {
        let mut c = Chip::new(true);
        c.sprite(0, 0, 0, 1, 1, 1, 1);
        c.raw(1, 0, 128, 1, 1, 2, 3);
        c.sprite(2, 8, 0, 1, 1, 0, 2);
        let (out, flags) = c.row(0);
        assert_eq!(out[..8], [1; 8]);
        assert_eq!(out[8..16], [0; 8]);
        assert_eq!(flags, NONE);
    }

    #[test]
    fn x_zero_first_on_the_row_does_not_mask() {
        let mut c = Chip::new(true);
        c.raw(0, 0, 128, 1, 1, 1, 3);
        c.sprite(1, 8, 0, 1, 1, 0, 2);
        let (out, flags) = c.row(0);
        assert_eq!(out[8..16], [2; 8]);
        assert_eq!(flags, NONE);
    }

    #[test]
    fn x_zero_first_on_the_row_masks_after_a_row_cut_at_the_pixel_limit() {
        let mut c = Chip::new(true);
        // Eleven wide sprites whose last row is 4: 352 pixels there.
        for i in 0..11 {
            c.raw(i, 1, 128 - 3, 4, 1, i as u8 + 1, 2);
        }
        c.raw(11, 0, 128 + 5, 1, 1, 12, 3);
        c.sprite(12, 8, 5, 1, 1, 0, 2);
        assert_eq!(c.row(4), ([0; 320], OVERFLOW));
        assert_eq!(c.row(5), ([0; 320], NONE));
        let (out, flags) = c.row(6);
        assert_eq!(out[8..16], [2; 8]);
        assert_eq!(flags, NONE);
    }

    #[test]
    fn table_base_ignores_the_low_bit_in_320_pixel_mode_only() {
        let mut c = Chip::new(false);
        c.regs[5] = 0x71;
        c.sprite(0, 0, 0, 1, 1, 0, 1);
        c.sat = 0xE200;
        c.sprite(0, 0, 0, 1, 1, 0, 2);
        assert_eq!(c.row(0).0[0], 2);
        c.regs[12] = 0x81;
        assert_eq!(c.row(0).0[0], 1);
    }

    #[test]
    fn list_length_and_columns_of_256_pixel_mode() {
        let mut c = Chip::new(false);
        c.sprite(0, 252, 0, 1, 1, 70, 1);
        c.sprite(70, 0, 0, 1, 1, 0, 2);
        let (out, flags) = c.row(0);
        assert_eq!(out[0], 0, "link 70 is beyond 64 sprites");
        assert_eq!(out[252..256], [1; 4]);
        assert_eq!(out[256..260], [0; 4]);
        assert_eq!(flags, NONE);
        c.regs[12] = 0x81;
        let (out, _) = c.row(0);
        assert_eq!(out[0], 2);
        assert_eq!(out[252..260], [1; 8]);
    }

    #[test]
    fn interlace_mode_2_patterns_are_16_rows_from_y_256() {
        let mut c = Chip::new(true);
        c.regs[12] = 0x87;
        c.vram[64..96].fill(0x11);
        c.vram[96..128].fill(0x22);
        c.vram[128..192].fill(0x33);
        c.raw(0, 128, 256 + 10, 1, 2, 0, 1);
        for (row, want) in [(9, 0), (10, 1), (17, 1), (18, 2), (25, 2), (26, 3), (41, 3), (42, 0)] {
            assert_eq!(c.row(row).0[..8], [want; 8], "row {row}");
        }
        // Y bit 9 counts here: the same sprite 512 lower is off the row.
        c.raw(0, 128, 512 + 256 + 10, 1, 2, 0, 1);
        assert_eq!(c.row(10).0[0], 0);
    }
}
