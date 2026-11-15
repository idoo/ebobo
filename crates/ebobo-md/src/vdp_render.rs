//! Draws one screen row: scroll planes A and B, the window plane, sprites, priorities,
//! shadow and highlight, colour output. Owner: planes slice.
//!
//! A pure function of the chip's memory. Mode 5 only (the Master System compatibility mode
//! is not emulated).
//!
//! A row is worked out per layer into a line buffer of one byte per screen column, in the
//! format of [`SpriteRow`]: bits 0-3 the colour (0 = transparent), bits 4-5 the palette line,
//! bit 7 the priority flag of the cell (kept on transparent pixels too: shadow mode needs
//! it). The layers are then composited into colours.

use crate::vdp_mem::{Mem, SpriteFlags, SpriteRow};
use crate::vdp_sprites::sprite_row;

/// Output level of each 3-bit channel value at normal brightness.
const NORMAL: [u8; 8] = [0, 52, 87, 116, 144, 172, 206, 255];
/// Output levels of a shadowed pixel.
const SHADOW: [u8; 8] = [0, 29, 52, 70, 87, 101, 116, 130];
/// Output levels of a highlighted pixel.
const HIGHLIGHT: [u8; 8] = [130, 144, 158, 172, 187, 206, 228, 255];

/// Draws screen row `row` into `out` (the first 256 or 320 entries, `0x00RRGGBB`) and returns
/// the flags the row's sprites raise. With `out` = `None` only the flags are worked out, and
/// they are the same. `row` counts from the first displayed line; in interlace mode 2 rows
/// are half lines (0..448 or 0..480).
///
/// The caller handles a blanked display (register 1 bit 6 off: the whole row is the
/// background colour, no sprite flags); this function draws an enabled display.
pub fn render_row(m: &Mem, row: u32, out: Option<&mut [u32]>) -> SpriteFlags {
    let mut sprites: SpriteRow = [0; 320];
    let flags = sprite_row(m, row, &mut sprites);
    if let Some(out) = out {
        draw_row(m, row, &sprites, out);
    }
    flags
}

/// A 9-bit colour entry (`0000BBB0GGG0RRR0`) as `0x00RRGGBB`, through the console's
/// non-linear output levels. `shade`: 0 normal, 1 shadow, 2 highlight.
pub fn rgb(entry: u16, shade: u8) -> u32 {
    let levels = match shade {
        1 => &SHADOW,
        2 => &HIGHLIGHT,
        _ => &NORMAL,
    };
    let level = |shift: u32| u32::from(levels[usize::from(entry >> shift) & 7]);
    level(1) << 16 | level(5) << 8 | level(9)
}

/// A name table: its address in video RAM and its size in cells.
#[derive(Clone, Copy)]
struct Table {
    base: usize,
    hcells: usize,
    vcells: usize,
}

/// The big-endian word at `addr` of video RAM (the address wraps at 64 KB).
fn word(m: &Mem, addr: usize) -> usize {
    usize::from(m.vram[addr & 0xFFFF]) << 8 | usize::from(m.vram[(addr + 1) & 0xFFFF])
}

/// Scroll plane size in cells (across, down) from register 16. Size code 2 is not a size and
/// acts as 32; the name table never exceeds 8 KB, so a 128-cell width leaves 32 rows and a
/// 64-cell width at most 64.
fn plane_size(reg16: u8) -> (usize, usize) {
    let cells = |code: u8| match code & 3 {
        1 => 64,
        3 => 128,
        _ => 32,
    };
    let h = cells(reg16);
    let v = cells(reg16 >> 4);
    (h, v.min(4096 / h))
}

