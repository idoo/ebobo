//! Video chip (315-5313): state, ports, DMA, interrupts, HV counter. Owner: vdp slice.
//!
//! Contract with `system.rs`:
//! - Time is master clocks since power-on (`u64`), never going backwards. The VDP keeps its
//!   own position (line, clock within the line) and derives everything from it.
//! - The caller brings the chip up to date with [`Vdp::run_to`] before every [`Vdp::read`]
//!   and [`Vdp::write`], and before it looks at [`Vdp::irq_level`], [`Vdp::z80_irq`] or
//!   [`Vdp::take_frame_done`].
//! - A row is drawn from the state the chip has at one fixed point of its line, so a
//!   register or memory write lands between two rows. Drawing is `vdp_render::render_row`
//!   over a [`crate::vdp_mem::Mem`] view of this chip; a blanked display (register 1 bit 6
//!   off) is filled with the background colour here.
//! - Pixels go to a [`Screen`], which is not emulated state. With `None` nothing is drawn,
//!   and every bit of emulated state (the sprite overflow and collision flags included)
//!   comes out the same.
//! - Positions, counter values and DMA durations come from `vdp_timing.rs`.
//!
//! Line model: line 0 is the first displayed line; a line starts where the H counter reads
//! 0x00. Three things happen at fixed points of a line, each at most once per line: the row
//! is drawn (displayed lines), the vertical interrupt is raised (first blanked line only),
//! and the V counter moves on together with the horizontal interrupt counter.

use serde::{Deserialize, Serialize};

use crate::vdp_mem::{Mem, SpriteFlags};
use crate::vdp_timing::{self as timing, DmaKind, DmaStart, LINE_CLOCKS};
use crate::{MAX_H, MAX_W};

/// The picture: `w`×`h` pixels, `0x00RRGGBB`, row by row. Not part of the emulated state.
pub struct Screen {
    /// 256 or 320.
    pub w: usize,
    /// 224 or 240, doubled in interlace mode 2.
    pub h: usize,
    /// `MAX_W * MAX_H` entries; the picture is the top-left `w`×`h`, rows `MAX_W` apart.
    pub px: Vec<u32>,
}

impl Screen {
    pub fn new() -> Screen {
        Screen { w: 320, h: 224, px: vec![0; MAX_W * MAX_H] }
    }
}

impl Default for Screen {
    fn default() -> Screen {
        Screen::new()
    }
}

/// 68000 memory as a 68k→VDP DMA reads it.
pub trait DmaSource {
    /// `addr` is an even 24-bit 68000 address.
    fn dma_read16(&mut self, addr: u32) -> u16;
}

/// Bits of `Vdp::fired`: line points already passed on the current line.
const FIRED_ROW: u8 = 1;
const FIRED_VINT: u8 = 2;
const FIRED_HINT: u8 = 4;

/// A DMA on its way: the data moves one unit per access slot (a byte of video RAM, a word of
/// colour or scroll RAM), unit `k` at the master clock [`timing::dma_duration`] gives for `k + 1`
/// units from the start. Everything is zero / empty when no transfer is pending.
#[derive(Clone, Default, Hash, Serialize, Deserialize)]
struct DmaPending {
    /// 0 none, 1 68000 transfer, 2 fill, 3 copy.
    kind: u8,
    /// Command code CD3-CD0 at the trigger: the memory written.
    target: u8,
    /// Source words of a 68000 transfer, read at the trigger (the 68000 is held for all of it).
    words: Vec<u16>,
    /// Units written / units in all.
    done: u32,
    total: u32,
    /// Where the next unit goes, and the step after each (register 15 at the trigger).
    addr: u16,
    inc: u16,
    /// Copy: where the next byte comes from.
    src: u16,
    /// Fill: the data word.
    fill: u16,
    /// Master clocks of the trigger and of the last unit.
    t0: u64,
    end: u64,
    /// Beam and mode at the trigger ([`DmaStart`]).
    line: u32,
    mclk: u32,
    h40: bool,
    display_on: bool,
    active_lines: u32,
    total_lines: u32,
}

/// Complete emulated state of the chip.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Vdp {
    regs: [u8; 24],
    /// 65536 bytes.
    vram: Vec<u8>,
    /// 64 entries, `0000BBB0GGG0RRR0`.
    cram: Vec<u16>,
    /// 40 entries, 11 bits each.
    vsram: Vec<u16>,
    /// The vertical scroll the next row is drawn with: `vsram` as it stood at the previous line's
    /// `hint_point` (the reference takes it there, not at the row's own start; `row_point` probe).
    row_vsram: Vec<u16>,
    /// The first word of a two-word command was written.
    pending: bool,
    /// Command code CD5-CD0.
    code: u8,
    /// Command address A15-A0.
    addr: u16,
    /// Read-ahead buffer of the data port.
    read_buf: u16,
    /// A VRAM fill waits for the next data write.
    fill_armed: bool,
    sprite_overflow: bool,
    sprite_collision: bool,
    odd_frame: bool,
    pal: bool,
    /// Master clock of the chip's position.
    clock: u64,
    line: u32,
    /// Master clock inside the line, 0..3420.
    mclk: u32,
    /// `FIRED_*` points passed on this line.
    fired: u8,
    vint_pending: bool,
    hint_pending: bool,
    hint_counter: u8,
    /// The Z80 /INT line is low before this master clock.
    z80_int_until: u64,
    frame_done: bool,
    m68k_frozen_until: u64,
    dma_busy_until: u64,
    /// The transfer whose data is still on its way.
    dma: DmaPending,
    /// Write FIFO: the master clocks at which the last four data-port writes leave it, oldest
    /// first (a video RAM word leaves with its first byte; the second is written from a latch
    /// in the next slot). A write is waiting while its entry is after `clock`.
    fifo_done: [u64; 4],
    /// Master clock of the last slot the newest FIFO write uses.
    fifo_tail: u64,
    /// HV counter value latched while register 0 bit 1 is on.
    hv_latch: Option<u16>,
}

