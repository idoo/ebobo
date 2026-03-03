//! MMC2 / PxROM (mapper 9): CHR latches flipped by pattern fetches of tiles $FD/$FE.
//! MMC4 / FxROM (mapper 10): same latches (latch 0 also on $0FD8-$0FDF/$0FE8-$0FEF), 16 KiB
//! PRG banking at $8000 with the last bank fixed, and PRG RAM.

use super::{BoardLogic, Mem, K16, K4, K8};
use crate::cart::Mirroring;

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Mmc2 {
    prg: u8,
    /// $B000 (FD, $0xxx), $C000 (FE, $0xxx), $D000 (FD, $1xxx), $E000 (FE, $1xxx).
    chr: [u8; 4],
    /// `true` = latch holds $FE.
    latch0_fe: bool,
    latch1_fe: bool,
    horizontal: bool,
    mmc4: bool,
}

impl Mmc2 {
    pub fn new() -> Self {
        Mmc2 { prg: 0, chr: [0; 4], latch0_fe: true, latch1_fe: true, horizontal: false, mmc4: false }
    }

    pub fn new_mmc4() -> Self {
        Mmc2 { mmc4: true, ..Self::new() }
    }
}

impl BoardLogic for Mmc2 {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        if self.mmc4 {
            let bank = if a < K16 { self.prg as usize } else { mem.prg_16k() - 1 };
            return mem.prg_off(bank, K16, a & (K16 - 1));
        }
        let n8 = mem.prg_8k();
        let slot = a / K8;
        let bank = if slot == 0 { self.prg as usize } else { n8.wrapping_sub(4 - slot) };
        mem.prg_off(bank, K8, a & (K8 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        let reg = if a < K4 {
            if self.latch0_fe { self.chr[1] } else { self.chr[0] }
        } else if self.latch1_fe {
            self.chr[3]
        } else {
            self.chr[2]
        };
        reg as usize * K4 + (a & 0xFFF)
    }

    fn write(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        match addr >> 12 {
            0xA => self.prg = val & 0x0F,
            0xB => self.chr[0] = val & 0x1F,
            0xC => self.chr[1] = val & 0x1F,
            0xD => self.chr[2] = val & 0x1F,
            0xE => self.chr[3] = val & 0x1F,
            0xF => self.horizontal = val & 1 != 0,
            _ => {}
        }
    }

    #[inline]
    fn ppu_after_read(&mut self, addr: u16) -> bool {
        match addr & 0x1FFF {
            0x0FD8 => { self.latch0_fe = false; true }
            0x0FE8 => { self.latch0_fe = true; true }
            0x0FD9..=0x0FDF if self.mmc4 => { self.latch0_fe = false; true }
            0x0FE9..=0x0FEF if self.mmc4 => { self.latch0_fe = true; true }
            0x1FD8..=0x1FDF => { self.latch1_fe = false; true }
            0x1FE8..=0x1FEF => { self.latch1_fe = true; true }
            _ => false,
        }
    }

    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        Some(if self.horizontal { Mirroring::Horizontal } else { Mirroring::Vertical })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::Cartridge;

    #[test]
    fn mapper_mmc2_latches() {
        let mut c = Cartridge::new(&info(9, 8, 32)).unwrap();
        c.cpu_write(0xB000, 4, 0);
        c.cpu_write(0xC000, 6, 0);
        assert_eq!(c.ppu_read(0x0000, 0), 6);
        c.ppu_read(0x0FD8, 0);
        assert_eq!(c.ppu_read(0x0000, 0), 4);
        c.ppu_read(0x0FE8, 0);
        assert_eq!(c.ppu_read(0x0000, 0), 6);
    }

    #[test]
    fn mapper_mmc4_prg16_and_latch_range() {
        let mut c = Cartridge::new(&super::super::test_util::info(10, 8, 32)).unwrap();
        c.cpu_write(0xA000, 3, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(3));
        assert_eq!(c.cpu_peek(0xC000), Some(7));
        c.cpu_write(0xB000, 4, 0);
        c.cpu_write(0xC000, 6, 0);
        c.ppu_read(0x0FDA, 0);
        assert_eq!(c.ppu_read(0x0000, 0), 4);
    }
}
