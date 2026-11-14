//! 2C02 PPU, stepped one dot at a time. Owner: PPU slice.

use crate::cart::{Esx, Mirroring, Region};
use crate::mapper::{Cartridge, Nt};

/// All PPU state: registers, 2 KiB CIRAM (4 KiB for four-screen), OAM, palette RAM, render
/// pipeline. Part of the rollback snapshot, so `Clone + Hash`, and the frame buffer is NOT
/// stored here (it is passed into `tick`).
#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub struct Ppu {
    region: Region,

    // Memories.
    /// 2 KiB CIRAM, plus 2 KiB cartridge VRAM for `Mirroring::FourScreen`.
    #[serde(with = "crate::big_array")]
    ciram: [u8; 4096],
    palette: [u8; 32],
    #[serde(with = "crate::big_array")]
    oam: [u8; 256],
    /// Secondary OAM: 8 sprites of 4 bytes, filled during sprite evaluation.
    soam: [u8; 32],

    // Registers.
    ctrl: u8,
    mask: u8,
    status: u8,
    oam_addr: u8,
    /// PPUDATA read buffer.
    buffer: u8,
    /// Open-bus latch refreshed by every CPU read/write of $2000-$2007.
    io_latch: u8,

    // Loopy registers.
    v: u16,
    t: u16,
    x: u8,
    w: bool,

    // Timing.
    /// Absolute dot counter handed to the cartridge for A12 filtering.
    dot_counter: u64,
    scanline: i16,
    dot: u16,
    odd_frame: bool,
    frame_done: bool,
    /// Set by a $2002 read at the dot before vblank would be raised.
    suppress_vbl: bool,
    /// Odd-frame dot skip decided one dot early (rendering sampled at pre-render dot 338).
    #[serde(default)]
    skip_dot: bool,

    // Background pipeline.
    nt_latch: u8,
    at_latch: u8,
    pat_lo_latch: u8,
    pat_hi_latch: u8,
    bg_lo: u16,
    bg_hi: u16,
    at_lo: u16,
    at_hi: u16,

    // Sprite pipeline (current scanline).
    sp_x: [u8; 8],
    sp_attr: [u8; 8],
    sp_lo: [u8; 8],
    sp_hi: [u8; 8],
    sp_count: u8,
    /// Slot 0 holds sprite 0.
    sp_zero: bool,
    /// Per-x sprite pixel for the current `sp_*` state (0 = none, else pixel | pal << 2 |
    /// front << 4 | sprite-0 << 5). ESX sprites set bit 7 and carry their slot `i` instead of
    /// the palette: `i & 3` in bits 2-3, `i >> 2` in bit 6; the colour row is `sp_esx[i]`.
    /// Derived; rebuilt lazily when
    /// `sp_buf_dirty`.
    #[serde(with = "crate::big_array")]
    sp_buf: [u8; 256],
    sp_buf_dirty: bool,
    /// ESX palette id (1..=28, see `Esx::tile_byte`) of slot `i`'s fetched tile (8x16: the
    /// half on this line), 0 for the cartridge's own pattern.
    sp_esx: [u8; 8],
    /// ESX banks of the ROM (constant; `Arc`-shared, hashes to nothing).
    #[serde(skip)]
    esx: Option<Esx>,
    /// ESX palettes: `esx_pal[id << 2 | v]` is the colour of pixel value `v` for palette id `id`.
    #[serde(with = "crate::big_array")]
    esx_pal: [u8; 128],
    /// Render-only overlay for sprites beyond the hardware 8 per line (local display setting).
    #[serde(skip)]
    extra: SpriteOverlay,
}

/// Extra-sprite overlay: when `on`, 256 pixels tagged like `sp_buf` (never sprite 0).
/// Display-only, so it hashes to nothing and is not saved.
#[derive(Clone)]
struct SpriteOverlay {
    on: bool,
    buf: [u8; 256],
}

impl Default for SpriteOverlay {
    fn default() -> Self {
        SpriteOverlay { on: false, buf: [0; 256] }
    }
}

impl std::hash::Hash for SpriteOverlay {
    fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
}

impl Ppu {
    pub fn new(region: Region) -> Ppu {
        Ppu {
            region,
            ciram: [0; 4096],
            palette: [0; 32],
            oam: [0; 256],
            soam: [0xFF; 32],
            ctrl: 0,
            mask: 0,
            status: 0,
            oam_addr: 0,
            buffer: 0,
            io_latch: 0,
            v: 0,
            t: 0,
            x: 0,
            w: false,
            dot_counter: 0,
            scanline: -1,
            dot: 0,
            odd_frame: false,
            frame_done: false,
            suppress_vbl: false,
            skip_dot: false,
            nt_latch: 0,
            at_latch: 0,
            pat_lo_latch: 0,
            pat_hi_latch: 0,
            bg_lo: 0,
            bg_hi: 0,
            at_lo: 0,
            at_hi: 0,
            sp_x: [0; 8],
            sp_attr: [0; 8],
            sp_lo: [0; 8],
            sp_hi: [0; 8],
            sp_count: 0,
            sp_zero: false,
            sp_buf: [0; 256],
            sp_buf_dirty: true,
            sp_esx: [0; 8],
            esx: None,
            esx_pal: [0; 128],
            extra: SpriteOverlay::default(),
        }
    }

    /// Enables ESX1/ESX2 behaviour (`docs/EXTENSIONS.md`) for a ROM carrying the chunk.
    pub fn set_esx(&mut self, esx: Option<Esx>) {
        self.esx_pal = [0; 128];
        if let Some(e) = &esx {
            for (i, bank) in e.banks.iter().enumerate() {
                for (p, pal) in bank.palettes.iter().enumerate() {
                    let id = (i << 2) + p + 1;
                    self.esx_pal[(id << 2) + 1..(id << 2) + 4].copy_from_slice(pal);
                }
            }
        }
        self.esx = esx;
        self.sp_buf_dirty = true;
    }

