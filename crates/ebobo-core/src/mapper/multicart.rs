//! Pirate multicarts (incl. Dendy-era cartridges): 15 (K-1029 100-in-1 Contra Function 16),
//! 57, 58, 200, 201, 202, 203, 212, 225, 226, 227, 229, 231, 233, 255.
//!
//! Most of them latch PRG/CHR/mirroring from the *address* bits of a $8000+ write, so the
//! board keeps the last write address (`addr`) and/or data (`r0`/`r1`) and derives the banks
//! on each access. Layouts follow the nesdev wiki (Disch's notes) and FCEUX where the wiki
//! is silent.
//!
//! Known simplifications:
//! - 15: CHR-RAM write protection in modes 0/3 is not modelled (no hook for it).
//! - 212: the $6000-$7FFF read quirk (D7 set) is not modelled.
//! - 227: CHR-RAM write protection is not modelled.
//! - 233: mirroring mode %00 ("almost 1ScA": NTB in the bottom-right quadrant) is treated as
//!   one-screen A; the outer 512 KiB bank (PRG A19) toggles on every reset, as in FCEUX.

use super::{BoardLogic, K8, K16, Mem};
use crate::cart::{Mirroring, RomInfo};

/// Mapper numbers routed to this file by `Cartridge::new`.
pub(crate) const MAPPERS: &[u16] = &[15, 57, 58, 200, 201, 202, 203, 212, 225, 226, 227, 229, 231, 233, 255];

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Multicart {
    mapper: u16,
    /// Last write address (address-latched boards).
    addr: u16,
    /// Data registers (meaning per mapper).
    r0: u8,
    r1: u8,
    /// Reset-toggled outer bank (233).
    reset_bit: u8,
    /// Mapper 225 $5800-$5FFF 4×4-bit RAM.
    nib: [u8; 4],
}

impl Multicart {
    /// `Some` for every number in `MAPPERS`.
    pub fn new(info: &RomInfo) -> Option<Self> {
        MAPPERS.contains(&info.mapper).then_some(Multicart {
            mapper: info.mapper,
            addr: 0,
            r0: 0,
            r1: 0,
            reset_bit: 0,
            nib: [0; 4],
        })
    }

    /// 16 KiB PRG banks for $8000 and $C000.
    fn prg16(&self) -> (usize, usize) {
        let a = self.addr as usize;
        let (r0, r1) = (self.r0 as usize, self.r1 as usize);
        let nrom = |b: usize, mode16: bool| if mode16 { (b, b) } else { (b & !1, b | 1) };
        match self.mapper {
            15 => {
                let b = r0 & 0x3F;
                match a & 3 {
                    0 => (b & !1, b | 1),
                    1 => (b, b | 7),
                    _ => (b, b), // mode 2 handled in prg_index (8 KiB)
                }
            }
            57 => nrom((r1 >> 5) & 7, r1 & 0x10 == 0),
            58 => nrom(a & 7, a & 0x40 != 0),
            200 => nrom(a & 7, true),
            201 => nrom((a & 0xFF) << 1, false),
            202 => {
                let b = (a >> 1) & 7;
                if a & 9 == 9 { (b & !1, b | 1) } else { (b, b) }
            }
            203 => nrom((r0 >> 2) as usize, true),
            212 => {
                if a & 0x4000 != 0 { nrom(a & 6, false) } else { nrom(a & 7, true) }
            }
            225 | 255 => {
                let b = ((a >> 6) & 0x3F) | ((a >> 8) & 0x40);
                nrom(b, a & 0x1000 != 0)
            }
            226 => {
                let b = (r0 & 0x1F) | ((r0 & 0x80) >> 2) | ((r1 & 1) << 6);
                nrom(b, r0 & 0x20 != 0)
            }
            227 => {
                let p = ((a >> 2) & 0x1F) | ((a & 0x100) >> 3);
                let s = a & 1 != 0;
                let l = a & 0x200 != 0;
                if a & 0x80 != 0 {
                    nrom(p, !s)
                } else {
                    let lo = if s { p & 0x3E } else { p };
                    (lo, if l { p | 7 } else { p & 0x38 })
                }
            }
            229 => {
                if a & 0x1E == 0 { (0, 1) } else { nrom(a & 0x1F, true) }
            }
            231 => {
                let b = a & 0x1E;
                (b, b | ((a >> 5) & 1))
            }
            233 => {
                let b = (r0 & 0x1F) | ((self.reset_bit as usize) << 5);
                nrom(b, r0 & 0x20 != 0)
            }
            _ => (0, 1),
        }
    }

