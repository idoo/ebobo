//! Where the beam is: H and V counter values, blanking ranges, the points of a line where
//! interrupts are raised and a row is drawn, how long a DMA takes. Pure functions of the
//! position and the mode. Owner: vdp slice (with `vdp.rs`).
//!
//! A line is [`LINE_CLOCKS`] master clocks and starts where the H counter reads 0x00, the left
//! edge of the picture. Positions inside a line are master clocks from that point.

/// Master clocks per line.
pub const LINE_CLOCKS: u32 = crate::MCLK_PER_LINE as u32;

/// The point of a line whose state its row is drawn from: the left edge of the picture. The vertical
/// scroll is the exception: it is taken at the previous line's [`hint_point`] (`row_point` probe).
pub const ROW_DRAW_POINT: u32 = 0;

/// Lines per frame.
pub fn lines(pal: bool) -> u32 {
    if pal { 313 } else { 262 }
}

/// Pixel of the line (0-341 in the 256-pixel mode, 0-419 in the 320-pixel mode) the beam is
/// in at master clock `mclk` of the line.
///
/// A 256-mode pixel is 10 master clocks. A 320-mode pixel is 8, except during horizontal
/// sync, where the chip's pixel clock slows down so that the line still lasts 3420 master
/// clocks: from the ninth-bit counter value 0x1CD on, 15 pixels of 10, 2 of 8, 15 of 10, 2 of 8.
fn pixel(mclk: u32, h40: bool) -> u32 {
    if !h40 {
        return mclk / 10;
    }
    // Pixels 0-368 (counter 0x000-0x16C, then 0x1C9-0x1CC) are 8 master clocks each.
    const STRETCH_PX: u32 = 369;
    const STRETCH_AT: u32 = STRETCH_PX * 8;
    if mclk < STRETCH_AT {
        return mclk / 8;
    }
    let m = mclk - STRETCH_AT;
    STRETCH_PX
        + match m {
            0..150 => m / 10,
            150..166 => 15 + (m - 150) / 8,
            166..316 => 17 + (m - 166) / 10,
            316..332 => 32 + (m - 316) / 8,
            _ => 34 + (m - 332) / 8,
        }
}

/// H counter as the port returns it (the upper 8 of its 9 bits) at master clock `mclk`
/// (0..3420) of the line. 256-pixel mode: 0x00-0x93, then 0xE9-0xFF. 320-pixel mode:
/// 0x00-0xB6, then 0xE4-0xFF.
pub fn h_counter(mclk: u32, h40: bool) -> u8 {
    let px = pixel(mclk.min(LINE_CLOCKS - 1), h40);
    let (last, resume) = if h40 { (0x16C, 0x1C9) } else { (0x127, 0x1D2) };
    let c = if px <= last { px } else { px - last - 1 + resume };
    (c >> 1) as u8
}

/// Master clock of the line at which the H counter (8 bits, before its jump) reaches `hc`.
const fn at_h(hc: u32, h40: bool) -> u32 {
    hc * 2 * if h40 { 8 } else { 10 }
}

/// Horizontal blanking flag: set from `hblank_start` to the end of the line and from the
/// start of the next to `hblank_end` (H counter 0x8F to 0x01 in the 256-pixel mode, 0xAF to 0x01
/// in the 320-pixel mode). Measured against the reference with the `blank_edges` probe; the
/// documented 0x93/0xB3 to 0x05/0x06 read about 4 H counts late on both edges in both widths.
pub fn hblank_start(h40: bool) -> u32 {
    at_h(if h40 { 0xAF } else { 0x8F }, h40)
}

/// See [`hblank_start`].
pub fn hblank_end(h40: bool) -> u32 {
    at_h(0x01, h40)
}

/// Where on the first line of vertical blanking the vertical interrupt is requested
/// (H counter 0x02).
pub fn vint_point(h40: bool) -> u32 {
    at_h(0x02, h40)
}