impl Vdp {
    /// Power-on state. `pal`: 313 lines at 50 Hz instead of 262 at 60 Hz.
    /// The chip powers on part-way through a frame, not at line 0: the reference machine's first
    /// vertical blanking comes 32 lines (NTSC) / 58 lines (PAL) after the 68000's first instruction
    /// (probes `boot_phase`, `boot_clock`), so the counters start at line 191 + 780 clocks
    /// (NTSC) / 164 + 720 clocks (PAL); the line points before that position count as passed.
    /// Register 10 (lines between horizontal interrupts) powers on as 0xFF, the others as 0: the
    /// reference latches no horizontal request on the lines before a program first writes it
    /// (probes `int_boot`, `hint_lines`); with 0 a request would wait from the first line on and
    /// fire the moment a game enables the interrupt.
    pub fn new(pal: bool) -> Vdp {
        let (line, mclk) = if pal { (164, 720) } else { (191, 780) };
        let mut v = Vdp {
            regs: [0; 24],
            vram: vec![0; 0x10000],
            cram: vec![0; 64],
            vsram: vec![0; 40],
            row_vsram: vec![0; 40],
            pending: false,
            code: 0,
            addr: 0,
            read_buf: 0,
            fill_armed: false,
            sprite_overflow: false,
            sprite_collision: false,
            odd_frame: false,
            pal,
            clock: 0,
            line,
            mclk,
            fired: 0,
            vint_pending: false,
            hint_pending: false,
            hint_counter: 0xFF,
            z80_int_until: 0,
            frame_done: false,
            m68k_frozen_until: 0,
            dma_busy_until: 0,
            dma: DmaPending::default(),
            fifo_done: [0; 4],
            fifo_tail: 0,
            hv_latch: None,
        };
        v.regs[10] = 0xFF;
        v.fired = v.points().iter().filter(|&&(_, p)| p <= mclk).fold(0, |f, &(bit, _)| f | bit);
        v
    }

    fn h40(&self) -> bool {
        self.regs[12] & 0x01 != 0
    }

    /// Displayed lines: 224, or 240 in the 30-cell mode (register 1 bit 3).
    fn active_lines(&self) -> u32 {
        if self.regs[1] & 0x08 != 0 { 240 } else { 224 }
    }

    /// Register 12 bits 2-1: 0/2 progressive, 1 interlace, 3 interlace with double height.
    fn interlace(&self) -> u8 {
        (self.regs[12] >> 1) & 3
    }

    fn display_on(&self) -> bool {
        self.regs[1] & 0x40 != 0
    }

    fn increment(&self) -> u16 {
        self.regs[15] as u16
    }

    /// Advances to master clock `now`: finishes lines (drawing each into `screen`), raises
    /// interrupt requests, moves the HV counter and the status flags.
    pub fn run_to(&mut self, now: u64, mut screen: Option<&mut Screen>) {
        loop {
            self.fire_due(screen.as_deref_mut());
            if self.clock >= now {
                self.dma_apply();
                return;
            }
            let next = self.next_point_in_line();
            let step = ((next - self.mclk) as u64).min(now - self.clock);
            self.clock += step;
            self.mclk += step as u32;
            if self.mclk >= LINE_CLOCKS {
                self.mclk = 0;
                self.fired = 0;
                self.line = (self.line + 1) % self.lines();
            }
        }
    }

    /// Line points of the current mode, in `FIRED_*` order.
    fn points(&self) -> [(u8, u32); 3] {
        let h40 = self.h40();
        [(FIRED_ROW, timing::ROW_DRAW_POINT), (FIRED_VINT, timing::vint_point(h40)), (FIRED_HINT, timing::hint_point(h40))]
    }

    /// The nearest point after the position on this line not yet passed, or the line's end.
    fn next_point_in_line(&self) -> u32 {
        self.points()
            .iter()
            .filter(|&&(bit, p)| self.fired & bit == 0 && p > self.mclk)
            .map(|&(_, p)| p)
            .min()
            .unwrap_or(LINE_CLOCKS)
    }

    /// Acts on every line point at or before the position not yet passed on this line.
    fn fire_due(&mut self, screen: Option<&mut Screen>) {
        let [(_, row), (_, vint), (_, hint)] = self.points();
        let active = self.active_lines();
        if self.fired & FIRED_ROW == 0 && self.mclk >= row {
            self.fired |= FIRED_ROW;
            self.dma_apply();
            if self.line < active {
                self.draw_row(screen);
            }
        }
        if self.fired & FIRED_VINT == 0 && self.mclk >= vint {
            self.fired |= FIRED_VINT;
            if self.line == active {
                self.vint_pending = true;
                self.frame_done = true;
                self.z80_int_until = self.clock + LINE_CLOCKS as u64;
                self.odd_frame = self.interlace() & 1 != 0 && !self.odd_frame;
            }
        }
        if self.fired & FIRED_HINT == 0 && self.mclk >= hint {
            self.fired |= FIRED_HINT;
            self.dma_apply();
            self.row_vsram.copy_from_slice(&self.vsram);
            // The V counter steps here; the counter counts on the lines it then shows as
            // 0 to `active` (the step into line 0 counts already) and reloads on the others.
            if (self.line + 1) % self.lines() <= active {
                if self.hint_counter == 0 {
                    self.hint_counter = self.regs[10];
                    self.hint_pending = true;
                } else {
                    self.hint_counter -= 1;
                }
            } else {
                self.hint_counter = self.regs[10];
            }
        }
    }

    /// Draws the current line's row (two half-line rows in interlace mode 2) and collects
    /// the sprite flags.
    fn draw_row(&mut self, screen: Option<&mut Screen>) {
        let w = if self.h40() { 320 } else { 256 };
        let double = self.interlace() == 3;
        let rows = if double { [self.line * 2, self.line * 2 + 1] } else { [self.line, self.line] };
        let rows = &rows[..if double { 2 } else { 1 }];
        let mut flags = SpriteFlags::default();
        if self.display_on() {
            let m = Mem {
                regs: &self.regs,
                vram: &self.vram,
                cram: self.cram.as_slice().try_into().expect("64 colours"),
                vsram: self.row_vsram.as_slice().try_into().expect("40 scroll words"),
            };
            let mut screen = screen;
            for &r in rows {
                let out = screen.as_deref_mut().map(|s| {
                    let o = r as usize * MAX_W;
                    &mut s.px[o..o + w]
                });
                let f = crate::vdp_render::render_row(&m, r, out);
                flags.overflow |= f.overflow;
                flags.collision |= f.collision;
            }
            if let Some(s) = screen {
                s.w = w;
                s.h = self.active_lines() as usize * if double { 2 } else { 1 };
            }
        } else if let Some(s) = screen {
            s.w = w;
            s.h = self.active_lines() as usize * if double { 2 } else { 1 };
            let bg = crate::vdp_render::rgb(self.cram[(self.regs[7] & 0x3F) as usize], 0);
            for &r in rows {
                let o = r as usize * MAX_W;
                s.px[o..o + w].fill(bg);
            }
        }
        self.sprite_overflow |= flags.overflow;
        self.sprite_collision |= flags.collision;
    }

