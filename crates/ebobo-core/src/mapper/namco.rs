//! Namco boards.
//!
//! - Namco 108 / DxROM (206) and its variants: 76 (2 KiB CHR banks), 88 (CHR A16 split:
//!   $0000 half from the lower 64 KiB, $1000 half from the upper), 95 (CHR bit 5 of R0/R1 picks
//!   the CIRAM page), 154 (88 + single-screen mirroring via data bit 6).
//! - Namco 163 (19): 8 KiB PRG, 8x1 KiB CHR, 15-bit CPU-cycle IRQ, 128-byte internal RAM at
//!   $4800 (address/auto-increment via $F800) and up to 8 time-multiplexed wavetable channels.
//!
//! Not done: N163 CHR banks $E0-$FF selecting CIRAM (and the $C000-$D800 nametable registers)
//! need the PPU to call `nt_read`/`nt_write` and a CIRAM path in `chr_index`; the PPU does not
//! yet, so those values select plain CHR ROM banks. Mapper 95's two independent nametable
//! pages are approximated with the closest `Mirroring` value.

use super::{BoardLogic, Mem, K1, K8};
use crate::cart::{Mirroring, RomInfo};

/// Mapper numbers routed to this file by `Cartridge::new`.
pub(crate) const MAPPERS: &[u16] = &[19, 206, 76, 88, 95, 154];

#[derive(Clone, Copy, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Kind {
    N108,
    M76,
    M88,
    M95,
    M154,
    N163,
}

/// N163 cycles per channel update.
const N163_CHANNEL_CYCLES: u8 = 15;

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Namco {
    kind: Kind,
    // Namco 108 family.
    sel: u8,
    regs: [u8; 8],
    single: Option<Mirroring>,
    // Namco 163.
    prg: [u8; 3],
    chr: [u8; 8],
    irq_counter: u16,
    irq_enabled: bool,
    irq_line: bool,
    #[serde(with = "crate::big_array")]
    ram: [u8; 128],
    ram_addr: u8,
    auto_inc: bool,
    sound_off: bool,
    /// Channel being updated (7 down to 8 - active count).
    chan: u8,
    chan_timer: u8,
    /// Output of the channel last updated (time-multiplexed DAC), -120..=105.
    out: i16,
}

impl Namco {
    /// `Some` once the board for `info.mapper`/`info.submapper` is implemented.
    pub fn new(info: &RomInfo) -> Option<Self> {
        let kind = match info.mapper {
            19 => Kind::N163,
            206 => Kind::N108,
            76 => Kind::M76,
            88 => Kind::M88,
            95 => Kind::M95,
            154 => Kind::M154,
            _ => return None,
        };
        Some(Namco {
            kind,
            sel: 0,
            regs: [0, 2, 4, 5, 6, 7, 0, 1],
            single: None,
            prg: [0, 1, 2],
            chr: [0, 1, 2, 3, 4, 5, 6, 7],
            irq_counter: 0,
            irq_enabled: false,
            irq_line: false,
            ram: [0; 128],
            ram_addr: 0,
            auto_inc: false,
            sound_off: false,
            chan: 7,
            chan_timer: 0,
            out: 0,
        })
    }

    fn n108_chr(&self, a: usize) -> usize {
        let r = &self.regs;
        match self.kind {
            Kind::M76 => {
                let bank = r[2 + (a >> 11)] as usize;
                bank * 2 * K1 + (a & 0x7FF)
            }
            _ => {
                let (bank1k, off) = if a < 0x1000 {
                    let b = (r[a >> 11] & 0x3E) as usize + ((a >> 10) & 1);
                    (b, a & 0x3FF)
                } else {
                    (r[2 + ((a - 0x1000) >> 10)] as usize, a & 0x3FF)
                };
                let bank1k = match self.kind {
                    Kind::M88 | Kind::M154 if a >= 0x1000 => (bank1k & 0x3F) | 0x40,
                    Kind::M88 | Kind::M154 => bank1k & 0x3F,
                    Kind::M95 => bank1k & 0x1F,
                    _ => bank1k & 0x3F,
                };
                bank1k * K1 + off
            }
        }
    }