    /// 8 KiB CHR bank.
    fn chr8(&self) -> usize {
        let a = self.addr as usize;
        let (r0, r1) = (self.r0 as usize, self.r1 as usize);
        match self.mapper {
            57 => {
                // $8000 [CH.. ..AA]: C=bit7 CHR mode (1=NROM, use $8800 bb), H=bit6 CHR A16.
                let low = if r0 & 0x80 != 0 { r1 & 3 } else { r0 & 3 };
                ((r0 & 0x40) >> 3) | (r1 & 4) | low
            }
            58 => (a >> 3) & 7,
            200 => a & 7,
            201 => a & 0xFF,
            202 => (a >> 1) & 7,
            203 => r0 & 3,
            212 => a & 7,
            225 | 255 => (a & 0x3F) | ((a >> 8) & 0x40),
            229 => a & 0xFF,
            _ => 0, // CHR-RAM boards (15, 226, 227, 231, 233)
        }
    }
}

impl BoardLogic for Multicart {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        if self.mapper == 15 && self.addr & 3 == 2 {
            let b = ((self.r0 as usize & 0x3F) << 1) | (self.r0 as usize >> 7);
            return mem.prg_off(b, K8, a & (K8 - 1));
        }
        let (lo, hi) = self.prg16();
        let b = if a < K16 { lo } else { hi };
        mem.prg_off(b, K16, a & (K16 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        self.chr8() * K8 + (a & (K8 - 1))
    }

    fn write(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        match self.mapper {
            57 => {
                if addr & 0x8800 == 0x8800 { self.r1 = val } else { self.r0 = val }
            }
            226 => {
                if addr & 1 == 0 { self.r0 = val } else { self.r1 = val }
            }
            15 | 203 | 233 => {
                self.addr = addr;
                self.r0 = val;
            }
            _ => self.addr = addr,
        }
    }

    fn write_low(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        if self.mapper == 225 && addr >= 0x5800 {
            self.nib[(addr & 3) as usize] = val & 0x0F;
        }
    }

    fn peek_low(&self, addr: u16, _: &Mem) -> Option<u8> {
        (self.mapper == 225 && addr >= 0x5800).then(|| self.nib[(addr & 3) as usize])
    }

    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        let a = self.addr;
        let h = |b: bool| if b { Mirroring::Horizontal } else { Mirroring::Vertical };
        Some(match self.mapper {
            15 => h(self.r0 & 0x40 != 0),
            57 => h(self.r1 & 0x08 != 0),
            58 => h(a & 0x80 != 0),
            200 | 212 => h(a & 0x08 != 0),
            202 => h(a & 1 != 0),
            225 | 255 => h(a & 0x2000 != 0),
            226 => h(self.r0 & 0x40 != 0),
            227 => h(a & 2 != 0),
            229 => h(a & 0x20 != 0),
            231 => h(a & 0x80 != 0),
            233 => match self.r0 >> 6 {
                0 => Mirroring::SingleLower,
                1 => Mirroring::Vertical,
                2 => Mirroring::Horizontal,
                _ => Mirroring::SingleUpper,
            },
            _ => return None, // 201, 203: header mirroring
        })
    }