    /// Master clocks from the position to `point` of `line`, always more than 0 (a point
    /// at or before the position is taken from the next frame).
    fn time_to(&self, line: u32, point: u32) -> u64 {
        let total = self.lines();
        let frame = (total * LINE_CLOCKS) as i64;
        let lines_ahead = ((line + total - self.line) % total) as i64;
        let mut d = lines_ahead * LINE_CLOCKS as i64 + point as i64 - self.mclk as i64;
        while d <= 0 {
            d += frame;
        }
        d as u64
    }

    /// The earliest master clock after the chip's position at which [`Vdp::irq_level`],
    /// [`Vdp::z80_irq`] or the frame-done flag changes if nothing is written meanwhile.
    pub fn next_event(&self) -> u64 {
        let h40 = self.h40();
        let active = self.active_lines();
        let mut best = self.clock + self.time_to(active, timing::vint_point(h40));
        if self.z80_int_until > self.clock {
            best = best.min(self.z80_int_until);
        }
        if self.regs[0] & 0x10 != 0 {
            let total = self.lines();
            let mut hc = self.hint_counter;
            // `l` is the line whose step (at its `hint_point`) comes next.
            let mut l = self.line;
            if self.fired & FIRED_HINT != 0 {
                l = (l + 1) % total;
            }
            for _ in 0..2 * total {
                if (l + 1) % total <= active {
                    if hc == 0 {
                        best = best.min(self.clock + self.time_to(l, timing::hint_point(h40)));
                        break;
                    }
                    hc -= 1;
                } else {
                    hc = self.regs[10];
                }
                l = (l + 1) % total;
            }
        }
        best.max(self.clock + 1)
    }

    /// True once per frame, when the last displayed line is finished (vertical blanking
    /// starts): the picture in the screen is complete. Reading clears it.
    pub fn take_frame_done(&mut self) -> bool {
        std::mem::take(&mut self.frame_done)
    }

    /// 68000 interrupt level requested now: 6 (vertical), 4 (horizontal) or 0. Follows the
    /// enable bits, so a write that enables a pending interrupt raises it at once.
    pub fn irq_level(&self) -> u8 {
        if self.vint_pending && self.regs[1] & 0x20 != 0 {
            6
        } else if self.hint_pending && self.regs[0] & 0x10 != 0 {
            4
        } else {
            0
        }
    }

    /// The 68000 acknowledged `level`: that request is cleared.
    pub fn irq_ack(&mut self, level: u8) {
        match level {
            6 => self.vint_pending = false,
            4 => self.hint_pending = false,
            _ => {}
        }
    }

    /// The Z80's /INT line is low (one line long, starting with the vertical interrupt;
    /// not affected by the enable bits).
    pub fn z80_irq(&self) -> bool {
        self.clock < self.z80_int_until
    }

    /// The 68000 is held off the bus until this master clock (at or before the chip's position
    /// when it is not): by a 68k→VDP DMA, or by a data-port write made while four writes wait
    /// in the FIFO, until the oldest of them is taken.
    pub fn m68k_frozen_until(&self) -> u64 {
        self.m68k_frozen_until
    }

    /// The line the V counter shows: it moves on to the next line at `hint_point`.
    fn shown_line(&self) -> u32 {
        if self.mclk >= timing::hint_point(self.h40()) { (self.line + 1) % self.lines() } else { self.line }
    }

    /// HV counter now: V counter in the high byte, H counter in the low byte.
    fn hv_counter(&self) -> u16 {
        let v = timing::v_counter(self.shown_line(), self.pal, self.regs[1] & 0x08 != 0, self.interlace());
        (v as u16) << 8 | timing::h_counter(self.mclk, self.h40()) as u16
    }

    /// Data-port writes waiting in the FIFO now: 0-4.
    fn fifo_pending(&self) -> usize {
        self.fifo_done.iter().filter(|&&t| t > self.clock).count()
    }

    /// A data-port write enters the FIFO. The chip takes it in the access slots after the
    /// write before it (or after now when nothing waits): one slot for a colour or scroll RAM
    /// word, two for a video RAM word (a byte per slot), and it leaves the FIFO at the last
    /// (`fifo_pace` probe: a held write resumes a slot after the oldest word's first byte).
    /// With four writes waiting the 68000 is held until the oldest leaves.
    fn fifo_push(&mut self, slots: u32) {
        /// Master clocks from a write until the chip can take the word in a slot (`fifo_pace`).
        const FIFO_LATENCY: u64 = 64;
        let oldest = self.fifo_done[0];
        if oldest > self.clock {
            self.m68k_frozen_until = self.m68k_frozen_until.max(oldest);
        }
        let from = (self.clock + FIFO_LATENCY).max(self.fifo_tail);
        let pos = self.mclk as u64 + (from - self.clock);
        let lc = timing::LINE_CLOCKS as u64;
        let start = DmaStart {
            line: ((self.line as u64 + pos / lc) % self.lines() as u64) as u32,
            mclk: (pos % lc) as u32,
            h40: self.h40(),
            display_on: self.display_on(),
            active_lines: self.active_lines(),
            total_lines: self.lines(),
        };
        self.fifo_tail = from + timing::slot_after(slots, start);
        self.fifo_done = [self.fifo_done[1], self.fifo_done[2], self.fifo_done[3], self.fifo_tail];
    }

    fn status(&self) -> u16 {
        let h40 = self.h40();
        // Vertical blanking follows the V counter: from its first line after the picture to
        // the line before its last one (the flag falls a line before line 0).
        let shown = self.shown_line();
        let vblank = (shown >= self.active_lines() && shown != self.lines() - 1) || !self.display_on();
        let hblank = self.mclk >= timing::hblank_start(h40) || self.mclk < timing::hblank_end(h40);
        let waiting = self.fifo_pending();
        0x3400
            | ((waiting == 0) as u16) << 9
            | ((waiting == 4) as u16) << 8
            | (self.vint_pending as u16) << 7
            | (self.sprite_overflow as u16) << 6
            | (self.sprite_collision as u16) << 5
            | (self.odd_frame as u16) << 4
            | (vblank as u16) << 3
            | (hblank as u16) << 2
            | ((self.clock < self.dma_busy_until) as u16) << 1
            | self.pal as u16
    }

    /// Word read of a port, `addr` = 0x00-0x1E even (the low bits of 0xC000xx): 0x00/0x02
    /// data, 0x04/0x06 status, 0x08-0x0E HV counter. Other addresses give 0xFFFF.
    pub fn read(&mut self, addr: u32, _now: u64) -> u16 {
        self.dma_apply();
        match addr & 0x1E {
            0x00 | 0x02 => {
                self.pending = false;
                let v = self.read_buf;
                self.prefetch();
                v
            }
            0x04 | 0x06 => {
                let v = self.status();
                self.pending = false;
                self.sprite_overflow = false;
                self.sprite_collision = false;
                v
            }
            0x08 | 0x0A | 0x0C | 0x0E => self.hv_latch.unwrap_or_else(|| self.hv_counter()),
            _ => 0xFFFF,
        }
    }