    fn n163_step_channel(&mut self) {
        let active = ((self.ram[0x7F] >> 4) & 7) + 1;
        let base = 0x40 + 8 * self.chan as usize;
        let r = &mut self.ram;
        let freq = r[base] as u32 | (r[base + 2] as u32) << 8 | ((r[base + 4] & 3) as u32) << 16;
        let len = 256 - (r[base + 4] & 0xFC) as u32;
        let mut phase = r[base + 1] as u32 | (r[base + 3] as u32) << 8 | (r[base + 5] as u32) << 16;
        phase = (phase + freq) % (len << 16);
        r[base + 1] = phase as u8;
        r[base + 3] = (phase >> 8) as u8;
        r[base + 5] = (phase >> 16) as u8;
        let nib = ((phase >> 16) + r[base + 6] as u32) & 0xFF;
        let byte = r[(nib >> 1) as usize & 0x7F];
        let sample = if nib & 1 == 0 { byte & 0xF } else { byte >> 4 };
        let vol = (r[base + 7] & 0xF) as i16;
        self.out = (sample as i16 - 8) * vol;
        self.chan = if self.chan <= 8 - active { 7 } else { self.chan - 1 };
    }
}

impl BoardLogic for Namco {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        let last = mem.prg_8k() - 1;
        let slot = a >> 13;
        let bank = if self.kind == Kind::N163 {
            match slot {
                0..=2 => (self.prg[slot] & 0x3F) as usize,
                _ => last,
            }
        } else {
            match slot {
                0 => (self.regs[6] & 0x3F) as usize,
                1 => (self.regs[7] & 0x3F) as usize,
                2 => last - 1,
                _ => last,
            }
        };
        bank * K8 + (a & (K8 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        if self.kind == Kind::N163 {
            self.chr[a >> 10] as usize * K1 + (a & 0x3FF)
        } else {
            self.n108_chr(a)
        }
    }

    fn write(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        if self.kind == Kind::N163 {
            match addr & 0xF800 {
                0x8000..=0xB800 => self.chr[((addr - 0x8000) >> 11) as usize] = val,
                0xE000 => {
                    self.prg[0] = val & 0x3F;
                    self.sound_off = val & 0x40 != 0;
                }
                0xE800 => self.prg[1] = val & 0x3F,
                0xF000 => self.prg[2] = val & 0x3F,
                0xF800 => {
                    self.ram_addr = val & 0x7F;
                    self.auto_inc = val & 0x80 != 0;
                }
                _ => {} // $C000-$D800: CIRAM nametable select, see module docs.
            }
            return;
        }
        if self.kind == Kind::M154 {
            self.single = Some(if val & 0x40 != 0 { Mirroring::SingleUpper } else { Mirroring::SingleLower });
        }
        if addr >= 0xA000 {
            return;
        }
        if addr & 1 == 0 {
            self.sel = val & 7;
        } else {
            self.regs[self.sel as usize] = val;
        }
    }

    fn write_low(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        if self.kind != Kind::N163 {
            return;
        }
        match addr & 0xF800 {
            0x4800 => {
                self.ram[self.ram_addr as usize] = val;
                if self.auto_inc {
                    self.ram_addr = (self.ram_addr + 1) & 0x7F;
                }
            }
            0x5000 => {
                self.irq_counter = (self.irq_counter & 0x7F00) | val as u16;
                self.irq_line = false;
            }
            0x5800 => {
                self.irq_counter = (self.irq_counter & 0xFF) | ((val & 0x7F) as u16) << 8;
                self.irq_enabled = val & 0x80 != 0;
                self.irq_line = false;
            }
            _ => {}
        }
    }

    fn read_low(&mut self, addr: u16, mem: &Mem) -> Option<u8> {
        let v = self.peek_low(addr, mem);
        if self.kind == Kind::N163 && addr & 0xF800 == 0x4800 && self.auto_inc {
            self.ram_addr = (self.ram_addr + 1) & 0x7F;
        }
        v
    }

    fn peek_low(&self, addr: u16, _: &Mem) -> Option<u8> {
        if self.kind != Kind::N163 {
            return None;
        }
        match addr & 0xF800 {
            0x4800 => Some(self.ram[self.ram_addr as usize]),
            0x5000 => Some(self.irq_counter as u8),
            0x5800 => Some((self.irq_counter >> 8) as u8 | (self.irq_enabled as u8) << 7),
            _ => None,
        }
    }

    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        match self.kind {
            Kind::M154 => self.single,
            Kind::M95 => {
                let (a, b) = (self.regs[0] & 0x20 != 0, self.regs[1] & 0x20 != 0);
                Some(match (a, b) {
                    (false, false) => Mirroring::SingleLower,
                    (true, true) => Mirroring::SingleUpper,
                    _ => Mirroring::Horizontal,
                })
            }
            _ => None,
        }
    }