    /// Re-attaches the ESX banks (skipped by serde) of the running PPU after a state load.
    /// `esx_pal` and the sprite buffer are serialized, so nothing else is rebuilt.
    pub fn adopt_rom(&mut self, running: &Ppu) {
        self.esx = running.esx.clone();
        self.set_sprite_limit(running.sprite_limit());
    }

    /// `false` also draws the sprites past the 8 per line the hardware shows. Render-only:
    /// evaluation, fetches, flags and the hashed state are unchanged.
    pub fn set_sprite_limit(&mut self, limit: bool) {
        if self.extra.on == limit {
            self.extra.on = !limit;
            self.extra.buf = [0; 256];
        }
    }

    pub fn sprite_limit(&self) -> bool {
        !self.extra.on
    }

    /// Rasterises in-range sprites past the 8 in secondary OAM into the overlay for the
    /// next line, reading CHR without mapper side effects.
    #[cold]
    fn build_extra_sprites(&mut self, cart: &Cartridge) {
        let buf = &mut self.extra.buf;
        buf.fill(0);
        if self.sp_count < 8 {
            return;
        }
        let line = self.scanline;
        let tall = self.ctrl & 0x20 != 0;
        let h: i16 = if tall { 16 } else { 8 };
        let mut found = 0;
        for n in 0..64 {
            let s = &self.oam[n * 4..n * 4 + 4];
            let y = s[0] as i16;
            if !(line >= y && line < y + h) {
                continue;
            }
            found += 1;
            if found <= 8 {
                continue;
            }
            let (tile, attr, x0) = (s[1], s[2], s[3] as usize);
            let mut row = line - y;
            if attr & 0x80 != 0 {
                row = h - 1 - row;
            }
            let row = row as u16;
            let base = if tall {
                ((tile as u16 & 1) << 12) | ((tile as u16 & 0xFE) << 4) | ((row & 8) << 1)
            } else {
                ((self.ctrl as u16 & 0x08) << 9) | ((tile as u16) << 4)
            };
            let addr = base | (row & 7);
            let (mut lo, mut hi) = (cart.ppu_peek(addr), cart.ppu_peek(addr | 8));
            if attr & 0x40 != 0 {
                lo = lo.reverse_bits();
                hi = hi.reverse_bits();
            }
            let tag = ((attr & 3) << 2) | (((attr & 0x20 == 0) as u8) << 4);
            for dx in 0..8usize {
                let x = x0 + dx;
                if x >= 256 {
                    break;
                }
                let bit = 7 - dx;
                let p = ((lo >> bit) & 1) | (((hi >> bit) & 1) << 1);
                // Earlier OAM entries win: only fill pixels still empty.
                if p != 0 && buf[x] == 0 {
                    buf[x] = p | tag;
                }
            }
        }
    }

    /// Reset button.
    pub fn reset(&mut self) {
        self.ctrl = 0;
        self.mask = 0;
        self.status = 0;
        self.buffer = 0;
        self.io_latch = 0;
        self.w = false;
        self.x = 0;
        self.t = 0;
        self.scanline = -1;
        self.dot = 0;
        self.odd_frame = false;
        self.frame_done = false;
        self.suppress_vbl = false;
        self.skip_dot = false;
        self.bg_lo = 0;
        self.bg_hi = 0;
        self.at_lo = 0;
        self.at_hi = 0;
        self.sp_count = 0;
        self.sp_zero = false;
        self.sp_buf_dirty = true;
    }

    // ---------------------------------------------------------------- CPU interface

    /// CPU read of $2000-$3FFF (`addr & 7` selects the register).
    pub fn read_register(&mut self, addr: u16, cart: &mut Cartridge) -> u8 {
        match addr & 7 {
            2 => {
                // Reading one dot before vblank is raised suppresses the flag (and the NMI)
                // for this frame; reading at or after the set dot clears it anyway.
                if self.scanline == 241 && self.dot == 1 {
                    self.suppress_vbl = true;
                }
                let val = (self.status & 0xE0) | (self.io_latch & 0x1F);
                self.status &= 0x7F;
                self.w = false;
                self.io_latch = val;
                val
            }
            4 => {
                let mut val = self.oam[self.oam_addr as usize];
                if self.oam_addr & 3 == 2 && self.esx.is_none() {
                    val &= 0xE3;
                }
                self.io_latch = val;
                val
            }
            7 => {
                let addr = self.v & 0x3FFF;
                let val = if addr >= 0x3F00 {
                    // Palette reads are immediate; the buffer still gets the nametable byte
                    // underneath the palette window.
                    self.buffer = self.vram_read(addr & 0x2FFF, cart);
                    let mut p = self.palette[palette_index(addr)];
                    if self.mask & 1 != 0 {
                        p &= 0x30;
                    }
                    (p & 0x3F) | (self.io_latch & 0xC0)
                } else {
                    let old = self.buffer;
                    self.buffer = self.vram_read(addr, cart);
                    old
                };
                self.increment_v();
                self.expose_v(cart);
                self.io_latch = val;
                val
            }
            _ => self.io_latch,
        }
    }

    /// Side-effect-free register read for debuggers (no flag clears, no buffer advance).
    pub fn peek_register(&self, addr: u16) -> u8 {
        match addr & 7 {
            2 => (self.status & 0xE0) | (self.io_latch & 0x1F),
            4 => {
                let mut val = self.oam[self.oam_addr as usize];
                if self.oam_addr & 3 == 2 && self.esx.is_none() {
                    val &= 0xE3;
                }
                val
            }
            7 => {
                let addr = self.v & 0x3FFF;
                if addr >= 0x3F00 {
                    (self.palette[palette_index(addr)] & 0x3F) | (self.io_latch & 0xC0)
                } else {
                    self.buffer
                }
            }
            _ => self.io_latch,
        }
    }