/// Draws screen columns `x0..x1` of name table `t` into `buf`, with the plane moved right by
/// `hs` pixels and plane row `v` (screen row plus vertical scroll, wrapped here) on the
/// screen row. `cell_shift` is 3 for 8-row cells, 4 for the 16-row cells of interlace mode 2.
fn plane_span(m: &Mem, t: Table, cell_shift: u32, hs: usize, v: usize, x0: usize, x1: usize, buf: &mut [u8; 320]) {
    let cell_h = 1usize << cell_shift;
    let v = v & (t.vcells * cell_h - 1);
    let line = v & (cell_h - 1);
    let row_addr = t.base + (v >> cell_shift) * t.hcells * 2;
    let wmask = t.hcells * 8 - 1;
    let mut px = x0.wrapping_sub(hs) & wmask;
    let mut x = x0;
    while x < x1 {
        let entry = word(m, row_addr + (px >> 3) * 2);
        let ty = if entry & 0x1000 != 0 { cell_h - 1 - line } else { line };
        let pa = ((entry & 0x7FF) << (cell_shift + 2)) + ty * 4;
        let mut bits = (word(m, pa) as u32) << 16 | word(m, pa + 2) as u32;
        if entry & 0x0800 != 0 {
            let b = bits.swap_bytes();
            bits = (b & 0x0F0F_0F0F) << 4 | (b >> 4) & 0x0F0F_0F0F;
        }
        let attr = ((entry >> 8) & 0x80 | (entry >> 9) & 0x30) as u8;
        let first = px & 7;
        let n = (8 - first).min(x1 - x);
        bits <<= first * 4;
        for o in &mut buf[x..x + n] {
            *o = attr | (bits >> 28) as u8;
            bits <<= 4;
        }
        x += n;
        px = (px + n) & wmask;
    }
}

/// Draws screen columns `x0..x1` of scroll plane `plane` (0 = A, 1 = B) with its vertical
/// scroll: one value for the plane, or (register 11 bit 2) one per two-cell column. The
/// columns move with the fine horizontal scroll; the partial column it leaves at the left
/// edge takes the value of column 0.
#[allow(clippy::too_many_arguments)]
fn scroll_plane(m: &Mem, t: Table, cell_shift: u32, hs: usize, plane: usize, row: usize, x0: usize, x1: usize, buf: &mut [u8; 320]) {
    let vmask = if cell_shift == 4 { 0x7FF } else { 0x3FF };
    if m.regs[11] & 4 == 0 {
        plane_span(m, t, cell_shift, hs, row + (usize::from(m.vsram[plane]) & vmask), x0, x1, buf);
        return;
    }
    let fine = hs & 15;
    let mut x = x0;
    while x < x1 {
        let unit = (x + 16 - fine) >> 4;
        let col = unit.saturating_sub(1).min(19);
        let end = (unit * 16 + fine).min(x1);
        plane_span(m, t, cell_shift, hs, row + (usize::from(m.vsram[col * 2 + plane]) & vmask), x, end, buf);
        x = end;
    }
}

/// The plane pixel in front at one column: A high, B high, A low, B low; 0 if both are
/// transparent.
#[inline]
fn plane_front(b: u8, a: u8) -> u8 {
    if a >= 0x80 && a & 15 != 0 {
        a
    } else if b >= 0x80 && b & 15 != 0 {
        b
    } else if a & 15 != 0 {
        a
    } else if b & 15 != 0 {
        b
    } else {
        0
    }
}