    fn irq(&self) -> bool {
        self.irq_line
    }

    fn cpu_tick(&mut self) {
        if self.kind != Kind::N163 {
            return;
        }
        if self.irq_enabled && self.irq_counter < 0x7FFF {
            self.irq_counter += 1;
            if self.irq_counter == 0x7FFF {
                self.irq_line = true;
            }
        }
        if self.sound_off {
            return;
        }
        self.chan_timer += 1;
        if self.chan_timer >= N163_CHANNEL_CYCLES {
            self.chan_timer = 0;
            self.n163_step_channel();
        }
    }

    fn reset(&mut self) {
        self.irq_line = false;
        self.irq_enabled = false;
    }

    fn has_audio(&self) -> bool {
        self.kind == Kind::N163
    }

    /// Single-channel peak (±120) is roughly a full-volume 2A03 square; scale ~×32.
    fn audio_level(&self) -> i32 {
        if self.kind == Kind::N163 { self.out as i32 * 32 } else { 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::Cartridge;
    use crate::cart::Mirroring;

    /// PRG byte = 8 KiB bank number, CHR byte = 1 KiB bank number.
    fn cart(mapper: u16, prg8: usize, chr1: usize) -> Cartridge {
        let mut i = info(mapper, 1, 1);
        let prg: Vec<u8> = (0..prg8).flat_map(|b| std::iter::repeat_n(b as u8, 0x2000)).collect();
        let chr: Vec<u8> = (0..chr1).flat_map(|b| std::iter::repeat_n(b as u8, 0x400)).collect();
        i.prg_rom = crate::cart::RomBytes(std::sync::Arc::from(prg));
        i.chr_rom = crate::cart::RomBytes(std::sync::Arc::from(chr));
        Cartridge::new(&i).unwrap()
    }

    #[test]
    fn n108_banking() {
        let mut c = cart(206, 16, 64);
        for (r, v) in [(0u8, 10u8), (1, 21), (2, 30), (3, 31), (4, 32), (5, 33), (6, 3), (7, 4)] {
            c.cpu_write(0x8000, r, 0);
            c.cpu_write(0x8001, v, 0);
        }
        assert_eq!(c.cpu_peek(0x8000), Some(3));
        assert_eq!(c.cpu_peek(0xA000), Some(4));
        assert_eq!(c.cpu_peek(0xC000), Some(14));
        assert_eq!(c.cpu_peek(0xE000), Some(15));
        let chr: Vec<u8> = (0..8).map(|s| c.ppu_read(s * 0x400, 0)).collect();
        assert_eq!(chr, [10, 11, 20, 21, 30, 31, 32, 33]);
        // $A000+ ignored.
        c.cpu_write(0xA001, 9, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(3));
    }

    #[test]
    fn m76_m88_m154_chr() {
        let mut c = cart(76, 4, 64);
        c.cpu_write(0x8000, 2, 0);
        c.cpu_write(0x8001, 5, 0);
        assert_eq!((c.ppu_read(0, 0), c.ppu_read(0x400, 0)), (10, 11));
        let mut c = cart(88, 4, 128);
        c.cpu_write(0x8000, 2, 0);
        c.cpu_write(0x8001, 3, 0);
        c.cpu_write(0x8000, 0, 0);
        c.cpu_write(0x8001, 0x42, 0);
        assert_eq!(c.ppu_read(0x1000, 0), 0x43);
        assert_eq!(c.ppu_read(0, 0), 2);
        let mut c = cart(154, 4, 128);
        c.cpu_write(0xC000, 0x40, 0);
        assert_eq!(c.mirroring(), Mirroring::SingleUpper);
        c.cpu_write(0x8000, 0, 0);
        assert_eq!(c.mirroring(), Mirroring::SingleLower);
    }

    #[test]
    fn m95_nametables() {
        let mut c = cart(95, 4, 32);
        c.cpu_write(0x8000, 0, 0);
        c.cpu_write(0x8001, 0x20, 0);
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0x8000, 1, 0);
        c.cpu_write(0x8001, 0x22, 0);
        assert_eq!(c.mirroring(), Mirroring::SingleUpper);
    }

    #[test]
    fn n163_banking() {
        let mut c = cart(19, 16, 64);
        c.cpu_write(0xE000, 5, 0);
        c.cpu_write(0xE800, 6, 0);
        c.cpu_write(0xF000, 7, 0);
        for s in 0..8u16 {
            c.cpu_write(0x8000 + s * 0x800, 40 + s as u8, 0);
        }
        assert_eq!(c.cpu_peek(0x8000), Some(5));
        assert_eq!(c.cpu_peek(0xA000), Some(6));
        assert_eq!(c.cpu_peek(0xC000), Some(7));
        assert_eq!(c.cpu_peek(0xE000), Some(15));
        let chr: Vec<u8> = (0..8).map(|s| c.ppu_read(s * 0x400, 0)).collect();
        assert_eq!(chr, [40, 41, 42, 43, 44, 45, 46, 47]);
    }

    #[test]
    fn n163_irq() {
        let mut c = cart(19, 4, 8);
        c.cpu_write(0x5000, 0xFD, 0);
        c.cpu_write(0x5800, 0xFF, 0); // counter $7FFD, enabled
        c.cpu_tick();
        assert!(!c.irq());
        c.cpu_tick();
        assert!(c.irq());
        c.cpu_tick(); // stays at $7FFF
        assert_eq!(c.cpu_peek(0x5000), Some(0xFF));
        c.cpu_write(0x5800, 0x80, 0);
        assert!(!c.irq());
    }

    #[test]
    fn n163_ram_auto_increment() {
        let mut c = cart(19, 4, 8);
        c.cpu_write(0xF800, 0x80 | 0x7E, 0);
        for v in [1, 2, 3] {
            c.cpu_write(0x4800, v, 0);
        }
        c.cpu_write(0xF800, 0x80 | 0x7E, 0);
        assert_eq!(c.cpu_read(0x4800), Some(1));
        assert_eq!(c.cpu_read(0x4800), Some(2));
        assert_eq!(c.cpu_read(0x4800), Some(3)); // wrapped to $00
        c.cpu_write(0xF800, 0x00, 0);
        assert_eq!(c.cpu_read(0x4800), Some(3));
        assert_eq!(c.cpu_read(0x4800), Some(3));
    }

    #[test]
    fn n163_channel_periodic() {
        let mut c = cart(19, 4, 8);
        let mut w = |a: u8, v: u8| {
            c.cpu_write(0xF800, a, 0);
            c.cpu_write(0x4800, v, 0);
        };
        // Wave: 4 nibbles 0,0,F,F at nibble address 0 (square, length 4).
        w(0x00, 0x00);
        w(0x01, 0xFF);
        // Channel 7 ($78): freq $10000 = one sample per update, length 4, volume 15.
        w(0x78, 0x00);
        w(0x7A, 0x00);
        w(0x7C, (256u16 - 4) as u8 | 1);
        w(0x7E, 0x00);
        w(0x7F, 0x0F); // 1 channel, volume 15
        assert!(c.has_expansion_audio());
        let mut levels = Vec::new();
        for _ in 0..8 {
            for _ in 0..15 {
                c.cpu_tick();
            }
            levels.push(c.expansion_audio());
        }
        let lo = -8 * 15 * 32;
        let hi = 7 * 15 * 32;
        assert_eq!(levels, [lo, hi, hi, lo, lo, hi, hi, lo]);
    }
}