    /// CPU write of $2000-$3FFF (`addr & 7` selects the register).
    pub fn write_register(&mut self, addr: u16, val: u8, cart: &mut Cartridge) {
        self.io_latch = val;
        match addr & 7 {
            0 => {
                self.ctrl = val;
                self.t = (self.t & 0xF3FF) | ((val as u16 & 3) << 10);
            }
            1 => self.mask = val,
            3 => self.oam_addr = val,
            4 => {
                self.oam[self.oam_addr as usize] = val;
                self.oam_addr = self.oam_addr.wrapping_add(1);
            }
            5 => {
                if !self.w {
                    self.x = val & 7;
                    self.t = (self.t & 0xFFE0) | (val as u16 >> 3);
                    self.w = true;
                } else {
                    self.t = (self.t & 0x8FFF) | ((val as u16 & 7) << 12);
                    self.t = (self.t & 0xFC1F) | ((val as u16 & 0xF8) << 2);
                    self.w = false;
                }
            }
            6 => {
                if !self.w {
                    self.t = (self.t & 0x00FF) | ((val as u16 & 0x3F) << 8);
                    self.w = true;
                } else {
                    self.t = (self.t & 0xFF00) | val as u16;
                    self.v = self.t;
                    self.expose_v(cart);
                    self.w = false;
                }
            }
            7 => {
                let addr = self.v & 0x3FFF;
                self.vram_write(addr, val, cart);
                self.increment_v();
                self.expose_v(cart);
            }
            _ => {}
        }
    }

    /// One OAM DMA byte: same effect as a CPU write to OAMDATA ($2004).
    pub fn oam_dma_write(&mut self, val: u8) {
        self.oam[self.oam_addr as usize] = val;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }

    /// NMI output level as the CPU's edge detector samples it: vblank flag AND PPUCTRL bit 7,
    /// one dot ahead of the flag itself (the detector latches the line during the following
    /// dot, so it already sees the flag raised at 241/1 and cleared at -1/1).
    #[inline]
    pub fn nmi_line(&self) -> bool {
        if self.ctrl & 0x80 == 0 || self.dot != 1 {
            return self.ctrl & 0x80 != 0 && self.status & 0x80 != 0;
        }
        match self.scanline {
            241 => !self.suppress_vbl,
            -1 => false,
            _ => self.status & 0x80 != 0,
        }
    }

    /// True from the dot before the vblank flag is raised until two dots after: a $2002 read
    /// or an NMI-disabling $2000 write here still cancels the NMI the edge already latched.
    #[inline]
    pub fn nmi_cancel_window(&self) -> bool {
        self.scanline == 241 && matches!(self.dot, 1..=3)
    }

    /// True exactly once per frame, right after the dot that sets the vblank flag.
    #[inline]
    pub fn take_frame_done(&mut self) -> bool {
        core::mem::take(&mut self.frame_done)
    }

    /// Current scanline: -1 = pre-render, 0-239 visible, 240 post-render, 241.. vblank.
    #[cfg(test)]
    pub fn scanline(&self) -> i16 {
        self.scanline
    }

    /// Current dot within the scanline, 0-340.
    #[cfg(test)]
    pub fn dot(&self) -> u16 {
        self.dot
    }

    // ---------------------------------------------------------------- PPU bus

    #[inline]
    fn vram_read(&mut self, addr: u16, cart: &mut Cartridge) -> u8 {
        match addr & 0x3FFF {
            0x0000..=0x1FFF => cart.ppu_read(addr & 0x1FFF, self.dot_counter),
            0x2000..=0x3EFF if cart.ppu_hooks() => match cart.nt_read(addr) {
                Nt::Value(v) => v,
                Nt::Ciram(p) => self.ciram[(p & 3) * 0x400 + (addr & 0x3FF) as usize],
            },
            0x2000..=0x3EFF => self.ciram[self.nt_index(addr, cart.mirroring())],
            _ => self.palette[palette_index(addr)],
        }
    }

    #[inline]
    fn vram_write(&mut self, addr: u16, val: u8, cart: &mut Cartridge) {
        match addr & 0x3FFF {
            0x0000..=0x1FFF => cart.ppu_write(addr & 0x1FFF, val, self.dot_counter),
            0x2000..=0x3EFF if cart.ppu_hooks() => {
                if let Some(p) = cart.nt_write(addr, val) {
                    self.ciram[(p & 3) * 0x400 + (addr & 0x3FF) as usize] = val;
                }
            }
            0x2000..=0x3EFF => {
                let i = self.nt_index(addr, cart.mirroring());
                self.ciram[i] = val;
            }
            _ => self.palette[palette_index(addr)] = val & 0x3F,
        }
    }

    /// $2000-$3EFF ($3000-$3EFF mirrors $2000-$2EFF) folded into the 4 KiB CIRAM array.
    #[inline]
    fn nt_index(&self, addr: u16, mirroring: Mirroring) -> usize {
        let a = (addr & 0x0FFF) as usize;
        let table = a >> 10;
        let bank = match mirroring {
            Mirroring::Horizontal => table >> 1,
            Mirroring::Vertical => table & 1,
            Mirroring::SingleLower => 0,
            Mirroring::SingleUpper => 1,
            Mirroring::FourScreen => table,
        };
        bank * 0x400 + (a & 0x3FF)
    }

    #[inline]
    fn increment_v(&mut self) {
        if self.rendering() && (self.scanline == -1 || self.scanline < 240) {
            // $2007 access during rendering clocks both counters instead of adding.
            self.inc_coarse_x();
            self.inc_y();
        } else {
            let step = if self.ctrl & 4 != 0 { 32 } else { 1 };
            self.v = (self.v + step) & 0x7FFF;
        }
    }

    /// Outside active rendering the PPU address bus carries `v`, so a new `v` in pattern
    /// space is visible to the board (MMC3 counts A12 rises from $2006/$2007 accesses).
    #[inline]
    fn expose_v(&mut self, cart: &mut Cartridge) {
        let addr = self.v & 0x3FFF;
        let active = self.rendering() && self.scanline < 240;
        if addr < 0x2000 && !active {
            cart.ppu_read(addr, self.dot_counter);
        }
    }