    fn reset(&mut self) {
        self.addr = 0;
        self.r0 = 0;
        self.r1 = 0;
        if self.mapper == 233 {
            self.reset_bit ^= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::Cartridge;
    use crate::cart::Mirroring;

    /// PRG 16 KiB bank number at $8000/$C000 and CHR 4 KiB bank at $0000.
    fn banks(c: &mut Cartridge) -> (u8, u8, u8) {
        (c.cpu_read(0x8000).unwrap(), c.cpu_read(0xC000).unwrap(), c.ppu_read(0, 0))
    }

    fn cart(m: u16, prg16: usize, chr4: usize) -> Cartridge {
        Cartridge::new(&info(m, prg16, chr4)).unwrap()
    }

    #[test]
    fn m15_modes() {
        let mut c = cart(15, 64, 0);
        c.cpu_write(0x8000, 0x45, 0); // mode 0, bank 5 → 4/5, H
        assert_eq!(banks(&mut c).0..=banks(&mut c).1, 4..=5);
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0x8001, 0x09, 0); // UNROM: 9 / 15
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (9, 15));
        assert_eq!(c.mirroring(), Mirroring::Vertical);
        c.cpu_write(0x8003, 0x0A, 0);
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (10, 10));
        c.cpu_write(0x8002, 0x8A, 0); // 8 KiB: upper half of 16K bank 10 everywhere
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (10, 10));
    }

    #[test]
    fn m57_regs() {
        let mut c = cart(57, 16, 32);
        c.cpu_write(0x8800, 0x6F, 0); // P=3, 16K, H, B=1, bb=3
        c.cpu_write(0x8000, 0x42, 0); // CNROM CHR mode, H=1, AA=2
        let (lo, hi, chr) = banks(&mut c);
        assert_eq!((lo, hi), (3, 3));
        assert_eq!(chr, ((8 | 4 | 2) * 2) as u8);
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0x8000, 0x80, 0); // NROM CHR mode, H=0: bb from $8800
        assert_eq!(banks(&mut c).2, ((4 | 3) * 2) as u8);
        c.cpu_write(0x8800, 0x50, 0); // 32K, P=2 → 2/3
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (2, 3));
    }

    #[test]
    fn m58_address_latch() {
        let mut c = cart(58, 8, 16);
        c.cpu_write(0x80D3, 0, 0); // P=3, C=2, 16K, H
        assert_eq!(banks(&mut c), (3, 3, 4));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0x8003, 0, 0); // 32K
        assert_eq!(banks(&mut c), (2, 3, 0));
        assert_eq!(c.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn m200_201_202() {
        let mut c = cart(200, 8, 16);
        c.cpu_write(0x800D, 0, 0);
        assert_eq!(banks(&mut c), (5, 5, 10));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        let mut c = cart(201, 8, 16);
        c.cpu_write(0x8002, 0, 0);
        assert_eq!(banks(&mut c), (4, 5, 4));
        let mut c = cart(202, 8, 16);
        c.cpu_write(0x8006, 0, 0); // bank 3, 16K, V
        assert_eq!(banks(&mut c), (3, 3, 6));
        assert_eq!(c.mirroring(), Mirroring::Vertical);
        c.cpu_write(0x8009, 0, 0); // bank 4, 32K, H
        assert_eq!(banks(&mut c), (4, 5, 8));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn m203_212() {
        let mut c = cart(203, 16, 8);
        c.cpu_write(0x8000, (5 << 2) | 2, 0);
        assert_eq!(banks(&mut c), (5, 5, 4));
        let mut c = cart(212, 8, 16);
        c.cpu_write(0x800B, 0, 0); // 16K bank 3, H
        assert_eq!(banks(&mut c), (3, 3, 6));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0xC006, 0, 0); // 32K
        assert_eq!(banks(&mut c), (6, 7, 12));
    }

    #[test]
    fn m225_255() {
        for m in [225, 255] {
            let mut c = cart(m, 128, 256);
            // H=1, M=1, O=1, P=3, C=5
            c.cpu_write(0x8000 | 0x4000 | 0x2000 | 0x1000 | (3 << 6) | 5, 0, 0);
            assert_eq!(banks(&mut c), (67, 67, (69 * 2) as u8));
            assert_eq!(c.mirroring(), Mirroring::Horizontal);
            c.cpu_write(0x8000 | (3 << 6), 0, 0);
            assert_eq!(banks(&mut c), (2, 3, 0));
            assert_eq!(c.mirroring(), Mirroring::Vertical);
        }
        let mut c = cart(225, 2, 2);
        c.cpu_write(0x5801, 0xFA, 0);
        assert_eq!(c.cpu_read(0x5801), Some(0x0A));
    }

    #[test]
    fn m226_regs() {
        let mut c = cart(226, 128, 0);
        c.cpu_write(0x8000, 0x80 | 0x40 | 0x20 | 3, 0);
        c.cpu_write(0x8001, 1, 0);
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (99, 99));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0x8000, 3, 0);
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (66, 67));
    }

    #[test]
    fn m227_modes() {
        let mut c = cart(227, 64, 0);
        // O=1, S=0: 16K bank p=5 both; M bit1 → H
        c.cpu_write(0x8080 | (5 << 2) | 2, 0, 0);
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (5, 5));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0x8081 | (5 << 2), 0, 0); // 32K
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (4, 5));
        c.cpu_write(0x8000 | (10 << 2), 0, 0); // UNROM-ish, L=0 → last = p & $38
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (10, 8));
        c.cpu_write(0x8200 | (10 << 2), 0, 0); // L=1 → p|7
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (10, 15));
    }

    #[test]
    fn m229_231() {
        let mut c = cart(229, 32, 64);
        c.cpu_write(0x8000, 0, 0);
        assert_eq!(banks(&mut c), (0, 1, 0));
        c.cpu_write(0x8025, 0, 0);
        assert_eq!(banks(&mut c), (5, 5, 10));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        let mut c = cart(231, 32, 0);
        c.cpu_write(0x80A6, 0, 0); // b=6, A5=1, H
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (6, 7));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn m233_reset_toggles_outer_bank() {
        let mut c = cart(233, 64, 0);
        c.cpu_write(0x8000, 0x80 | 0x20 | 3, 0);
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (3, 3));
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.reset();
        assert_eq!((banks(&mut c).0, banks(&mut c).1), (32, 33));
        assert_eq!(c.mirroring(), Mirroring::SingleLower);
        c.reset();
        assert_eq!(banks(&mut c).0, 0);
    }
}
