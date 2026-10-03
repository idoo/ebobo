//! Action 53 (28): homebrew multicart board. $5000-$5FFF selects one of four registers
//! (value bits 7 and 0), $8000-$FFFF writes it. Outer bank + inner bank with a game size,
//! 32 KiB or 16 KiB (fixed low/high half) PRG modes, 4x8 KiB CHR RAM, 1-screen/V/H mirroring.

use super::{BoardLogic, K8, K16, Mem};
use crate::cart::Mirroring;

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Action53 {
    select: u8,
    chr: u8,
    inner: u8,
    mode: u8,
    outer: u8,
}

impl Default for Action53 {
    /// Outer bank $FF and 32 KiB mode: the menu in the last 32 KiB boots.
    fn default() -> Self {
        Action53 {
            select: 0,
            chr: 0,
            inner: 0,
            mode: 0,
            outer: 0xFF,
        }
    }
}

impl Action53 {
    /// 16 KiB PRG bank (unwrapped) for the CPU half `a14` (0 = $8000, 1 = $C000).
    fn prg_bank(&self, a14: usize) -> usize {
        let size = (self.mode >> 4) & 3;
        let mask = (2usize << size) - 1;
        let outer = (self.outer as usize) << 1;
        let inner = self.inner as usize & 0x0F;
        if self.mode & 0x08 == 0 {
            // 32 KiB: inner bank selects 32 KiB inside the game, A14 the half.
            (outer & !mask) | (((inner << 1) | a14) & mask)
        } else if a14 == ((self.mode >> 2) & 1) as usize {
            // Fixed half: bit 2 = 0 fixes $8000 to the game's first bank, 1 fixes $C000 to its last.
            outer | a14
        } else {
            (outer & !mask) | (inner & mask)
        }
    }

    /// Register write of $8000-$FFFF to the register picked by `select`.
    fn set_reg(&mut self, val: u8) {
        if self.select & 0x80 == 0 && self.mode & 2 == 0 {
            // Registers $00/$01 also drive 1-screen mirroring while not in V/H mode.
            self.mode = (self.mode & !1) | ((val >> 4) & 1);
        }
        match self.select {
            0x00 => self.chr = val & 3,
            0x01 => self.inner = val & 0x0F,
            0x80 => self.mode = val & 0x3F,
            _ => self.outer = val,
        }
    }

    fn chr_base(&self) -> usize {
        (self.chr & 3) as usize * K8
    }

    fn mirror(&self) -> Mirroring {
        match self.mode & 3 {
            0 => Mirroring::SingleLower,
            1 => Mirroring::SingleUpper,
            2 => Mirroring::Vertical,
            _ => Mirroring::Horizontal,
        }
    }
}

impl BoardLogic for Action53 {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        mem.prg_off(self.prg_bank(a / K16), K16, a & (K16 - 1))
    }
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        self.chr_base() + a
    }
    fn write_low(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        if addr >= 0x5000 {
            self.select = (val & 0x80) | (val & 1);
        }
    }
    fn write(&mut self, _: u16, val: u8, _: u64, _: &Mem) {
        self.set_reg(val);
    }
    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        Some(self.mirror())
    }
    fn reset(&mut self) {
        *self = Action53::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(mode: u8, outer: u8, inner: u8) -> Action53 {
        Action53 {
            select: 0,
            chr: 0,
            inner,
            mode,
            outer,
        }
    }

    #[test]
    fn action53_reset_maps_last_32k() {
        let b = Action53::default();
        assert_eq!((b.prg_bank(0), b.prg_bank(1)), (0x1FE, 0x1FF));
    }

    #[test]
    fn action53_32k_mode_masks_by_game_size() {
        // size 0 (32K): inner ignored
        assert_eq!(board(0x00, 3, 5).prg_bank(0), 6);
        // size 1 (64K): inner bit 0 counts
        let b = board(0x10, 3, 5);
        assert_eq!((b.prg_bank(0), b.prg_bank(1)), (6 | 2, 6 | 3));
        // size 3 (256K): inner bits 0-2
        assert_eq!(board(0x30, 0x10, 7).prg_bank(1), 0x20 | 15);
    }

    #[test]
    fn action53_16k_fixed_modes() {
        // mode 2: $8000 fixed to outer's first bank, $C000 = inner
        let b = board(0x08 | 0x10, 4, 3);
        assert_eq!((b.prg_bank(0), b.prg_bank(1)), (8, 8 | 3));
        // mode 3: $C000 fixed to outer's last bank, $8000 = inner
        let b = board(0x0C | 0x20, 4, 6);
        assert_eq!((b.prg_bank(0), b.prg_bank(1)), (8 | 6, 9));
    }

    #[test]
    fn action53_registers_and_mirroring() {
        let mut b = Action53::default();
        b.select = 0x00;
        b.set_reg(0x13);
        assert_eq!((b.chr_base(), b.mirror()), (3 * K8, Mirroring::SingleUpper));
        b.select = 0x80;
        b.set_reg(0x02);
        assert_eq!(b.mirror(), Mirroring::Vertical);
        b.select = 0x01;
        b.set_reg(0x1F);
        assert_eq!((b.inner, b.mirror()), (0x0F, Mirroring::Vertical));
        b.select = 0x81;
        b.set_reg(0x07);
        assert_eq!(b.outer, 7);
        b.select = 0x80;
        b.set_reg(0x03);
        assert_eq!(b.mirror(), Mirroring::Horizontal);
    }
}