    // ---------------------------------------------------------------- scrolling

    #[inline]
    fn rendering(&self) -> bool {
        self.mask & 0x18 != 0
    }

    #[inline]
    fn inc_coarse_x(&mut self) {
        if self.v & 0x001F == 31 {
            self.v &= !0x001F;
            self.v ^= 0x0400;
        } else {
            self.v += 1;
        }
    }

    #[inline]
    fn inc_y(&mut self) {
        if self.v & 0x7000 != 0x7000 {
            self.v += 0x1000;
        } else {
            self.v &= !0x7000;
            let mut y = (self.v & 0x03E0) >> 5;
            if y == 29 {
                y = 0;
                self.v ^= 0x0800;
            } else if y == 31 {
                y = 0;
            } else {
                y += 1;
            }
            self.v = (self.v & !0x03E0) | (y << 5);
        }
    }

    #[inline]
    fn copy_x(&mut self) {
        self.v = (self.v & !0x041F) | (self.t & 0x041F);
    }

    #[inline]
    fn copy_y(&mut self) {
        self.v = (self.v & !0x7BE0) | (self.t & 0x7BE0);
    }

    // ---------------------------------------------------------------- background

    #[inline]
    fn shift(&mut self) {
        self.bg_lo <<= 1;
        self.bg_hi <<= 1;
        self.at_lo <<= 1;
        self.at_hi <<= 1;
    }

    #[inline]
    fn load_shifters(&mut self) {
        self.bg_lo = (self.bg_lo & 0xFF00) | self.pat_lo_latch as u16;
        self.bg_hi = (self.bg_hi & 0xFF00) | self.pat_hi_latch as u16;
        self.at_lo = (self.at_lo & 0xFF00) | if self.at_latch & 1 != 0 { 0xFF } else { 0 };
        self.at_hi = (self.at_hi & 0xFF00) | if self.at_latch & 2 != 0 { 0xFF } else { 0 };
    }

    #[inline]
    fn bg_pattern_addr(&self) -> u16 {
        ((self.ctrl as u16 & 0x10) << 8) | ((self.nt_latch as u16) << 4) | ((self.v >> 12) & 7)
    }

    // ---------------------------------------------------------------- sprites

    #[inline]
    fn sprite_height(&self) -> i16 {
        if self.ctrl & 0x20 != 0 { 16 } else { 8 }
    }

    /// Fills secondary OAM for the next scanline, reproducing the sprite-overflow bug.
    fn eval_sprites(&mut self) {
        self.soam = [0xFF; 32];
        let line = self.scanline;
        let h = self.sprite_height();
        let mut n = 0usize;
        let mut m = 0usize;
        let mut count = 0usize;
        let mut zero = false;

        while n < 64 {
            let y = self.oam[(n * 4 + m) & 0xFF] as i16;
            let in_range = line >= y && line < y + h;
            if count < 8 {
                if in_range {
                    if n == 0 {
                        zero = true;
                    }
                    let src = n * 4;
                    self.soam[count * 4..count * 4 + 4]
                        .copy_from_slice(&self.oam[src..src + 4]);
                    count += 1;
                }
                n += 1;
            } else if in_range {
                self.status |= 0x20;
                break;
            } else {
                // Hardware bug: the byte index advances together with the sprite index, so
                // the "y" compared is some other attribute byte.
                n += 1;
                m = (m + 1) & 3;
            }
        }

        self.sp_count = count as u8;
        self.sp_zero = zero;
        self.sp_buf_dirty = true;
    }

    /// Pattern address for sprite slot `i`; empty slots fetch tile $FF (one A12 rise per
    /// slot, which MMC3 counts).
    fn sprite_pattern_addr(&self, i: usize, high: bool) -> u16 {
        let tall = self.ctrl & 0x20 != 0;
        let y = self.soam[i * 4] as i16;
        let tile = self.soam[i * 4 + 1];
        let attr = self.soam[i * 4 + 2];
        let empty = i >= self.sp_count as usize;

        let mut row = if empty { 0 } else { self.scanline - y };
        if !empty && attr & 0x80 != 0 {
            row = (if tall { 15 } else { 7 }) - row;
        }
        let row = row.clamp(0, 15) as u16;

        let base = if tall {
            ((tile as u16 & 1) << 12) | ((tile as u16 & 0xFE) << 4) | ((row & 8) << 1)
        } else {
            ((self.ctrl as u16 & 0x08) << 9) | ((tile as u16) << 4)
        };
        base | (row & 7) | if high { 8 } else { 0 }
    }

