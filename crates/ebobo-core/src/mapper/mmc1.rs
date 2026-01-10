//! MMC1 / SxROM (mapper 1), incl. SOROM/SUROM/SXROM PRG-RAM and outer PRG banking.

use super::{BoardLogic, Mem, K16, K4};
use crate::cart::Mirroring;

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Mmc1 {
    shift: u8,
    count: u8,
    control: u8,
    chr0: u8,
    chr1: u8,
    prg: u8,
    /// CPU cycle of the last register write; `u64::MAX` = none yet.
    last_write_cycle: u64,
}

impl Mmc1 {
    pub fn new() -> Self {
        Mmc1 { shift: 0, count: 0, control: 0x0C, chr0: 0, chr1: 0, prg: 0, last_write_cycle: u64::MAX }
    }
}

impl BoardLogic for Mmc1 {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        // SUROM/SXROM: 512 KiB PRG, CHR bank 0 bit 4 selects the 256 KiB half.
        let outer = if mem.prg.len() > 0x40000 { (self.chr0 & 0x10) as usize } else { 0 };
        let reg = (self.prg & 0x0F) as usize;
        let hi = a >= K16;
        let bank = match (self.control >> 2) & 3 {
            0 | 1 => (reg & 0x0E) | hi as usize,
            2 => if hi { reg } else { 0 },
            _ => if hi { 0x0F } else { reg },
        };
        mem.prg_off(outer | bank, K16, a & (K16 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        if self.control & 0x10 == 0 {
            (self.chr0 & 0x1E) as usize * K4 + a
        } else if a < K4 {
            self.chr0 as usize * K4 + a
        } else {
            self.chr1 as usize * K4 + (a & 0xFFF)
        }
    }

    fn prg_ram_bank(&self, mem: &Mem) -> Option<usize> {
        if self.prg & 0x10 != 0 {
            return None;
        }
        Some(match mem.prg_ram.len() {
            // SOROM: CHR bank bit 3 selects the 8 KiB half.
            0x4000 => ((self.chr0 >> 3) & 1) as usize,
            // SXROM: CHR bank bits 2-3.
            l if l > 0x4000 => ((self.chr0 >> 2) & 3) as usize,
            _ => 0,
        })
    }

    fn write(&mut self, addr: u16, val: u8, cycle: u64, _: &Mem) {
        let consecutive = cycle.checked_sub(1) == Some(self.last_write_cycle);
        self.last_write_cycle = cycle;
        if consecutive {
            return;
        }
        if val & 0x80 != 0 {
            self.shift = 0;
            self.count = 0;
            self.control |= 0x0C;
            return;
        }
        self.shift |= (val & 1) << self.count;
        self.count += 1;
        if self.count == 5 {
            let v = self.shift;
            match (addr >> 13) & 3 {
                0 => self.control = v,
                1 => self.chr0 = v,
                2 => self.chr1 = v,
                _ => self.prg = v,
            }
            self.shift = 0;
            self.count = 0;
        }
    }

    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        Some(match self.control & 3 {
            0 => Mirroring::SingleLower,
            1 => Mirroring::SingleUpper,
            2 => Mirroring::Vertical,
            _ => Mirroring::Horizontal,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::Cartridge;
    use crate::cart::Mirroring;

    fn mmc1_write(c: &mut Cartridge, addr: u16, val: u8, cycle: &mut u64) {
        for i in 0..5 {
            c.cpu_write(addr, (val >> i) & 1, *cycle);
            *cycle += 2;
        }
    }

    #[test]
    fn mapper_mmc1_serial_bank_select() {
        let mut c = Cartridge::new(&info(1, 8, 32)).unwrap();
        let mut cyc = 100;
        // Power-on control $0C: PRG mode 3, last bank fixed at $C000.
        assert_eq!(c.cpu_peek(0xC000), Some(7));
        mmc1_write(&mut c, 0xE000, 5, &mut cyc);
        assert_eq!(c.cpu_peek(0x8000), Some(5));
        // 4 KiB CHR mode + vertical mirroring.
        mmc1_write(&mut c, 0x8000, 0x1E, &mut cyc);
        assert_eq!(c.mirroring(), Mirroring::Vertical);
        mmc1_write(&mut c, 0xA000, 3, &mut cyc);
        mmc1_write(&mut c, 0xC000, 9, &mut cyc);
        assert_eq!(c.ppu_read(0x0000, 0), 3);
        assert_eq!(c.ppu_read(0x1000, 0), 9);
        // PRG mode 0 (32 KiB): bank 5 -> 4/5.
        mmc1_write(&mut c, 0x8000, 0x02, &mut cyc);
        assert_eq!(c.cpu_peek(0x8000), Some(4));
        assert_eq!(c.cpu_peek(0xC000), Some(5));
    }

    #[test]
    fn mapper_mmc1_consecutive_write_ignored() {
        let mut c = Cartridge::new(&info(1, 8, 32)).unwrap();
        // Bits of 3 = 1,1,0,0,0; the second write lands on the next cycle and is dropped, so
        // one more write is needed to complete the register and the value shifts.
        let mut cyc = 10;
        c.cpu_write(0xE000, 1, cyc);
        c.cpu_write(0xE000, 1, cyc + 1); // ignored (RMW dummy write)
        cyc += 3;
        for bit in [1, 0, 0, 0] {
            c.cpu_write(0xE000, bit, cyc);
            cyc += 2;
        }
        assert_eq!(c.cpu_peek(0x8000), Some(3));
        // Reset bit restores PRG mode 3.
        c.cpu_write(0x8000, 0x80, cyc);
        assert_eq!(c.cpu_peek(0xC000), Some(7));
    }
}