/// Where on a line the V counter moves on to the next line's value and the horizontal
/// interrupt counter is stepped (H counter 0x85, or 0xA5 in 320-pixel mode: just after the
/// picture's right edge).
pub fn hint_point(h40: bool) -> u32 {
    at_h(if h40 { 0xA5 } else { 0x85 }, h40)
}

/// V counter as the port returns it for line `line` (0 = first displayed line).
///
/// The 9-bit counter runs 0x000-0x0EA then 0x1E5-0x1FF (NTSC), 0x000-0x102 then 0x1CA-0x1FF
/// (PAL, 224 lines), 0x000-0x10A then 0x1D2-0x1FF (PAL, 240 lines); NTSC with 240 lines never
/// jumps (0x000-0x105). `interlace`: register 12 bits 2-1. In interlace mode 2 (3) the counter
/// is doubled; in both interlace modes bit 0 of the value read is bit 8 of the counter.
pub fn v_counter(line: u32, pal: bool, v30: bool, interlace: u8) -> u8 {
    let (last, resume) = match (pal, v30) {
        (false, false) => (0x0EA, 0x1E5),
        (false, true) => (0x105, 0x1FA),
        (true, false) => (0x102, 0x1CA),
        (true, true) => (0x10A, 0x1D2),
    };
    let mut c = if line <= last { line } else { line - last - 1 + resume };
    if interlace == 3 {
        c <<= 1;
    }
    if interlace & 1 != 0 {
        c = (c & 0xFE) | ((c >> 8) & 1);
    }
    c as u8
}

/// What a DMA moves.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DmaKind {
    /// 68000 memory to video memory.
    Bus,
    /// Video RAM fill.
    Fill,
    /// Video RAM copy.
    Copy,
}

/// Bytes a DMA moves per line (for a 68k transfer to colour or scroll RAM: words). A fill moves a
/// byte per slot like a 68k transfer while drawing (`dma_rate` probe: 256 bytes in 14.45 lines of
/// the 320-pixel mode, 16.25 of the 256-pixel mode).
pub fn dma_rate(kind: DmaKind, h40: bool, blanked: bool) -> u32 {
    match (kind, h40, blanked) {
        (DmaKind::Bus, false, false) => 16,
        (DmaKind::Bus, false, true) => 167,
        (DmaKind::Bus, true, false) => 18,
        (DmaKind::Bus, true, true) => 205,
        (DmaKind::Fill, false, false) => 16,
        (DmaKind::Fill, false, true) => 166,
        (DmaKind::Fill, true, false) => 18,
        (DmaKind::Fill, true, true) => 204,
        (DmaKind::Copy, false, false) => 8,
        (DmaKind::Copy, false, true) => 83,
        (DmaKind::Copy, true, false) => 9,
        (DmaKind::Copy, true, true) => 102,
    }
}

/// The beam and the mode a DMA starts in.
#[derive(Clone, Copy)]
pub struct DmaStart {
    pub line: u32,
    /// Master clock inside the line.
    pub mclk: u32,
    pub h40: bool,
    /// Register 1 bit 6.
    pub display_on: bool,
    /// Displayed lines: 224 or 240.
    pub active_lines: u32,
    /// Lines per frame.
    pub total_lines: u32,
}

/// Master clocks a DMA of `units` (bytes; words for a 68k transfer to colour or scroll RAM)
/// takes: line by line at the rate of [`dma_rate`], a line being blanked when the display is
/// off or the line is outside the picture. The part of a line that is left gets its share of
/// the line's rate.
pub fn dma_duration(kind: DmaKind, units: u32, s: DmaStart) -> u64 {
    let mut left = units as u64;
    let mut line = s.line % s.total_lines;
    let mut avail = (LINE_CLOCKS - s.mclk.min(LINE_CLOCKS)) as u64;
    let mut t = 0u64;
    loop {
        let rate = dma_rate(kind, s.h40, !s.display_on || line >= s.active_lines) as u64;
        let cap = rate * avail / LINE_CLOCKS as u64;
        if left <= cap {
            return t + (left * LINE_CLOCKS as u64).div_ceil(rate);
        }
        left -= cap;
        t += avail;
        avail = LINE_CLOCKS as u64;
        line = (line + 1) % s.total_lines;
    }
}