    fn sprite_fetch(&mut self, cart: &mut Cartridge, dot: u16) {
        self.sp_buf_dirty = true;
        let i = ((dot - 257) / 8) as usize;
        match (dot - 257) & 7 {
            0 => {
                self.sp_attr[i] = self.soam[i * 4 + 2];
                self.sp_x[i] = self.soam[i * 4 + 3];
            }
            5 | 7 => {
                let high = (dot - 257) & 7 == 7;
                let addr = self.sprite_pattern_addr(i, high);
                let mut b = cart.ppu_read(addr, self.dot_counter);
                let attr = self.soam[i * 4 + 2];
                if !high {
                    self.sp_esx[i] = 0;
                }
                if let Some(esx) = &self.esx {
                    if i < self.sp_count as usize {
                        // Tile number of this fetch (8x16: the half's tile) is addr bits 4-11.
                        let bank = (attr >> 2) & 7;
                        if let Some((e, pal)) = esx.tile_byte(bank, (addr >> 4) as u8, addr) {
                            b = e;
                            self.sp_esx[i] = pal;
                        }
                    }
                }
                if attr & 0x40 != 0 {
                    b = b.reverse_bits();
                }
                let b = if i < self.sp_count as usize { b } else { 0 };
                if high {
                    self.sp_hi[i] = b;
                } else {
                    self.sp_lo[i] = b;
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- stepping

    /// Advances one dot. Visible pixels go to `fb` (`WIDTH * HEIGHT`, value = palette index
    /// | emphasis bits << 6). Pattern fetches go through `cart.ppu_read(addr, dot_counter)`.
    #[inline]
    pub fn tick(&mut self, cart: &mut Cartridge, fb: &mut [u16]) {
        let line = self.scanline;
        let dot = self.dot;

        if line >= 240 {
            // Post-render and vblank lines: only the vblank flag event and the counters.
            if line == 241 && dot == 1 {
                if !self.suppress_vbl {
                    self.status |= 0x80;
                }
                self.suppress_vbl = false;
                self.frame_done = true;
            }
            self.dot_counter += 1;
            // The odd-frame skip only happens on the pre-render line, so mid-line dots
            // just count.
            if dot < 340 {
                self.dot = dot + 1;
            } else {
                self.advance();
            }
            return;
        }
        if line == -1 && dot == 1 {
            // Clear vblank, sprite 0 hit and overflow.
            self.status &= 0x1F;
        }

        if self.rendering() {
            self.render_dot(cart, dot);
        }
        if line >= 0 && dot.wrapping_sub(1) < 256 {
            self.output_pixel(fb, line as usize, (dot - 1) as usize);
        }
        self.dot_counter += 1;
        // Below dot 338 neither the line wrap nor the odd-frame skip (sampled at 339) applies.
        if dot < 338 {
            self.dot = dot + 1;
        } else {
            self.advance();
        }
    }

    /// One dot of background/sprite fetching (rendering enabled).
    #[inline]
    fn render_dot(&mut self, cart: &mut Cartridge, dot: u16) {
        if dot == 0 {
            return;
        }
        if dot <= 256 {
            self.shift();
            self.bg_fetch(cart, dot);
            if dot == 256 {
                self.inc_y();
            }
        } else if dot <= 320 {
            if dot == 257 {
                if cart.ppu_hooks() {
                    cart.ppu_fetch(true, self.ctrl & 0x20 != 0);
                }
                self.load_shifters();
                self.copy_x();
                self.eval_sprites();
            }
            // OAMADDR is cleared throughout sprite tile loading.
            self.oam_addr = 0;
            self.sprite_fetch(cart, dot);
            if dot == 320 && self.extra.on {
                self.build_extra_sprites(cart);
            }
            if self.scanline == -1 && (280..=304).contains(&dot) {
                self.copy_y();
            }
        } else if dot <= 336 {
            if dot == 321 && cart.ppu_hooks() {
                cart.ppu_fetch(false, self.ctrl & 0x20 != 0);
            }
            self.shift();
            self.bg_fetch(cart, dot);
        } else if dot == 337 || dot == 339 {
            let addr = 0x2000 | (self.v & 0x0FFF);
            self.nt_latch = self.vram_read(addr, cart);
        }
    }

    /// Background fetch phase of dots 1-256 and 321-336.
    #[inline(always)]
    fn bg_fetch(&mut self, cart: &mut Cartridge, dot: u16) {
        match (dot - 1) & 7 {
            0 => {
                self.load_shifters();
                let addr = 0x2000 | (self.v & 0x0FFF);
                self.nt_latch = self.vram_read(addr, cart);
            }
            2 => {
                let addr = 0x23C0
                    | (self.v & 0x0C00)
                    | ((self.v >> 4) & 0x38)
                    | ((self.v >> 2) & 0x07);
                let mut at = self.vram_read(addr, cart);
                if self.v & 0x40 != 0 {
                    at >>= 4;
                }
                if self.v & 0x02 != 0 {
                    at >>= 2;
                }
                self.at_latch = at & 3;
            }
            4 => {
                let addr = self.bg_pattern_addr();
                self.pat_lo_latch = cart.ppu_read(addr, self.dot_counter);
            }
            6 => {
                let addr = self.bg_pattern_addr() | 8;
                self.pat_hi_latch = cart.ppu_read(addr, self.dot_counter);
            }
            7 => self.inc_coarse_x(),
            _ => {}
        }
    }

    #[inline]
    fn output_pixel(&mut self, fb: &mut [u16], y: usize, x: usize) {
        // An empty `fb` means fast-forward: keep every state side effect, skip composition.
        let skip = fb.is_empty();
        if skip && !self.rendering() {
            return;
        }
        let index = if !self.rendering() {
            // Rendering off: the backdrop, or whatever palette entry `v` points at.
            if self.v & 0x3F00 == 0x3F00 {
                palette_index(self.v)
            } else {
                0
            }
        } else {
            let (mut bg_pixel, mut bg_pal) = (0u8, 0u8);
            if self.mask & 0x08 != 0 && (x >= 8 || self.mask & 0x02 != 0) {
                let mux = 0x8000u16 >> self.x;
                bg_pixel = ((self.bg_lo & mux != 0) as u8) | (((self.bg_hi & mux != 0) as u8) << 1);
                bg_pal = ((self.at_lo & mux != 0) as u8) | (((self.at_hi & mux != 0) as u8) << 1);
            }

            let (mut sp_pixel, mut sp_pal, mut sp_front, mut sp_is_zero) = (0u8, 0u8, false, false);
            let mut sp_esx = false;
            if self.mask & 0x10 != 0 && (x >= 8 || self.mask & 0x04 != 0) {
                if self.sp_buf_dirty {
                    self.build_sprite_buf();
                }
                let mut s = self.sp_buf[x];
                if s == 0 && self.extra.on {
                    // Overlay sprites never carry the sprite-0 tag, so the hit is unchanged.
                    s = self.extra.buf[x];
                }
                if s != 0 {
                    sp_pixel = s & 3;
                    sp_pal = 4 + ((s >> 2) & 3);
                    sp_front = s & 0x10 != 0;
                    sp_is_zero = s & 0x20 != 0;
                    if s & 0x80 != 0 {
                        // ESX slot in place of the palette; `sp_pal` becomes the `esx_pal` row.
                        sp_esx = true;
                        sp_pal = self.sp_esx[(((s >> 2) & 3) | ((s >> 4) & 4)) as usize];
                    }
                }
            }

            if sp_is_zero && bg_pixel != 0 && sp_pixel != 0 && x != 255 {
                self.status |= 0x40;
            }
            if skip {
                return;
            }

            let sprite_wins = sp_pixel != 0 && (bg_pixel == 0 || sp_front);
            if sprite_wins && sp_esx {
                let color = self.esx_pal[((sp_pal << 2) | sp_pixel) as usize];
                self.write_color(fb, y, x, color);
                return;
            }
            let (pal, pixel) = if bg_pixel == 0 {
                (sp_pal, sp_pixel)
            } else if sp_pixel == 0 || !sp_front {
                (bg_pal, bg_pixel)
            } else {
                (sp_pal, sp_pixel)
            };

            if pixel == 0 {
                0
            } else {
                ((pal << 2) | pixel) as usize & 0x1F
            }
        };

        let color = self.palette[index];
        self.write_color(fb, y, x, color);
    }

    /// Applies greyscale and emphasis to NES colour `color` and stores it.
    #[inline]
    fn write_color(&self, fb: &mut [u16], y: usize, x: usize, color: u8) {
        let mut color = color & 0x3F;
        if self.mask & 1 != 0 {
            color &= 0x30;
        }
        fb[y * 256 + x] = color as u16 | ((self.mask as u16 & 0xE0) << 1);
    }

    /// Rasterises the 8 sprite slots into `sp_buf`; lower slots win, as in hardware.
    #[cold]
    fn build_sprite_buf(&mut self) {
        self.sp_buf_dirty = false;
        self.sp_buf = [0; 256];
        for i in (0..self.sp_count as usize).rev() {
            let attr = self.sp_attr[i];
            let pal_bits = if self.sp_esx[i] == 0 {
                (attr & 3) << 2
            } else {
                let s = i as u8;
                ((s & 3) << 2) | ((s & 4) << 4) | 0x80
            };
            let tag = pal_bits
                | (((attr & 0x20 == 0) as u8) << 4)
                | (((i == 0 && self.sp_zero) as u8) << 5);
            let x0 = self.sp_x[i] as usize;
            for dx in 0..8usize {
                let x = x0 + dx;
                if x >= 256 {
                    break;
                }
                let bit = 7 - dx;
                let p = ((self.sp_lo[i] >> bit) & 1) | (((self.sp_hi[i] >> bit) & 1) << 1);
                if p != 0 {
                    self.sp_buf[x] = p | tag;
                }
            }
        }
    }

    #[inline]
    fn advance(&mut self) {
        let last_line: i16 = if matches!(self.region, Region::Pal) { 310 } else { 260 };
        let mut dot = self.dot + 1;

        // On NTSC the last dot of the pre-render line is skipped on odd frames when
        // rendering is enabled; the decision samples rendering a dot before the skip.
        if self.scanline == -1 && dot >= 339 && !matches!(self.region, Region::Pal) {
            if dot == 339 {
                self.skip_dot = self.odd_frame && self.rendering();
            } else if dot == 340 && self.skip_dot {
                dot = 341;
            }
        }

        if dot > 340 {
            self.dot = 0;
            if self.scanline == last_line {
                self.scanline = -1;
                self.odd_frame = !self.odd_frame;
            } else {
                self.scanline += 1;
            }
        } else {
            self.dot = dot;
        }
    }
}

/// $3F00-$3FFF folded into 32 bytes; $3F10/$14/$18/$1C mirror $3F00/$04/$08/$0C.
#[inline]
fn palette_index(addr: u16) -> usize {
    let i = (addr & 0x1F) as usize;
    if i & 0x13 == 0x10 { i & 0x0F } else { i }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WIDTH;

    /// Synthetic NROM image: 16 KiB PRG, 8 KiB CHR, horizontal mirroring.
    fn cart_with_chr(chr: &[u8]) -> Cartridge {
        let mut rom = Vec::with_capacity(16 + 16384 + 8192);
        rom.extend_from_slice(b"NES\x1A");
        rom.push(1); // 16 KiB PRG
        rom.push(1); // 8 KiB CHR
        rom.extend_from_slice(&[0u8; 10]);
        rom.resize(16 + 16384, 0);
        let mut chr_bank = vec![0u8; 8192];
        chr_bank[..chr.len()].copy_from_slice(chr);
        rom.extend_from_slice(&chr_bank);
        let info = crate::cart::parse(&rom).expect("synthetic NROM parses");
        Cartridge::new(&info).expect("NROM board")
    }

    fn cart() -> Cartridge {
        cart_with_chr(&[])
    }

    fn fb() -> Vec<u16> {
        vec![0u16; WIDTH * 240]
    }

    fn run_to(ppu: &mut Ppu, cart: &mut Cartridge, fb: &mut [u16], line: i16, dot: u16) {
        for _ in 0..200_000 {
            if ppu.scanline() == line && ppu.dot() == dot {
                return;
            }
            ppu.tick(cart, fb);
        }
        panic!("never reached ({line}, {dot})");
    }

    #[test]
    fn ppu_vblank_set_at_241_1_and_cleared_at_prerender() {
        let (mut p, mut c, mut f) = (Ppu::new(Region::Ntsc), cart(), fb());
        run_to(&mut p, &mut c, &mut f, 241, 1);
        assert_eq!(p.peek_register(0x2002) & 0x80, 0, "not set before the dot");
        p.tick(&mut c, &mut f);
        assert_eq!(p.peek_register(0x2002) & 0x80, 0x80, "set at 241,1");

        run_to(&mut p, &mut c, &mut f, -1, 1);
        assert_eq!(p.peek_register(0x2002) & 0x80, 0x80, "still set before pre-render dot 1");
        p.tick(&mut c, &mut f);
        assert_eq!(p.peek_register(0x2002) & 0x80, 0, "cleared at pre-render dot 1");
    }

    #[test]
    fn ppu_nmi_line_follows_flag_and_ctrl() {
        let (mut p, mut c, mut f) = (Ppu::new(Region::Ntsc), cart(), fb());
        run_to(&mut p, &mut c, &mut f, 241, 1);
        p.tick(&mut c, &mut f);
        assert!(!p.nmi_line(), "NMI disabled in PPUCTRL");
        p.write_register(0x2000, 0x80, &mut c);
        assert!(p.nmi_line(), "enabling NMI during vblank raises the line");
        p.read_register(0x2002, &mut c);
        assert!(!p.nmi_line(), "reading $2002 clears the flag");
    }

    #[test]
    fn ppu_frame_done_once_per_frame() {
        let (mut p, mut c, mut f) = (Ppu::new(Region::Ntsc), cart(), fb());
        run_to(&mut p, &mut c, &mut f, 241, 1);
        p.tick(&mut c, &mut f);
        assert!(p.take_frame_done());
        assert!(!p.take_frame_done(), "latch consumed");

        let mut seen = 0;
        for _ in 0..89_342 {
            p.tick(&mut c, &mut f);
            if p.take_frame_done() {
                seen += 1;
            }
        }
        assert_eq!(seen, 1, "exactly one frame boundary per frame");
    }

    fn frame_lengths(region: Region, mask: u8, frames: usize) -> Vec<u64> {
        let (mut p, mut c, mut f) = (Ppu::new(region), cart(), fb());
        p.write_register(0x2001, mask, &mut c);
        // Align on a frame boundary first.
        loop {
            p.tick(&mut c, &mut f);
            if p.take_frame_done() {
                break;
            }
        }
        let mut out = Vec::new();
        for _ in 0..frames {
            let mut dots = 0u64;
            loop {
                p.tick(&mut c, &mut f);
                dots += 1;
                if p.take_frame_done() {
                    break;
                }
            }
            out.push(dots);
        }
        out
    }

    #[test]
    fn ppu_ntsc_frame_length() {
        assert_eq!(frame_lengths(Region::Ntsc, 0x00, 2), vec![89_342, 89_342]);
        let mut with_rendering = frame_lengths(Region::Ntsc, 0x18, 2);
        with_rendering.sort_unstable();
        assert_eq!(with_rendering, vec![89_341, 89_342], "odd frames skip one dot");
    }

    #[test]
    fn ppu_pal_frame_length() {
        assert_eq!(frame_lengths(Region::Pal, 0x18, 2), vec![106_392, 106_392]);
    }

    #[test]
    fn ppu_status_read_clears_write_toggle() {
        let (mut p, mut c) = (Ppu::new(Region::Ntsc), cart());
        // Put a known byte at $2000 in CIRAM.
        p.write_register(0x2006, 0x20, &mut c);
        p.write_register(0x2006, 0x00, &mut c);
        p.write_register(0x2007, 0xAB, &mut c);

        // Half-finished address write, then $2002 resets the toggle.
        p.write_register(0x2006, 0x3C, &mut c);
        p.read_register(0x2002, &mut c);
        p.write_register(0x2006, 0x20, &mut c);
        p.write_register(0x2006, 0x00, &mut c);

        assert_eq!(p.read_register(0x2007, &mut c), 0x00, "first read is the stale buffer");
        assert_eq!(p.read_register(0x2007, &mut c), 0xAB, "buffered byte from $2000");
    }

    #[test]
    fn ppu_ppudata_increment_and_mirroring() {
        let (mut p, mut c) = (Ppu::new(Region::Ntsc), cart());
        p.write_register(0x2006, 0x20, &mut c);
        p.write_register(0x2006, 0x10, &mut c);
        p.write_register(0x2007, 0x5A, &mut c);
        // Horizontal mirroring: $2410 is the same byte as $2010; second read returns it.
        p.write_register(0x2006, 0x24, &mut c);
        p.write_register(0x2006, 0x10, &mut c);
        p.read_register(0x2007, &mut c);
        assert_eq!(p.read_register(0x2007, &mut c), 0x5A, "$2400 mirrors $2000 horizontally");
        // That read loaded $2411 (incremented by 1), which is empty.
        assert_eq!(p.read_register(0x2007, &mut c), 0x00, "address auto-incremented by 1");

        // $3000 mirrors $2000.
        p.write_register(0x2006, 0x30, &mut c);
        p.write_register(0x2006, 0x10, &mut c);
        p.read_register(0x2007, &mut c);
        assert_eq!(p.peek_register(0x2007), 0x5A, "$3000 mirrors $2000");

        // Increment of 32.
        p.write_register(0x2000, 0x04, &mut c);
        p.write_register(0x2006, 0x20, &mut c);
        p.write_register(0x2006, 0x00, &mut c);
        p.write_register(0x2007, 0x11, &mut c);
        p.write_register(0x2007, 0x22, &mut c);
        p.write_register(0x2006, 0x20, &mut c);
        p.write_register(0x2006, 0x20, &mut c);
        p.read_register(0x2007, &mut c);
        assert_eq!(p.peek_register(0x2007), 0x22, "second write landed 32 bytes on");
    }

    #[test]
    fn ppu_palette_mirroring() {
        let (mut p, mut c) = (Ppu::new(Region::Ntsc), cart());
        let write = |p: &mut Ppu, c: &mut Cartridge, addr: u16, val: u8| {
            p.write_register(0x2006, (addr >> 8) as u8, c);
            p.write_register(0x2006, addr as u8, c);
            p.write_register(0x2007, val, c);
        };
        write(&mut p, &mut c, 0x3F00, 0x11);
        write(&mut p, &mut c, 0x3F04, 0x12);
        write(&mut p, &mut c, 0x3F10, 0x22);
        write(&mut p, &mut c, 0x3F14, 0x23);

        let read = |p: &mut Ppu, c: &mut Cartridge, addr: u16| -> u8 {
            p.write_register(0x2006, (addr >> 8) as u8, c);
            p.write_register(0x2006, addr as u8, c);
            p.read_register(0x2007, c) & 0x3F
        };
        assert_eq!(read(&mut p, &mut c, 0x3F00), 0x22, "$3F10 mirrors $3F00");
        assert_eq!(read(&mut p, &mut c, 0x3F10), 0x22);
        assert_eq!(read(&mut p, &mut c, 0x3F04), 0x23, "$3F14 mirrors $3F04");
        assert_eq!(read(&mut p, &mut c, 0x3F14), 0x23);
        assert_eq!(read(&mut p, &mut c, 0x3F05), 0x00, "unmirrored entry untouched");
    }

    #[test]
    fn ppu_oam_byte2_bits_read_back_as_zero() {
        let (mut p, mut c) = (Ppu::new(Region::Ntsc), cart());
        p.write_register(0x2003, 0, &mut c);
        for b in [0x10u8, 0x20, 0xFF, 0x30] {
            p.write_register(0x2004, b, &mut c);
        }
        p.write_register(0x2003, 2, &mut c);
        assert_eq!(p.read_register(0x2004, &mut c), 0xE3, "byte 2 bits 2-4 read as 0");
        p.write_register(0x2003, 1, &mut c);
        assert_eq!(p.read_register(0x2004, &mut c), 0x20);
    }

    #[test]
    fn ppu_sprite_zero_hit_on_opaque_tile() {
        // Tile 0: plane 0 all ones -> every pixel is colour 1 (opaque).
        let mut chr = vec![0u8; 16];
        for b in chr.iter_mut().take(8) {
            *b = 0xFF;
        }
        let mut c = cart_with_chr(&chr);
        let mut p = Ppu::new(Region::Ntsc);
        let mut f = fb();

        // Sprite 0 at (x=16, y=10), tile 0, in front.
        p.write_register(0x2003, 0, &mut c);
        for b in [10u8, 0x00, 0x00, 16] {
            p.write_register(0x2004, b, &mut c);
        }
        // Background and sprites on, no left-column clipping.
        p.write_register(0x2001, 0x1E, &mut c);

        loop {
            p.tick(&mut c, &mut f);
            if p.take_frame_done() {
                break;
            }
        }
        assert_eq!(p.peek_register(0x2002) & 0x40, 0x40, "sprite 0 hit during the frame");

        // Cleared again on the next pre-render line.
        run_to(&mut p, &mut c, &mut f, -1, 1);
        p.tick(&mut c, &mut f);
        assert_eq!(p.peek_register(0x2002) & 0x40, 0, "sprite 0 hit cleared at pre-render");
    }

    #[test]
    fn ppu_sprite_overflow_with_nine_sprites_on_a_line() {
        let (mut p, mut c, mut f) = (Ppu::new(Region::Ntsc), cart(), fb());
        p.write_register(0x2003, 0, &mut c);
        for _ in 0..9 {
            for b in [20u8, 0x00, 0x00, 0] {
                p.write_register(0x2004, b, &mut c);
            }
        }
        p.write_register(0x2001, 0x18, &mut c);
        loop {
            p.tick(&mut c, &mut f);
            if p.take_frame_done() {
                break;
            }
        }
        assert_eq!(p.peek_register(0x2002) & 0x20, 0x20, "overflow flag set");
    }

    #[test]
    fn ppu_sprite_limit_off_draws_all_twelve_sprites_without_changing_state() {
        use std::hash::{Hash, Hasher};
        let mut chr = vec![0u8; 16];
        chr[..8].fill(0xFF);
        let run = |limit: bool| {
            let (mut p, mut c, mut f) = (Ppu::new(Region::Ntsc), cart_with_chr(&chr), fb());
            p.set_sprite_limit(limit);
            p.write_register(0x2006, 0x3F, &mut c);
            p.write_register(0x2006, 0x11, &mut c);
            p.write_register(0x2007, 0x16, &mut c);
            p.write_register(0x2003, 0, &mut c);
            for n in 0..64u8 {
                let s = if n < 12 { [20, 0, 0, n * 16] } else { [0xFF, 0, 0, 0] };
                for b in s {
                    p.write_register(0x2004, b, &mut c);
                }
            }
            p.write_register(0x2001, 0x14, &mut c);
            for _ in 0..2 {
                loop {
                    p.tick(&mut c, &mut f);
                    if p.take_frame_done() {
                        break;
                    }
                }
            }
            let line = &f[22 * WIDTH..23 * WIDTH];
            let shown = (0..16).filter(|i| line[i * 16] & 0x3F == 0x16).count();
            let mut h = std::collections::hash_map::DefaultHasher::new();
            p.hash(&mut h);
            (shown, p.peek_register(0x2002), h.finish())
        };
        let (hw, hw_status, hw_hash) = run(true);
        let (all, all_status, all_hash) = run(false);
        assert_eq!(hw, 8, "hardware limit shows 8 sprites");
        assert_eq!(all, 12, "limit off shows all 12 sprites");
        assert_eq!(hw_status & 0x20, 0x20, "overflow flag set");
        assert_eq!(hw_status, all_status, "status flags identical");
        assert_eq!(hw_hash, all_hash, "hashed state identical");
    }

    #[test]
    fn ppu_disabled_rendering_outputs_backdrop() {
        let (mut p, mut c, mut f) = (Ppu::new(Region::Ntsc), cart(), fb());
        p.write_register(0x2006, 0x3F, &mut c);
        p.write_register(0x2006, 0x00, &mut c);
        p.write_register(0x2007, 0x29, &mut c);
        // Point v away from the palette, else the entry v addresses is shown instead.
        p.write_register(0x2006, 0x20, &mut c);
        p.write_register(0x2006, 0x00, &mut c);
        // Emphasis bits set, rendering off.
        p.write_register(0x2001, 0xE0, &mut c);
        loop {
            p.tick(&mut c, &mut f);
            if p.take_frame_done() {
                break;
            }
        }
        assert_eq!(f[0] & 0x3F, 0x29, "backdrop colour");
        assert_eq!(f[0] >> 6, 0b111, "emphasis in bits 6-8");
    }
}