    /// Fills the read buffer from the command's memory at the address and advances it.
    fn prefetch(&mut self) {
        let a = self.addr as usize;
        self.read_buf = match self.code & 0x0F {
            0x00 => (self.vram[a & !1] as u16) << 8 | self.vram[a | 1] as u16,
            0x08 => self.cram[(a >> 1) & 0x3F],
            0x04 => self.vsram.get((a >> 1) & 0x3F).copied().unwrap_or(self.vsram[0]),
            0x0C => (self.read_buf & 0xFF00) | self.vram[a ^ 1] as u16,
            _ => return,
        };
        self.addr = self.addr.wrapping_add(self.increment());
    }

    /// Writes a word where the command points and advances the address.
    fn store(&mut self, val: u16) {
        let a = self.addr as usize;
        match self.code & 0x0F {
            0x01 => {
                self.vram[a] = (val >> 8) as u8;
                self.vram[a ^ 1] = val as u8;
            }
            0x03 => self.cram[(a >> 1) & 0x3F] = val & 0x0EEE,
            0x05 => {
                if let Some(e) = self.vsram.get_mut((a >> 1) & 0x3F) {
                    *e = val & 0x07FF;
                }
            }
            _ => {}
        }
        self.addr = self.addr.wrapping_add(self.increment());
    }

    /// Word write of a port (data or control). A control write that starts a 68k→VDP DMA
    /// reads its source through `mem` here; the data reaches video memory unit by unit until
    /// [`Vdp::m68k_frozen_until`].
    pub fn write(&mut self, addr: u32, val: u16, _now: u64, mem: &mut dyn DmaSource) {
        self.dma_apply();
        match addr & 0x1E {
            0x00 | 0x02 => self.write_data(val),
            0x04 | 0x06 => self.write_control(val, mem),
            _ => {}
        }
    }

    fn write_data(&mut self, val: u16) {
        self.pending = false;
        self.fifo_push(if self.code & 0x0F == 0x01 { 2 } else { 1 });
        self.store(val);
        if self.fill_armed {
            self.fill_armed = false;
            self.dma_fill(val);
        }
    }

    fn write_control(&mut self, val: u16, mem: &mut dyn DmaSource) {
        self.fill_armed = false;
        if self.pending {
            self.pending = false;
            self.code = (self.code & 0x03) | ((val >> 2) as u8 & 0x3C);
            if self.regs[1] & 0x10 == 0 {
                self.code &= !0x20;
            }
            self.addr = (self.addr & 0x3FFF) | (val & 3) << 14;
            if self.code & 0x20 != 0 {
                match self.regs[23] >> 6 {
                    0 | 1 => self.dma_bus(mem),
                    2 => self.fill_armed = true,
                    _ => self.dma_copy(),
                }
            } else if self.code & 1 == 0 {
                self.prefetch();
            }
            return;
        }
        self.code = (self.code & 0x3C) | (val >> 14) as u8;
        self.addr = (self.addr & 0xC000) | (val & 0x3FFF);
        if val & 0xC000 == 0x8000 {
            let r = ((val >> 8) & 0x1F) as usize;
            if r < 24 {
                if r == 0 {
                    let on = val & 0x02 != 0;
                    if on && self.hv_latch.is_none() {
                        self.hv_latch = Some(self.hv_counter());
                    } else if !on {
                        self.hv_latch = None;
                    }
                }
                self.regs[r] = val as u8;
            }
        } else {
            self.pending = true;
        }
    }

    /// DMA length (registers 19-20), 0 meaning 0x10000.
    fn dma_len(&self) -> u32 {
        match self.regs[19] as u32 | (self.regs[20] as u32) << 8 {
            0 => 0x10000,
            n => n,
        }
    }

    /// Starts a transfer of `units` now (the caller has set what it moves in `self.dma`) and
    /// returns the master clock it ends at.
    fn dma_begin(&mut self, kind: DmaKind, units: u32) -> u64 {
        let start = DmaStart {
            line: self.line,
            mclk: self.mclk,
            h40: self.h40(),
            display_on: self.display_on(),
            active_lines: self.active_lines(),
            total_lines: self.lines(),
        };
        let end = self.clock + timing::dma_duration(kind, units, start);
        let inc = self.increment();
        let d = &mut self.dma;
        d.kind = match kind {
            DmaKind::Bus => 1,
            DmaKind::Fill => 2,
            DmaKind::Copy => 3,
        };
        d.target = self.code & 0x0F;
        d.done = 0;
        d.total = units;
        d.addr = self.addr;
        d.inc = inc;
        d.t0 = self.clock;
        d.end = end;
        d.line = start.line;
        d.mclk = start.mclk;
        d.h40 = start.h40;
        d.display_on = start.display_on;
        d.active_lines = start.active_lines;
        d.total_lines = start.total_lines;
        self.dma_busy_until = end;
        end
    }

    /// Writes the units of the pending transfer that are due at the chip's position.
    fn dma_apply(&mut self) {
        if self.dma.kind == 0 {
            return;
        }
        let d = &self.dma;
        let due = if self.clock >= d.end {
            d.total
        } else {
            let kind = match d.kind {
                1 => DmaKind::Bus,
                2 => DmaKind::Fill,
                _ => DmaKind::Copy,
            };
            let start = DmaStart {
                line: d.line,
                mclk: d.mclk,
                h40: d.h40,
                display_on: d.display_on,
                active_lines: d.active_lines,
                total_lines: d.total_lines,
            };
            timing::dma_units_done(kind, start, self.clock - d.t0).min(d.total)
        };
        self.dma_advance(due);
    }

    /// Finishes the pending transfer at once (a new one is starting).
    fn dma_flush(&mut self) {
        self.dma_advance(self.dma.total);
    }

