//! MMC3 / TxROM (mapper 4) with the A12 scanline IRQ counter.

use super::{BoardLogic, Mem, K1, K8};
use crate::cart::Mirroring;

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Mmc3 {
    bank_select: u8,
    regs: [u8; 8],
    horizontal: bool,
    ram_protect: u8,
    irq_latch: u8,
    irq_counter: u8,
    irq_reload: bool,
    irq_enabled: bool,
    irq: bool,
    a12_high: bool,
    /// PPU dot at which A12 last went low (start of the current low period).
    a12_low_since: u64,
}

impl Mmc3 {
    pub fn new() -> Self {
        Mmc3 {
            bank_select: 0,
            regs: [0, 2, 4, 5, 6, 7, 0, 1],
            horizontal: false,
            ram_protect: 0x80,
            irq_latch: 0,
            irq_counter: 0,
            irq_reload: false,
            irq_enabled: false,
            irq: false,
            a12_high: false,
            a12_low_since: 0,
        }
    }
}

impl BoardLogic for Mmc3 {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        let n8 = mem.prg_8k();
        let slot = a / K8;
        let swap = self.bank_select & 0x40 != 0;
        let bank = match (slot, swap) {
            (0, false) | (2, true) => self.regs[6] as usize,
            (1, _) => self.regs[7] as usize,
            (0, true) | (2, false) => n8.wrapping_sub(2),
            _ => n8 - 1,
        };
        mem.prg_off(bank, K8, a & (K8 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        let a = if self.bank_select & 0x80 != 0 { a ^ 0x1000 } else { a };
        let bank = match a >> 10 {
            0 => self.regs[0] & 0xFE,
            1 => self.regs[0] | 1,
            2 => self.regs[1] & 0xFE,
            3 => self.regs[1] | 1,
            n => self.regs[n - 2],
        };
        bank as usize * K1 + (a & 0x3FF)
    }

    fn prg_ram_bank(&self, _: &Mem) -> Option<usize> {
        (self.ram_protect & 0x80 != 0).then_some(0)
    }

    fn prg_ram_writable(&self) -> bool {
        self.ram_protect & 0x40 == 0
    }

    fn write(&mut self, addr: u16, val: u8, _: u64, mem: &Mem) {
        match (addr >> 13, addr & 1) {
            (4, 0) => self.bank_select = val,
            (4, _) => self.regs[(self.bank_select & 7) as usize] = val,
            (5, 0) => {
                if mem.header_mirroring != Mirroring::FourScreen {
                    self.horizontal = val & 1 != 0;
                }
            }
            (5, _) => self.ram_protect = val,
            (6, 0) => self.irq_latch = val,
            (6, _) => {
                self.irq_counter = 0;
                self.irq_reload = true;
            }
            (7, 0) => {
                self.irq_enabled = false;
                self.irq = false;
            }
            _ => self.irq_enabled = true,
        }
    }

    /// A12 rise detector with the M2-style low-time filter.
    #[inline]
    fn ppu_access(&mut self, addr: u16, ppu_dot: u64) {
        let high = addr & 0x1000 != 0;
        if high {
            if !self.a12_high {
                self.a12_high = true;
                if ppu_dot.wrapping_sub(self.a12_low_since) >= 10 {
                    if self.irq_counter == 0 || self.irq_reload {
                        self.irq_counter = self.irq_latch;
                        self.irq_reload = false;
                    } else {
                        self.irq_counter -= 1;
                    }
                    if self.irq_counter == 0 && self.irq_enabled {
                        self.irq = true;
                    }
                }
            }
        } else if self.a12_high {
            self.a12_high = false;
            self.a12_low_since = ppu_dot;
        }
    }

    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        Some(if self.horizontal { Mirroring::Horizontal } else { Mirroring::Vertical })
    }

    fn irq(&self) -> bool {
        self.irq
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::Cartridge;

    /// One scanline of PPU fetches: BG patterns at $0xxx, sprite patterns at $1xxx.
    fn scanline(c: &mut Cartridge, dot: &mut u64) {
        let start = *dot;
        for x in (0..256).step_by(8) {
            c.ppu_read(0x0000 | (x as u16 & 0xFF0), start + x + 5);
        }
        for s in 0..8u64 {
            c.ppu_read(0x1000, start + 257 + s * 8 + 5);
            c.ppu_read(0x1008, start + 257 + s * 8 + 7);
        }
        c.ppu_read(0x0000, start + 325);
        c.ppu_read(0x0008, start + 333);
        *dot = start + 341;
    }

    fn mmc3_with_latch(latch: u8) -> Cartridge {
        let mut c = Cartridge::new(&info(4, 8, 32)).unwrap();
        c.cpu_write(0xC000, latch, 0);
        c.cpu_write(0xC001, 0, 0);
        c.cpu_write(0xE001, 0, 0);
        c
    }

    #[test]
    fn mapper_mmc3_irq_after_latch_plus_one_lines() {
        let latch = 5;
        let mut c = mmc3_with_latch(latch);
        let mut dot = 0;
        for _ in 0..latch {
            scanline(&mut c, &mut dot);
            assert!(!c.irq());
        }
        scanline(&mut c, &mut dot);
        assert!(c.irq(), "IRQ after latch+1 scanlines");
        c.cpu_write(0xE000, 0, 0);
        assert!(!c.irq());
    }

    #[test]
    fn mapper_mmc3_a12_filter_rejects_fast_toggles() {
        let mut c = mmc3_with_latch(2);
        // Initial rise after a long low period clocks once (reload to 2).
        let mut dot = 100;
        c.ppu_read(0x1000, dot);
        for _ in 0..200 {
            dot += 4;
            c.ppu_read(0x0000, dot);
            dot += 4;
            c.ppu_read(0x1000, dot);
        }
        assert!(!c.irq());
    }

    #[test]
    fn mapper_mmc3_prg_banking() {
        let mut c = Cartridge::new(&info(4, 8, 32)).unwrap();
        // 16 banks of 8 KiB; 16 KiB bank b holds byte b, so 8 KiB bank n reads n/2.
        c.cpu_write(0x8000, 6, 0);
        c.cpu_write(0x8001, 4, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.cpu_peek(0xC000), Some(7));
        c.cpu_write(0x8000, 0x46, 0);
        assert_eq!(c.cpu_peek(0xC000), Some(2));
        assert_eq!(c.cpu_peek(0x8000), Some(7));
    }
}