/// Units of a DMA started at `s` that are done `elapsed` master clocks later: the inverse of
/// [`dma_duration`] (the largest count whose duration is at most `elapsed`), stepped line by line.
pub fn dma_units_done(kind: DmaKind, s: DmaStart, elapsed: u64) -> u32 {
    let lc = LINE_CLOCKS as u64;
    let mut line = s.line % s.total_lines;
    let mut avail = (LINE_CLOCKS - s.mclk.min(LINE_CLOCKS)) as u64;
    let mut t = 0u64;
    let mut done = 0u64;
    loop {
        let rate = dma_rate(kind, s.h40, !s.display_on || line >= s.active_lines) as u64;
        if elapsed < t + avail {
            return (done + (elapsed - t) * rate / lc) as u32;
        }
        done += rate * avail / lc;
        t += avail;
        avail = lc;
        line = (line + 1) % s.total_lines;
    }
}

/// Access slots of a drawing line in the 320-pixel mode: master clock of the line of each of the 18.
/// Measured with the `fifo_pace` probe (when a held data-port write resumes, video RAM words from
/// H 0x10, 0x80 and 0xE8): the slots crowd near both edges of the picture and are sparse in its
/// middle, one falls in horizontal blanking.
const DRAW_SLOTS_H40: [u16; 18] = [32, 144, 256, 432, 624, 720, 816, 1008, 1200, 1392, 1584, 1840, 2096, 2176, 2304, 2464, 2608, 3138];

/// As [`DRAW_SLOTS_H40`], the 16 slots of a drawing line in the 256-pixel mode.
const DRAW_SLOTS_H32: [u16; 16] = [380, 560, 740, 1020, 1320, 1460, 1600, 1940, 2280, 2400, 2560, 2720, 2880, 3020, 3180, 3340];

/// Master clocks from the position `s` to the `n`-th access slot after it (`n` ≥ 1): the slots in
/// which the chip takes a write from its FIFO. A drawing line has the slots of [`DRAW_SLOTS_H40`]
/// / [`DRAW_SLOTS_H32`]; a blanked line (display off or outside the picture) as many as a 68k DMA
/// moves units ([`dma_rate`] of [`DmaKind::Bus`]: 205 in the 320-pixel mode, 167 in the 256-pixel
/// mode), spread evenly: slot `k` of a line with `r` slots is at master clock `(2k + 1) * 3420 / 2r`.
pub fn slot_after(n: u32, s: DmaStart) -> u64 {
    let lc = LINE_CLOCKS as u64;
    let at = s.mclk.min(LINE_CLOCKS - 1) as u64;
    let mut line = s.line % s.total_lines;
    let mut base = 0u64;
    let mut from = at + 1;
    let mut left = n.max(1);
    loop {
        if s.display_on && line < s.active_lines {
            let table: &[u16] = if s.h40 { &DRAW_SLOTS_H40 } else { &DRAW_SLOTS_H32 };
            for &p in table {
                let pos = p as u64;
                if pos >= from {
                    left -= 1;
                    if left == 0 {
                        return base + pos - at;
                    }
                }
            }
        } else {
            let rate = dma_rate(DmaKind::Bus, s.h40, true) as u64;
            let mut k = from * rate / lc;
            while k < rate {
                let pos = (2 * k + 1) * lc / (2 * rate);
                if pos >= from {
                    left -= 1;
                    if left == 0 {
                        return base + pos - at;
                    }
                }
                k += 1;
            }
        }
        base += lc;
        from = 0;
        line = (line + 1) % s.total_lines;
    }
}