    /// Writes units up to `due`. A 68000 transfer to video RAM writes a word's low byte, then
    /// its high byte; a fill the data word's high byte to `address ^ 1` (colour and scroll RAM
    /// get whole words); a copy a byte from the source.
    fn dma_advance(&mut self, due: u32) {
        while self.dma.done < due {
            let a = self.dma.addr as usize;
            let mut step = true;
            match (self.dma.kind, self.dma.target) {
                (1, 0x01) => {
                    let w = self.dma.words[(self.dma.done >> 1) as usize];
                    if self.dma.done & 1 == 0 {
                        self.vram[a ^ 1] = w as u8;
                        step = false;
                    } else {
                        self.vram[a] = (w >> 8) as u8;
                    }
                }
                (2, 0x01) => self.vram[a ^ 1] = (self.dma.fill >> 8) as u8,
                (3, _) => {
                    self.vram[a ^ 1] = self.vram[self.dma.src as usize ^ 1];
                    self.dma.src = self.dma.src.wrapping_add(1);
                }
                (kind, 0x03) | (kind, 0x05) => {
                    let w = if kind == 1 { self.dma.words[self.dma.done as usize] } else { self.dma.fill };
                    if self.dma.target == 0x03 {
                        self.cram[(a >> 1) & 0x3F] = w & 0x0EEE;
                    } else if let Some(e) = self.vsram.get_mut((a >> 1) & 0x3F) {
                        *e = w & 0x07FF;
                    }
                }
                _ => {}
            }
            if step {
                self.dma.addr = self.dma.addr.wrapping_add(self.dma.inc);
            }
            self.dma.done += 1;
        }
        if self.dma.done >= self.dma.total {
            self.dma.kind = 0;
            self.dma.words.clear();
        }
    }

    /// 68000 to VRAM, CRAM or VSRAM. Source: word address in registers 21-23, wrapping
    /// inside its 128 KB window. The source is read now; the registers get their end values.
    fn dma_bus(&mut self, mem: &mut dyn DmaSource) {
        self.dma_flush();
        let len = self.dma_len();
        let high = (self.regs[23] as u32 & 0x7F) << 16;
        let mut src = self.regs[21] as u16 | (self.regs[22] as u16) << 8;
        for _ in 0..len {
            self.dma.words.push(mem.dma_read16((high | src as u32) << 1));
            src = src.wrapping_add(1);
        }
        self.regs[21] = src as u8;
        self.regs[22] = (src >> 8) as u8;
        self.regs[19] = 0;
        self.regs[20] = 0;
        let units = if self.code & 0x0F == 0x01 { len * 2 } else { len };
        self.m68k_frozen_until = self.dma_begin(DmaKind::Bus, units);
        self.addr = self.addr.wrapping_add((len as u16).wrapping_mul(self.increment()));
    }

    /// VRAM fill after the data write `val`: the high byte goes to `address ^ 1` for each
    /// byte of the length, the address advancing by register 15. Colour and scroll RAM get
    /// the whole word.
    fn dma_fill(&mut self, val: u16) {
        self.dma_flush();
        let len = self.dma_len();
        self.dma.fill = val;
        self.dma_begin(DmaKind::Fill, len);
        self.addr = self.addr.wrapping_add((len as u16).wrapping_mul(self.increment()));
        self.regs[19] = 0;
        self.regs[20] = 0;
    }

    /// VRAM copy: bytes from the source (registers 21-22) to the address, one per step.
    fn dma_copy(&mut self) {
        self.dma_flush();
        let len = self.dma_len();
        let src = self.regs[21] as u16 | (self.regs[22] as u16) << 8;
        self.dma.src = src;
        self.dma_begin(DmaKind::Copy, len);
        self.addr = self.addr.wrapping_add((len as u16).wrapping_mul(self.increment()));
        let src = src.wrapping_add(len as u16);
        self.regs[21] = src as u8;
        self.regs[22] = (src >> 8) as u8;
        self.regs[19] = 0;
        self.regs[20] = 0;
    }

    /// Lines per frame: 262 or 313.
    pub fn lines(&self) -> u32 {
        timing::lines(self.pal)
    }

    /// Debugging and tests.
    pub fn vram(&self) -> &[u8] {
        &self.vram
    }

    /// 64 colour entries, 9 bits each (`0000BBB0GGG0RRR0`).
    pub fn cram(&self) -> &[u16] {
        &self.cram
    }