/// Planes and compositing of [`render_row`], given the row's sprite pixels.
fn draw_row(m: &Mem, row: u32, sprites: &SpriteRow, out: &mut [u32]) {
    let r = m.regs;
    let h40 = r[12] & 0x81 != 0;
    let width = if h40 { 320 } else { 256 };
    let cell_shift: u32 = if (r[12] >> 1) & 3 == 3 { 4 } else { 3 };
    let row = row as usize;
    let line = row >> (cell_shift - 3);

    let (hcells, vcells) = plane_size(r[16]);
    let table_a = Table { base: usize::from(r[2] & 0x38) << 10, hcells, vcells };
    let table_b = Table { base: usize::from(r[4] & 0x07) << 13, hcells, vcells };
    let table_w = Table {
        base: usize::from(r[3] & if h40 { 0x3C } else { 0x3E }) << 10,
        hcells: if h40 { 64 } else { 32 },
        vcells: 32,
    };

    let hs_addr = (usize::from(r[13] & 0x3F) << 10)
        + match r[11] & 3 {
            0 => 0,
            1 => (line & 7) * 4,
            2 => (line & !7) * 4,
            _ => line * 4,
        };
    let hs_a = word(m, hs_addr) & 0x3FF;
    let hs_b = word(m, hs_addr + 2) & 0x3FF;

    // The window: the whole row above or below its row edge, else left or right of its column edge.
    let wx = (usize::from(r[17] & 31) * 16).min(width);
    let wy = usize::from(r[18] & 31) * 8;
    let whole = if r[18] & 0x80 != 0 { line >= wy } else { line < wy };
    let (w0, w1) = if whole {
        (0, width)
    } else if r[17] & 0x80 != 0 {
        (wx, width)
    } else {
        (0, wx)
    };
    let (a0, a1) = if w0 == 0 { (w1, width) } else { (0, w0) };

    let mut buf_b = [0u8; 320];
    let mut buf_a = [0u8; 320];
    scroll_plane(m, table_b, cell_shift, hs_b, 1, row, 0, width, &mut buf_b);
    scroll_plane(m, table_a, cell_shift, hs_a, 0, row, a0, a1, &mut buf_a);
    plane_span(m, table_w, cell_shift, 0, row, w0, w1, &mut buf_a);

    let bg = usize::from(r[7] & 0x3F);
    let out = &mut out[..width];
    let layers = out.iter_mut().zip(&buf_b[..width]).zip(&buf_a[..width]).zip(&sprites[..width]);
    if r[12] & 8 == 0 {
        let mut pal = [0u32; 64];
        for (p, &c) in pal.iter_mut().zip(m.cram.iter()) {
            *p = rgb(c, 0);
        }
        for (((o, &b), &a), &s) in layers {
            let p = plane_front(b, a);
            let c = if s & 15 != 0 && (s >= 0x80 || p < 0x80) { s } else { p };
            *o = pal[if c & 15 == 0 { bg } else { usize::from(c & 0x3F) }];
        }
    } else {
        let mut pal = [[0u32; 64]; 3];
        for (shade, line) in pal.iter_mut().enumerate() {
            for (p, &c) in line.iter_mut().zip(m.cram.iter()) {
                *p = rgb(c, shade as u8);
            }
        }
        for (((o, &b), &a), &s) in layers {
            // Shadowed unless a high-priority cell of either plane is here, whatever its pixel.
            let mut shade = usize::from((a | b) & 0x80 == 0);
            let mut c = plane_front(b, a);
            if s & 15 != 0 && (s >= 0x80 || c < 0x80) {
                match s & 0x3F {
                    0x3E => shade = if shade == 1 { 0 } else { 2 },
                    0x3F => shade = 1,
                    _ => {
                        c = s;
                        if s >= 0x80 || s & 15 == 14 {
                            shade = 0;
                        }
                    }
                }
            }
            *o = pal[shade][if c & 15 == 0 { bg } else { usize::from(c & 0x3F) }];
        }
    }
    if r[0] & 0x20 != 0 {
        out[..8].fill(rgb(m.cram[bg], 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: usize = 0xC000;
    const B: usize = 0xE000;
    const W: usize = 0xB000;
    const HS: usize = 0xA800;
    const UNTOUCHED: u32 = 0xFFFF_FFFF;

    /// A chip memory: 320 pixels wide, planes of 64 x 32 cells, every colour different.
    /// Pattern 1 is a ramp (first row colours 1-8 left to right, row `y` below it all colour
    /// `8 + y`); patterns 2-7 are filled with the colour of their number.
    struct Fx {
        regs: [u8; 24],
        vram: Vec<u8>,
        cram: [u16; 64],
        vsram: [u16; 40],
        /// Name table width in cells, for `cell`.
        hcells: usize,
    }

    impl Fx {
        fn new() -> Fx {
            let mut regs = [0u8; 24];
            regs[2] = 0x30;
            regs[3] = 0x2C;
            regs[4] = 0x07;
            regs[12] = 0x81;
            regs[13] = 0x2A;
            regs[16] = 0x01;
            let mut cram = [0u16; 64];
            for (i, c) in cram.iter_mut().enumerate() {
                *c = ((i as u16 & 7) << 1) | ((i as u16 >> 3) << 5) | 0x0800;
            }
            let mut vram = vec![0u8; 0x10000];
            vram[32..36].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
            for y in 1..8 {
                vram[32 + y * 4..36 + y * 4].fill((8 + y as u8) * 0x11);
            }
            for p in 2..8 {
                vram[p * 32..p * 32 + 32].fill(p as u8 * 0x11);
            }
            Fx { regs, vram, cram, vsram: [0; 40], hcells: 64 }
        }

        fn mem(&self) -> Mem<'_> {
            Mem { regs: &self.regs, vram: &self.vram, cram: &self.cram, vsram: &self.vsram }
        }

        fn poke(&mut self, addr: usize, w: u16) {
            self.vram[addr..addr + 2].copy_from_slice(&w.to_be_bytes());
        }

        fn cell(&mut self, base: usize, col: usize, row: usize, entry: u16) {
            self.poke(base + (row * self.hcells + col) * 2, entry);
        }

        fn draw_with(&self, row: u32, sprites: &SpriteRow) -> [u32; 320] {
            let mut out = [UNTOUCHED; 320];
            draw_row(&self.mem(), row, sprites, &mut out);
            out
        }

        fn draw(&self, row: u32) -> [u32; 320] {
            self.draw_with(row, &[0; 320])
        }

        /// Colour `index` of the palette at normal brightness.
        fn col(&self, index: usize) -> u32 {
            rgb(self.cram[index], 0)
        }
    }

    #[test]
    fn rgb_levels() {
        assert_eq!(rgb(0x0000, 0), 0x000000);
        assert_eq!(rgb(0x0EEE, 0), 0xFFFFFF);
        assert_eq!(rgb(0x0002, 0), 0x340000);
        assert_eq!(rgb(0x0040, 0), 0x005700);
        assert_eq!(rgb(0x0600, 0), 0x000074);
        assert_eq!(rgb(0x0A86, 0), 0xAC_90_74 >> 16 | 0x00_90_00 | 0x74 << 16);
        assert_eq!(rgb(0x0EEE, 1), 0x828282);
        assert_eq!(rgb(0x0002, 1), 0x1D0000);
        assert_eq!(rgb(0x0C00, 1), 0x000074);
        assert_eq!(rgb(0x0000, 2), 0x828282);
        assert_eq!(rgb(0x0EEE, 2), 0xFFFFFF);
        assert_eq!(rgb(0x0020, 2), 0x829082);
        for v in 0..8u16 {
            let e = v << 1 | v << 5 | v << 9;
            assert_eq!(rgb(e, 0), u32::from(NORMAL[v as usize]) * 0x010101);
            assert_eq!(rgb(e, 1), u32::from(SHADOW[v as usize]) * 0x010101);
            assert_eq!(rgb(e, 2), u32::from(HIGHLIGHT[v as usize]) * 0x010101);
        }
        // Bits outside the nine colour bits are ignored.
        assert_eq!(rgb(0xF111, 0), 0);
    }

    #[test]
    fn one_tile_at_its_cell() {
        let mut f = Fx::new();
        f.cell(A, 3, 2, 0x2001);
        let bg = f.col(0);
        let o = f.draw(16);
        for x in 0..320 {
            let want = if (24..32).contains(&x) { f.col(16 + 1 + x - 24) } else { bg };
            assert_eq!(o[x], want, "x {x}");
        }
        let o = f.draw(17);
        assert_eq!(o[23], bg);
        assert!(o[24..32].iter().all(|&c| c == f.col(16 + 9)));
        assert_eq!(o[32], bg);
        assert_eq!(f.draw(23)[24], f.col(16 + 15));
        assert!(f.draw(15).iter().all(|&c| c == bg));
        assert!(f.draw(24).iter().all(|&c| c == bg));
        // The same in plane B.
        let mut f = Fx::new();
        f.cell(B, 39, 27, 0x6001);
        let o = f.draw(27 * 8);
        assert_eq!(o[311], bg);
        for i in 0..8 {
            assert_eq!(o[312 + i], f.col(48 + 1 + i));
        }
    }

    #[test]
    fn flips() {
        let mut f = Fx::new();
        f.cell(A, 0, 0, 0x0801);
        f.cell(A, 2, 0, 0x1001);
        f.cell(A, 4, 0, 0x1801);
        let o = f.draw(0);
        for i in 0..8 {
            assert_eq!(o[i], f.col(8 - i));
            assert_eq!(o[16 + i], f.col(15));
            assert_eq!(o[32 + i], f.col(15));
        }
        let o = f.draw(7);
        for i in 0..8 {
            assert_eq!(o[i], f.col(15));
            assert_eq!(o[16 + i], f.col(1 + i));
            assert_eq!(o[32 + i], f.col(8 - i));
        }
        assert_eq!(f.draw(6)[16], f.col(9));
    }

    #[test]
    fn plane_and_window_priority() {
        let mut f = Fx::new();
        f.regs[7] = 0x2B;
        // Cell 0: both low. Cell 1: B high, A low. Cell 2: both high. Cell 3: B only. Cell 4: A high, clear pixels.
        f.cell(B, 0, 0, 0x0002);
        f.cell(A, 0, 0, 0x2003);
        f.cell(B, 1, 0, 0x8002);
        f.cell(A, 1, 0, 0x2003);
        f.cell(B, 2, 0, 0x8002);
        f.cell(A, 2, 0, 0xA003);
        f.cell(B, 3, 0, 0x0002);
        f.cell(B, 4, 0, 0x0002);
        f.cell(A, 4, 0, 0x8000);
        let o = f.draw(0);
        assert_eq!(o[0], f.col(16 + 3));
        assert_eq!(o[8], f.col(2));
        assert_eq!(o[16], f.col(16 + 3));
        assert_eq!(o[24], f.col(2));
        assert_eq!(o[32], f.col(2));
        assert_eq!(o[40], f.col(0x2B));

        // A window left of column 16 replaces plane A there and does not scroll.
        f.regs[17] = 1;
        f.cell(W, 0, 0, 0x4004);
        f.poke(HS, 5);
        f.vsram[0] = 3;
        let o = f.draw(0);
        assert!(o[0..8].iter().all(|&c| c == f.col(32 + 4)));
        assert!(o[8..16].iter().all(|&c| c == f.col(2)));
        // Plane A from x = 16 on, 5 pixels right and 3 rows up: cell 2 covers 21..29 of rows 0-4.
        assert_eq!(o[20], f.col(2));
        assert_eq!(o[21], f.col(16 + 3));
        // A high-priority window over a high-priority B.
        f.cell(W, 1, 0, 0xC004);
        assert_eq!(f.draw(0)[8], f.col(32 + 4));
        // Right of column 304.
        f.regs[17] = 0x80 | 19;
        f.cell(W, 38, 0, 0x0005);
        let o = f.draw(0);
        assert_eq!(o[303], f.col(0x2B));
        assert!(o[304..312].iter().all(|&c| c == f.col(5)));
        assert_eq!(o[0], f.col(2));
        // Above row 8: the whole of rows 0-7, none of row 8.
        f.regs[17] = 0;
        f.regs[18] = 1;
        let o = f.draw(7);
        assert_eq!(o[0], f.col(32 + 4));
        assert_eq!(o[304], f.col(5));
        assert_eq!(f.draw(8)[304], f.col(0x2B));
        // From row 8 down.
        f.regs[18] = 0x81;
        f.cell(W, 5, 1, 0x0006);
        assert_eq!(f.draw(7)[304], f.col(0x2B));
        assert_eq!(f.draw(8)[40], f.col(6));
    }

    #[test]
    fn horizontal_scroll_modes() {
        let mut f = Fx::new();
        f.cell(A, 0, 0, 0x0002);
        f.cell(A, 0, 1, 0x0002);
        f.cell(B, 1, 0, 0x0003);
        // Whole plane: the first table entry, A then B.
        f.poke(HS, 4);
        f.poke(HS + 2, 0x3F8);
        f.poke(HS + 5 * 4, 100);
        let o = f.draw(5);
        assert_eq!(o[3], f.col(3));
        assert!(o[4..12].iter().all(|&c| c == f.col(2)));
        assert_eq!(o[12], f.col(0));
        // Per row.
        f.regs[11] = 3;
        f.poke(HS + 5 * 4, 9);
        f.poke(HS + 5 * 4 + 2, 16);
        let o = f.draw(5);
        assert_eq!(o[8], f.col(0));
        assert!(o[9..17].iter().all(|&c| c == f.col(2)));
        assert!(o[24..32].iter().all(|&c| c == f.col(3)));
        let o = f.draw(4);
        assert!(o[0..8].iter().all(|&c| c == f.col(2)));
        assert!(o[8..16].iter().all(|&c| c == f.col(3)));
        // Per strip of 8 rows: the entry of the strip's first row.
        f.regs[11] = 2;
        f.poke(HS + 8 * 4, 3);
        f.poke(HS + 9 * 4, 200);
        for row in [8, 9, 15] {
            let o = f.draw(row);
            assert_eq!(o[2], f.col(0));
            assert!(o[3..11].iter().all(|&c| c == f.col(2)), "row {row}");
        }
        // The plane wraps: 64 cells are 512 pixels.
        f.regs[11] = 0;
        f.poke(HS, 0x3FF);
        f.cell(A, 63, 0, 0x0004);
        let o = f.draw(0);
        assert!(o[0..7].iter().all(|&c| c == f.col(2)));
        assert_eq!(o[7], f.col(3));
        f.poke(HS, 1);
        assert_eq!(f.draw(0)[0], f.col(4));
    }

    #[test]
    fn vertical_scroll_whole_and_per_column() {
        let mut f = Fx::new();
        for col in 3..7 {
            f.cell(A, col, 1, 0x0002);
        }
        f.cell(B, 0, 31, 0x0003);
        f.vsram[0] = 8;
        f.vsram[1] = 0x3FF;
        f.vsram[4] = 100;
        let o = f.draw(0);
        assert!(o[24..56].iter().all(|&c| c == f.col(2)));
        // Plane B: one row down, so its last row wraps to the top.
        assert_eq!(o[0], f.col(3));
        assert_eq!(f.draw(1)[0], f.col(0));
        // Per two-cell column: column 2 (x 32..48) scrolls for plane A, column 0 for plane B.
        f.regs[11] = 4;
        f.vsram[0] = 0;
        f.vsram[4] = 8;
        f.vsram[6] = 4;
        let o = f.draw(0);
        assert_eq!(o[0], f.col(3));
        assert!(o[24..32].iter().all(|&c| c == f.col(0)));
        assert!(o[32..48].iter().all(|&c| c == f.col(2)));
        assert!(o[48..56].iter().all(|&c| c == f.col(0)));
        let o = f.draw(8);
        assert!(o[32..48].iter().all(|&c| c == f.col(0)));
        assert!(o[48..56].iter().all(|&c| c == f.col(2)));
        assert_eq!(o[56], f.col(0));
        // The columns move with the fine horizontal scroll.
        f.poke(HS, 3);
        let o = f.draw(0);
        assert_eq!(o[34], f.col(0));
        assert!(o[35..51].iter().all(|&c| c == f.col(2)));
        assert_eq!(o[51], f.col(0));
    }

    #[test]
    fn width_256() {
        let mut f = Fx::new();
        f.regs[12] = 0;
        f.regs[16] = 0;
        f.hcells = 32;
        f.cell(A, 31, 0, 0x0002);
        f.cell(W, 31, 1, 0x0003);
        let o = f.draw(0);
        assert_eq!(o[247], f.col(0));
        assert!(o[248..256].iter().all(|&c| c == f.col(2)));
        assert!(o[256..].iter().all(|&c| c == UNTOUCHED));
        // 32 cells wrap at 256 pixels.
        f.poke(HS, 8);
        let o = f.draw(0);
        assert!(o[0..8].iter().all(|&c| c == f.col(2)));
        assert_eq!(o[248], f.col(0));
        // The window table is 32 cells wide and its base keeps bit 1 of register 3.
        f.regs[3] = 0x2E;
        f.regs[18] = 0x81;
        f.cell(W + 0x800, 31, 1, 0x0004);
        assert!(f.draw(8)[248..256].iter().all(|&c| c == f.col(4)));
        f.regs[12] = 0x81;
        f.hcells = 64;
        f.cell(W, 2, 1, 0x0005);
        assert_eq!(f.draw(8)[16], f.col(5));
    }

    #[test]
    fn plane_sizes() {
        assert_eq!(plane_size(0x00), (32, 32));
        assert_eq!(plane_size(0x11), (64, 64));
        assert_eq!(plane_size(0x10), (32, 64));
        assert_eq!(plane_size(0x30), (32, 128));
        assert_eq!(plane_size(0x03), (128, 32));
        assert_eq!(plane_size(0x33), (128, 32));
        assert_eq!(plane_size(0x31), (64, 64));
        assert_eq!(plane_size(0x22), (32, 32));
        // 128 cells across: cell 100 of row 0 is reachable by scrolling.
        let mut f = Fx::new();
        f.regs[16] = 0x03;
        f.hcells = 128;
        f.cell(A, 100, 0, 0x0002);
        f.poke(HS, 0x3FF & (1024 - 800));
        assert_eq!(f.draw(0)[0], f.col(2));
    }

    #[test]
    fn interlace_mode_2() {
        let mut f = Fx::new();
        f.regs[12] = 0x87;
        // Pattern 1 is now 16 rows at 64..128: row 9 is colour 5.
        f.vram[32..128].fill(0);
        f.vram[64 + 9 * 4..64 + 10 * 4].fill(0x55);
        f.cell(A, 0, 0, 0x0001);
        f.cell(A, 1, 0, 0x1001);
        f.cell(A, 2, 1, 0x0001);
        assert!(f.draw(9)[0..8].iter().all(|&c| c == f.col(5)));
        assert_eq!(f.draw(8)[0], f.col(0));
        assert!(f.draw(6)[8..16].iter().all(|&c| c == f.col(5)));
        assert_eq!(f.draw(25)[16], f.col(5));
        // Vertical scroll counts half lines; the scroll table and the window count lines.
        f.vsram[0] = 3;
        assert_eq!(f.draw(6)[0], f.col(5));
        f.vsram[0] = 0;
        f.regs[11] = 3;
        f.poke(HS + 4 * 4, 2);
        assert_eq!(f.draw(9)[1], f.col(0));
        assert_eq!(f.draw(9)[2], f.col(5));
        f.regs[18] = 1;
        assert_eq!(f.draw(9)[2], f.col(0));
        assert_eq!(f.draw(25)[16], f.col(5));
    }

    #[test]
    fn shadow_on_plane_pixels() {
        let mut f = Fx::new();
        f.regs[12] = 0x89;
        f.regs[7] = 0x11;
        f.cell(A, 0, 0, 0x0002);
        f.cell(A, 1, 0, 0x8002);
        f.cell(B, 2, 0, 0x8000);
        f.cell(B, 3, 0, 0x8003);
        f.cell(A, 3, 0, 0x0002);
        let o = f.draw(0);
        assert_eq!(o[0], rgb(f.cram[2], 1));
        assert_eq!(o[8], rgb(f.cram[2], 0));
        // A high-priority cell lifts the shadow even where its pixels are transparent.
        assert_eq!(o[16], rgb(f.cram[0x11], 0));
        assert_eq!(o[24], rgb(f.cram[3], 0));
        assert_eq!(o[32], rgb(f.cram[0x11], 1));
        // Without the mode nothing is shadowed.
        f.regs[12] = 0x81;
        assert_eq!(f.draw(0)[0], f.col(2));
        assert_eq!(f.draw(0)[32], f.col(0x11));
    }

    #[test]
    fn sprites_in_the_priority_order() {
        let mut f = Fx::new();
        f.cell(B, 0, 0, 0x0002);
        f.cell(A, 1, 0, 0x0003);
        f.cell(B, 2, 0, 0x8002);
        f.cell(A, 3, 0, 0x8003);
        let mut s: SpriteRow = [0; 320];
        s[0..40].fill(0x25);
        s[4] = 0;
        s[26] = 0xA5;
        s[39] = 0x3E;
        let o = f.draw_with(0, &s);
        assert_eq!(o[0], f.col(0x25));
        assert_eq!(o[4], f.col(2));
        assert_eq!(o[8], f.col(0x25));
        assert_eq!(o[16], f.col(2));
        assert_eq!(o[24], f.col(3));
        assert_eq!(o[26], f.col(0x25));
        assert_eq!(o[32], f.col(0x25));
        // Without shadow mode, colours 14 and 15 of line 3 are plain colours.
        assert_eq!(o[39], f.col(0x3E));
    }

    #[test]
    fn shadow_and_highlight_operators() {
        let mut f = Fx::new();
        f.regs[12] = 0x89;
        f.cell(A, 0, 0, 0x0002);
        f.cell(A, 1, 0, 0x8002);
        let mut s: SpriteRow = [0; 320];
        // Over a low plane pixel (shadowed).
        s[0] = 0x3E;
        s[1] = 0x3F;
        s[2] = 0x15;
        s[3] = 0x1E;
        s[4] = 0x95;
        s[5] = 0xBE;
        s[6] = 0x3D;
        // Over a high plane pixel (normal).
        s[8] = 0x3E;
        s[9] = 0x3F;
        s[10] = 0xBE;
        s[11] = 0xBF;
        s[12] = 0x15;
        s[13] = 0x95;
        // Over the background.
        s[16] = 0x3E;
        s[17] = 0xBF;
        let o = f.draw_with(0, &s);
        assert_eq!(o[0], rgb(f.cram[2], 0));
        assert_eq!(o[1], rgb(f.cram[2], 1));
        assert_eq!(o[2], rgb(f.cram[0x15], 1));
        assert_eq!(o[3], rgb(f.cram[0x1E], 0));
        assert_eq!(o[4], rgb(f.cram[0x15], 0));
        assert_eq!(o[5], rgb(f.cram[2], 0));
        assert_eq!(o[6], rgb(f.cram[0x3D], 1));
        assert_eq!(o[7], rgb(f.cram[2], 1));
        // Low-priority sprite pixels are behind the high plane pixel: no effect.
        assert_eq!(o[8], rgb(f.cram[2], 0));
        assert_eq!(o[9], rgb(f.cram[2], 0));
        assert_eq!(o[10], rgb(f.cram[2], 2));
        assert_eq!(o[11], rgb(f.cram[2], 1));
        assert_eq!(o[12], rgb(f.cram[2], 0));
        assert_eq!(o[13], rgb(f.cram[0x15], 0));
        assert_eq!(o[16], rgb(f.cram[0], 0));
        assert_eq!(o[17], rgb(f.cram[0], 1));
        assert_eq!(o[18], rgb(f.cram[0], 1));
    }

    #[test]
    fn left_column_blank() {
        let mut f = Fx::new();
        f.regs[0] = 0x20;
        f.regs[7] = 0x07;
        f.cell(A, 0, 0, 0x8002);
        f.cell(A, 1, 0, 0x8002);
        let mut s: SpriteRow = [0; 320];
        s[0..16].fill(0x85);
        let o = f.draw(0);
        assert!(o[0..8].iter().all(|&c| c == f.col(7)));
        assert!(o[8..16].iter().all(|&c| c == f.col(2)));
        assert_eq!(o[16], f.col(7));
        let o = f.draw_with(0, &s);
        assert!(o[0..8].iter().all(|&c| c == f.col(7)));
        assert_eq!(o[8], f.col(5));
        // The blanked column is at normal brightness in shadow mode.
        f.regs[12] = 0x89;
        f.cell(A, 0, 0, 0);
        assert_eq!(f.draw(0)[0], f.col(7));
    }

    #[test]
    fn sprites_through_render_row() {
        let mut f = Fx::new();
        // Sprite table at 0xF000: one 8x8 sprite of pattern 1, palette line 2, high priority, at the top left.
        f.regs[5] = 0x78;
        f.poke(0xF000, 128);
        f.poke(0xF002, 0);
        f.poke(0xF004, 0xC001);
        f.poke(0xF006, 128);
        f.cell(A, 0, 0, 0x8002);
        let m = f.mem();
        let mut out = [UNTOUCHED; 320];
        let flags = render_row(&m, 0, Some(&mut out));
        for i in 0..8 {
            assert_eq!(out[i], f.col(32 + 1 + i));
        }
        assert_eq!(out[8], f.col(0));
        assert_eq!(flags, SpriteFlags::default());
        assert_eq!(render_row(&m, 0, None), flags);
        let mut out = [UNTOUCHED; 320];
        render_row(&m, 8, Some(&mut out));
        assert_eq!(out[0], f.col(0));
    }
}