    /// Register 0-23.
    pub fn reg(&self, index: usize) -> u8 {
        self.regs[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};

    /// A chip at the start of line 0 (power-on starts part-way through a frame): the tests
    /// count lines and clocks from there.
    fn at_line_0(pal: bool) -> Vdp {
        Vdp { line: 0, mclk: 0, fired: 0, ..Vdp::new(pal) }
    }

    /// Returns the word address's low 16 bits and records every address read.
    struct Rec(Vec<u32>);
    impl DmaSource for Rec {
        fn dma_read16(&mut self, addr: u32) -> u16 {
            self.0.push(addr);
            (addr >> 1) as u16
        }
    }

    fn ctl(v: &mut Vdp, w: u16) {
        let t = v.clock;
        v.write(4, w, t, &mut Rec(Vec::new()));
    }
    fn data(v: &mut Vdp, w: u16) {
        let t = v.clock;
        v.write(0, w, t, &mut Rec(Vec::new()));
    }
    fn set_reg(v: &mut Vdp, r: u16, d: u8) {
        ctl(v, 0x8000 | r << 8 | d as u16);
    }
    /// Two-word command: `code` CD5-CD0, `addr` A15-A0.
    fn cmd(v: &mut Vdp, code: u8, addr: u16, mem: &mut Rec) {
        let t = v.clock;
        v.write(4, (code as u16 & 3) << 14 | addr & 0x3FFF, t, mem);
        v.write(4, (code as u16 & 0x3C) << 2 | addr >> 14, t, mem);
    }
    fn hash(v: &Vdp) -> u64 {
        let mut h = std::hash::DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    /// 320-pixel mode, display on, at the start of line 0, a video RAM write command at 0.
    fn drawing_h40() -> Vdp {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 1, 0x44);
        set_reg(&mut v, 12, 0x81);
        set_reg(&mut v, 15, 2);
        cmd(&mut v, 1, 0, &mut Rec(Vec::new()));
        v
    }

    #[test]
    fn fifo_holds_four_writes_and_the_fifth_stalls() {
        // Drawing line slots of the 320-pixel mode at 32, 144, 256, 432, 624, 720, 816, 1008, 1200, ...
        // master clocks; a word written at 0 can take a slot from 64 on.
        let mut v = drawing_h40();
        for w in 0..4 {
            data(&mut v, w);
        }
        assert_eq!(v.m68k_frozen_until(), 0);
        data(&mut v, 4);
        // A video RAM word takes two slots and leaves the FIFO at the second: the oldest at 256,
        // the next at 624.
        assert_eq!(v.m68k_frozen_until(), 256);
        v.run_to(256, None);
        data(&mut v, 5);
        assert_eq!(v.m68k_frozen_until(), 624);
        assert_eq!(&v.vram()[8..12], &[0, 4, 0, 5]);

        // A colour RAM word is one slot.
        let mut v = drawing_h40();
        cmd(&mut v, 3, 0, &mut Rec(Vec::new()));
        for w in 0..5 {
            data(&mut v, w);
        }
        assert_eq!(v.m68k_frozen_until(), 144);
    }

    #[test]
    fn fifo_stall_is_short_while_blanked() {
        // Display off: 205 slots a line, 16 or 17 master clocks apart; the first after 64 is at 75.
        let mut v = drawing_h40();
        set_reg(&mut v, 1, 0x04);
        cmd(&mut v, 1, 0, &mut Rec(Vec::new()));
        for w in 0..5 {
            data(&mut v, w);
        }
        assert_eq!(v.m68k_frozen_until(), 91);

        // Vertical blanking with the display on is the same.
        let mut v = drawing_h40();
        v.run_to(230 * 3420, None);
        for w in 0..5 {
            data(&mut v, w);
        }
        assert_eq!(v.m68k_frozen_until(), 230 * 3420 + 91);
    }

    #[test]
    fn fifo_status_bits_follow_the_waiting_writes() {
        let mut v = drawing_h40();
        assert_eq!(v.read(4, 0) & 0x0300, 0x0200);
        cmd(&mut v, 1, 0, &mut Rec(Vec::new()));
        data(&mut v, 1);
        assert_eq!(v.read(4, 0) & 0x0300, 0x0000);
        for w in 2..5 {
            data(&mut v, w);
        }
        assert_eq!(v.read(4, 0) & 0x0300, 0x0100);
        // The four words leave at 256, 624, 816 and 1200.
        v.run_to(256, None);
        assert_eq!(v.read(4, 256) & 0x0300, 0x0000);
        v.run_to(1199, None);
        assert_eq!(v.read(4, 1199) & 0x0300, 0x0000);
        v.run_to(1200, None);
        assert_eq!(v.read(4, 1200) & 0x0300, 0x0200);
    }

    #[test]
    fn fifo_clone_mid_stall_continues_identically() {
        let mut a = drawing_h40();
        for w in 0..5 {
            data(&mut a, w);
        }
        let mut b = a.clone();
        assert_eq!(hash(&a), hash(&b));
        for v in [&mut a, &mut b] {
            let t = v.m68k_frozen_until();
            v.run_to(t, None);
            data(v, 5);
            data(v, 6);
        }
        assert_eq!(a.m68k_frozen_until(), b.m68k_frozen_until());
        // Two more writes at 256 with four waiting: held until the second oldest, 816.
        assert_eq!(a.m68k_frozen_until(), 816);
        assert_eq!(a.read(4, 256), b.read(4, 256));
        assert_eq!(hash(&a), hash(&b));
    }

    #[test]
    fn registers_and_memories_round_trip() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 1, 0x14);
        set_reg(&mut v, 15, 2);
        assert_eq!(v.reg(1), 0x14);
        assert_eq!(v.reg(15), 2);
        let mut m = Rec(Vec::new());
        cmd(&mut v, 1, 0x100, &mut m);
        data(&mut v, 0x1234);
        data(&mut v, 0x5678);
        assert_eq!(&v.vram()[0x100..0x104], &[0x12, 0x34, 0x56, 0x78]);
        cmd(&mut v, 0, 0x100, &mut m);
        assert_eq!(v.read(0, 0), 0x1234);
        assert_eq!(v.read(0, 0), 0x5678);
        cmd(&mut v, 3, 0x02, &mut m);
        data(&mut v, 0xFFFF);
        assert_eq!(v.cram()[1], 0x0EEE);
        cmd(&mut v, 8, 0x02, &mut m);
        assert_eq!(v.read(2, 0), 0x0EEE);
        cmd(&mut v, 5, 0x04, &mut m);
        data(&mut v, 0xF123);
        cmd(&mut v, 4, 0x04, &mut m);
        assert_eq!(v.read(0, 0), 0x0123);
    }

    #[test]
    fn auto_increment_and_pending_cleared_by_status() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 15, 4);
        let mut m = Rec(Vec::new());
        cmd(&mut v, 1, 0, &mut m);
        data(&mut v, 0x0102);
        data(&mut v, 0x0304);
        assert_eq!(&v.vram()[0..6], &[1, 2, 0, 0, 3, 4]);
        // The two words leave the FIFO within a line; the status is read after that.
        v.run_to(3420, None);
        ctl(&mut v, 0x4000);
        let s = v.read(4, 0);
        assert_eq!(s & 0x0201, 0x0200);
        // The latch was cleared, so this is a register write again.
        set_reg(&mut v, 2, 0x30);
        assert_eq!(v.reg(2), 0x30);
    }

    #[test]
    fn vram_fill_pattern() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 1, 0x10);
        set_reg(&mut v, 15, 1);
        set_reg(&mut v, 19, 4);
        set_reg(&mut v, 20, 0);
        set_reg(&mut v, 23, 0x80);
        cmd(&mut v, 0x21, 0, &mut Rec(Vec::new()));
        data(&mut v, 0xAABB);
        assert_ne!(v.read(4, 0) & 2, 0);
        v.run_to(v.dma_busy_until, None);
        assert_eq!(&v.vram()[0..7], &[0xAA, 0xBB, 0xAA, 0xAA, 0x00, 0xAA, 0x00]);
        assert_eq!(v.m68k_frozen_until(), 0);
    }

    #[test]
    fn vram_copy() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 15, 2);
        cmd(&mut v, 1, 0, &mut Rec(Vec::new()));
        data(&mut v, 0x0102);
        data(&mut v, 0x0304);
        set_reg(&mut v, 1, 0x10);
        set_reg(&mut v, 15, 1);
        set_reg(&mut v, 19, 4);
        set_reg(&mut v, 21, 0);
        set_reg(&mut v, 22, 0);
        set_reg(&mut v, 23, 0xC0);
        cmd(&mut v, 0x30, 0x10, &mut Rec(Vec::new()));
        v.run_to(v.dma_busy_until, None);
        assert_eq!(&v.vram()[0x10..0x15], &[1, 2, 3, 4, 0]);
        assert_eq!(v.reg(21), 4);
    }

    #[test]
    fn bus_dma_source_wraps_and_freezes() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 1, 0x10);
        set_reg(&mut v, 15, 2);
        set_reg(&mut v, 19, 4);
        set_reg(&mut v, 20, 0);
        set_reg(&mut v, 21, 0xFE);
        set_reg(&mut v, 22, 0xFF);
        set_reg(&mut v, 23, 0x7F);
        let mut m = Rec(Vec::new());
        cmd(&mut v, 0x21, 0x200, &mut m);
        assert_eq!(m.0, vec![0xFFFFFC, 0xFFFFFE, 0xFE0000, 0xFE0002]);
        v.run_to(v.dma_busy_until, None);
        assert_eq!(&v.vram()[0x200..0x208], &[0xFF, 0xFE, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01]);

        // 167 words = 334 bytes at 167 bytes per blanked 256-pixel line: two lines.
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 1, 0x10);
        set_reg(&mut v, 15, 2);
        set_reg(&mut v, 19, 167);
        cmd(&mut v, 0x21, 0, &mut Rec(Vec::new()));
        assert_eq!(v.m68k_frozen_until(), 6840);
        v.run_to(100, None);
        assert_ne!(v.read(4, 100) & 2, 0);
        v.run_to(6840, None);
        assert_eq!(v.read(4, 6840) & 2, 0);
    }

    #[test]
    fn dma_rates_while_displaying() {
        let s = DmaStart { line: 0, mclk: 0, h40: true, display_on: true, active_lines: 224, total_lines: 262 };
        assert_eq!(timing::dma_duration(DmaKind::Bus, 18, s), 3420);
        assert_eq!(timing::dma_duration(DmaKind::Fill, 36, s), 6840);
        assert_eq!(timing::dma_duration(DmaKind::Copy, 102, DmaStart { line: 224, ..s }), 3420);
    }

    /// Bytes of `v`'s video RAM in `range` equal to `b`.
    fn count(v: &Vdp, range: std::ops::Range<usize>, b: u8) -> usize {
        v.vram()[range].iter().filter(|&&x| x == b).count()
    }

    fn hash_of(v: &Vdp) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    /// A 64-byte 68000 → VRAM transfer to 0x200 from source words 0x8080.., started at `line`.
    fn bus_64(line: u32) -> Vdp {
        let mut v = Vdp { line, mclk: 0, fired: 0, ..Vdp::new(false) };
        v.run_to(0, None);
        set_reg(&mut v, 1, 0x50);
        set_reg(&mut v, 12, 0x81);
        set_reg(&mut v, 15, 2);
        set_reg(&mut v, 19, 32);
        set_reg(&mut v, 21, 0x80);
        set_reg(&mut v, 22, 0x80);
        cmd(&mut v, 0x21, 0x200, &mut Rec(Vec::new()));
        v
    }

    #[test]
    fn units_done_inverts_duration() {
        let s = DmaStart { line: 220, mclk: 1000, h40: true, display_on: true, active_lines: 224, total_lines: 262 };
        for kind in [DmaKind::Bus, DmaKind::Fill, DmaKind::Copy] {
            for start in [s, DmaStart { line: 0, mclk: 0, ..s }, DmaStart { h40: false, mclk: 3419, ..s }] {
                for n in 1..700 {
                    let t = timing::dma_duration(kind, n, start);
                    assert_eq!(timing::dma_units_done(kind, start, t), n);
                    assert_eq!(timing::dma_units_done(kind, start, t - 1), n - 1);
                }
            }
        }
    }

    #[test]
    fn bus_dma_lands_unit_by_unit() {
        let mut v = bus_64(0);
        assert_eq!(count(&v, 0x200..0x240, 0), 64);
        assert_eq!(v.m68k_frozen_until(), 12160);
        assert_eq!((v.addr, v.reg(21), v.reg(19)), (0x240, 0xA0, 0));
        v.run_to(3420, None);
        assert_eq!(count(&v, 0x200..0x240, 0), 64 - 18);
        assert_eq!(count(&v, 0x200..0x212, 0), 0);
        v.run_to(12159, None);
        assert_eq!(count(&v, 0x200..0x240, 0), 1);
        v.run_to(12160, None);
        for i in 0..32 {
            assert_eq!(&v.vram()[0x200 + 2 * i..0x202 + 2 * i], &[0x80, 0x80 + i as u8]);
        }
        assert!(v.dma.words.is_empty() && v.dma.kind == 0);
    }

    #[test]
    fn bus_dma_in_vertical_blanking_takes_under_a_line() {
        let mut v = bus_64(230);
        assert_eq!(v.m68k_frozen_until(), 1068);
        v.run_to(3420, None);
        assert_eq!(count(&v, 0x200..0x240, 0), 0);
    }

    #[test]
    fn clone_mid_transfer_continues_identically() {
        let mut a = bus_64(0);
        a.run_to(5000, None);
        let mut b = a.clone();
        assert_eq!(hash_of(&a), hash_of(&b));
        a.run_to(9000, None);
        b.run_to(7000, None);
        b.run_to(9000, None);
        assert_eq!(hash_of(&a), hash_of(&b));
        a.run_to(13000, None);
        b.run_to(13000, None);
        assert_eq!(hash_of(&a), hash_of(&b));
        assert_eq!(hash_of(&a), {
            let mut c = bus_64(0);
            c.run_to(13000, None);
            hash_of(&c)
        });
    }

    #[test]
    fn fill_and_copy_land_unit_by_unit() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 1, 0x50);
        set_reg(&mut v, 12, 0x81);
        set_reg(&mut v, 15, 1);
        set_reg(&mut v, 19, 36);
        set_reg(&mut v, 23, 0x80);
        cmd(&mut v, 0x21, 0x300, &mut Rec(Vec::new()));
        data(&mut v, 0xAB00);
        assert_eq!(count(&v, 0x300..0x330, 0xAB), 1);
        v.run_to(3420, None);
        assert_eq!(count(&v, 0x300..0x330, 0xAB), 18);
        assert_ne!(v.read(4, 3420) & 2, 0);
        v.run_to(6840, None);
        assert_eq!(count(&v, 0x300..0x330, 0xAB), 36);
        assert_eq!(v.read(4, 6840) & 2, 0);

        v.vram[0x400..0x412].fill(0x5A);
        set_reg(&mut v, 19, 18);
        set_reg(&mut v, 21, 0x00);
        set_reg(&mut v, 22, 0x04);
        set_reg(&mut v, 23, 0xC0);
        cmd(&mut v, 0x30, 0x500, &mut Rec(Vec::new()));
        assert_eq!(count(&v, 0x500..0x520, 0x5A), 0);
        v.run_to(6840 + 3420, None);
        assert_eq!(count(&v, 0x500..0x520, 0x5A), 9);
        v.run_to(6840 + 6840, None);
        assert_eq!(count(&v, 0x500..0x520, 0x5A), 18);
        assert_eq!((v.reg(21), v.reg(22)), (0x12, 0x04));
    }

    #[test]
    fn one_frame_done_per_frame() {
        let mut v = at_line_0(false);
        let mut done = Vec::new();
        while done.len() < 3 {
            let t = v.next_event();
            v.run_to(t, None);
            if v.take_frame_done() {
                done.push(t);
            }
        }
        assert_eq!(done[0], 224 * 3420 + timing::vint_point(false) as u64);
        assert_eq!(done[1] - done[0], 262 * 3420);
        assert_eq!(done[2] - done[1], 262 * 3420);
        assert!(!v.take_frame_done());
    }

    #[test]
    fn vertical_interrupt_at_line_224() {
        for enabled in [false, true] {
            let mut v = at_line_0(false);
            v.run_to(0, None);
            if enabled {
                set_reg(&mut v, 1, 0x20);
            }
            let t = 224 * 3420 + timing::vint_point(false) as u64;
            v.run_to(t - 1, None);
            assert_eq!(v.irq_level(), 0);
            assert!(!v.z80_irq());
            v.run_to(t, None);
            assert_eq!(v.irq_level(), if enabled { 6 } else { 0 });
            assert!(v.z80_irq());
            // Pending flag (bit 7) and vertical blanking (bit 3), whether enabled or not.
            assert_eq!(v.read(4, t) & 0x88, 0x88);
            v.run_to(t + 3419, None);
            assert!(v.z80_irq());
            v.run_to(t + 3420, None);
            assert!(!v.z80_irq());
            v.irq_ack(6);
            assert_eq!(v.irq_level(), 0);
        }
    }

    #[test]
    fn horizontal_interrupt_every_n_plus_one_lines() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 0, 0x10);
        set_reg(&mut v, 10, 3);
        let frame = 262 * 3420u64;
        v.run_to(frame, None);
        v.irq_ack(4);
        let mut seen = Vec::new();
        for line in 0..262u64 {
            let t = frame + line * 3420 + timing::hint_point(false) as u64;
            if line % 4 == 2 && line < 223 {
                assert_eq!(v.next_event(), t);
            }
            v.run_to(t, None);
            if v.irq_level() == 4 {
                seen.push(line);
                v.irq_ack(4);
            }
        }
        // The interrupt is raised at the step into V counter 3, 7, ...: on the line before.
        assert_eq!(seen, (2..223).step_by(4).collect::<Vec<_>>());
    }

    #[test]
    fn vblank_flag_falls_a_line_before_line_0() {
        for (pal, total) in [(false, 262u64), (true, 313)] {
            let mut v = at_line_0(pal);
            v.run_to(0, None);
            set_reg(&mut v, 1, 0x44);
            let hint = timing::hint_point(false) as u64;
            let mut flag = |line: u64, m: u64| {
                let t = total * 3420 + line * 3420 + m;
                v.run_to(t, None);
                v.read(4, t) & 0x08
            };
            assert_eq!(flag(223, hint - 1), 0);
            assert_eq!(flag(223, hint), 0x08);
            assert_eq!(flag(total - 2, hint - 1), 0x08);
            assert_eq!(flag(total - 2, hint), 0);
        }
    }

    #[test]
    fn horizontal_interrupt_lines_follow_the_v_counter() {
        for (r10, first, step) in [(0u8, 261u64, 1usize), (1, 0, 2), (7, 6, 8)] {
            let mut v = at_line_0(false);
            v.run_to(0, None);
            set_reg(&mut v, 0, 0x10);
            set_reg(&mut v, 10, r10);
            let frame = 262 * 3420u64;
            v.run_to(frame - 3420, None);
            v.irq_ack(4);
            let mut seen = Vec::new();
            for i in 0..262u64 {
                let line = (261 + i) % 262;
                v.run_to(frame - 3420 + i * 3420 + timing::hint_point(false) as u64, None);
                if v.irq_level() == 4 {
                    seen.push(line);
                    v.irq_ack(4);
                }
            }
            let want: Vec<u64> = std::iter::once(261).chain(0..224).skip_while(|&l| l != first).step_by(step).collect();
            assert_eq!(seen, want, "register 10 = {r10}");
        }
    }

    #[test]
    fn hv_counter_samples() {
        assert_eq!(timing::h_counter(0, false), 0x00);
        assert_eq!(timing::h_counter(0x127 * 10, false), 0x93);
        assert_eq!(timing::h_counter(0x128 * 10, false), 0xE9);
        assert_eq!(timing::h_counter(3419, false), 0xFF);
        assert_eq!(timing::h_counter(0x16C * 8, true), 0xB6);
        assert_eq!(timing::h_counter(0x16D * 8, true), 0xE4);
        assert_eq!(timing::h_counter(3419, true), 0xFF);
        assert_eq!(timing::v_counter(0xEA, false, false, 0), 0xEA);
        assert_eq!(timing::v_counter(0xEB, false, false, 0), 0xE5);
        assert_eq!(timing::v_counter(261, false, false, 0), 0xFF);
        assert_eq!(timing::v_counter(0x102, true, false, 0), 0x02);
        assert_eq!(timing::v_counter(0x103, true, false, 0), 0xCA);
        assert_eq!(timing::v_counter(312, true, false, 0), 0xFF);
        assert_eq!(timing::v_counter(312, true, true, 0), 0xFF);

        let mut v = at_line_0(false);
        v.run_to(0, None);
        assert_eq!(v.read(8, 0), 0x0000);
        v.run_to(100 * 3420, None);
        assert_eq!(v.read(8, 0), 0x6400);
        v.run_to(100 * 3420 + 2700, None);
        assert_eq!(v.read(8, 0), 0x6587);
        set_reg(&mut v, 0, 0x02);
        v.run_to(150 * 3420, None);
        assert_eq!(v.read(8, 0), 0x6587);
        set_reg(&mut v, 0, 0x00);
        assert_eq!(v.read(8, 0), 0x9600);
    }

    #[test]
    fn clone_mid_frame_continues_identically() {
        let mut v = at_line_0(false);
        v.run_to(0, None);
        set_reg(&mut v, 0, 0x10);
        set_reg(&mut v, 1, 0x20);
        set_reg(&mut v, 10, 5);
        v.run_to(100 * 3420 + 1234, None);
        let mut c = v.clone();
        assert_eq!(hash(&v), hash(&c));
        for t in [300 * 3420u64, 700 * 3420, 1000 * 3420 + 17] {
            v.run_to(t, None);
            c.run_to(t, None);
        }
        assert_eq!(hash(&v), hash(&c));
        assert_eq!(v.next_event(), c.next_event());
    }
}
