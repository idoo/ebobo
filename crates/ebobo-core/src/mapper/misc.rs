//! Assorted single-chip/discrete boards: 13 CPROM, 34 BNROM/NINA-001, 32 Irem G-101,
//! 33/48 Taito TC0190/TC0690, 64 Tengen RAMBO-1, 68 Sunsoft-4, 70/152 Bandai 74161,
//! 78 Irem/Jaleco 74HC161, 79/113 NINA-003/006, 87 Jaleco J87, 89 Sunsoft-2, 93 Sunsoft-3R,
//! 118 TxSROM, 140 Jaleco J-11, 180 UNROM fixed-first, 184 Sunsoft-1, 185 CNROM protection,
//! 210 Namco 175/340.
//!
//! One struct for all of them: banks are kept resolved as 8 KiB PRG / 1 KiB CHR bank numbers
//! (`p`, `c`), so `prg_index`/`chr_index` are table lookups; each board's `write` updates them.
//!
//! Not done: 119 TQROM (needs CHR ROM and CHR RAM at once, `Mem` has one or the other),
//! 68's CHR-ROM nametables (`nt_read` not wired to the PPU), 185 "CHR disabled" open-bus reads
//! (no hook returns a CHR byte override; disabled CHR still reads bank 0).

use super::{BoardLogic, Mem, K1, K8};
use crate::cart::{Mirroring, RomInfo};

/// Mapper numbers routed to this file by `Cartridge::new`.
pub(crate) const MAPPERS: &[u16] =
    &[13, 34, 32, 33, 48, 64, 68, 70, 78, 79, 87, 89, 93, 113, 118, 119, 140, 152, 180, 184, 185, 210];

// Mirroring codes in `mir`.
const M_HEADER: u8 = 0;
const M_H: u8 = 1;
const M_V: u8 = 2;
const M_LO: u8 = 3;
const M_HI: u8 = 4;

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Misc {
    mapper: u16,
    sub: u8,
    /// Number of 8 KiB PRG banks.
    n8: u16,
    /// 8 KiB PRG bank per $8000/$A000/$C000/$E000 slot.
    p: [u16; 4],
    /// 1 KiB CHR bank per $0000..$1C00 slot.
    c: [u16; 8],
    mir: u8,
    /// Board registers (meaning per board: MMC3-style bank select + regs, mode bits).
    sel: u8,
    r: [u8; 16],
    ram_on: bool,
    // Scanline/cycle IRQ (48, 64, 118).
    irq_latch: u8,
    irq_counter: u8,
    irq_reload: bool,
    irq_enabled: bool,
    irq: bool,
    irq_cycle_mode: bool,
    prescaler: u8,
    a12_high: bool,
    a12_low_since: u64,
    /// CHR enabled (185).
    chr_on: bool,
}

impl Misc {
    /// `Some` for the implemented mapper numbers.
    pub fn new(info: &RomInfo) -> Option<Self> {
        let n8 = (info.prg_rom.len() / K8).max(1) as u16;
        let mut m = Misc {
            mapper: info.mapper,
            sub: info.submapper,
            n8,
            p: [0, 1, n8.wrapping_sub(2), n8.wrapping_sub(1)],
            c: [0, 1, 2, 3, 4, 5, 6, 7],
            mir: M_HEADER,
            sel: 0,
            r: [0; 16],
            ram_on: true,
            irq_latch: 0,
            irq_counter: 0,
            irq_reload: false,
            irq_enabled: false,
            irq: false,
            irq_cycle_mode: false,
            prescaler: 0,
            a12_high: false,
            a12_low_since: 0,
            chr_on: true,
        };
        match info.mapper {
            119 => return None,
            34 if info.chr_rom.len() > K8 || info.submapper == 1 => m.sub = 1,
            68 => m.ram_on = false,
            78 if info.submapper == 0 => {
                // Holy Diver dumps are usually tagged vertical, Cosmo Carrier horizontal.
                m.sub = if info.mirroring == Mirroring::Vertical { 3 } else { 1 };
            }
            // nesdev: without NES 2.0, battery => 175, else 340 (fits all commercial games).
            210 if info.submapper == 0 => m.sub = if info.battery { 1 } else { 2 },
            64 | 118 => {
                m.r[..8].copy_from_slice(&[0, 2, 4, 5, 6, 7, 0, 1]);
                m.update_mmc3_like();
            }
            _ => {}
        }
        // Discrete boards start at bank 0 with 32 KiB or "16 KiB + fixed last" layouts.
        match info.mapper {
            13 | 34 | 79 | 113 | 140 => m.prg32(0),
            70 | 152 | 78 | 89 | 93 => {
                m.prg16(0, 0);
            }
            180 => {
                m.prg16(0, 0);
                m.prg16(1, 0);
            }
            _ => {}
        }
        Some(m)
    }

    fn prg32(&mut self, b: u8) {
        let b = b as u16 * 4;
        self.p = [b, b + 1, b + 2, b + 3];
    }

    /// 16 KiB bank `b` into half `h` (0 = $8000, 1 = $C000).
    fn prg16(&mut self, h: usize, b: u8) {
        let b = b as u16 * 2;
        self.p[h * 2] = b;
        self.p[h * 2 + 1] = b + 1;
    }

    fn chr8(&mut self, b: u8) {
        let b = b as u16 * 8;
        for i in 0..8 {
            self.c[i] = b + i as u16;
        }
    }

    fn chr4(&mut self, h: usize, b: u8) {
        let b = b as u16 * 4;
        for i in 0..4 {
            self.c[h * 4 + i] = b + i as u16;
        }
    }

    fn chr2(&mut self, slot: usize, b: u8) {
        let b = b as u16 * 2;
        self.c[slot * 2] = b;
        self.c[slot * 2 + 1] = b + 1;
    }

    fn last(&self, k: u16) -> u16 {
        self.n8.wrapping_sub(k)
    }

    /// RAMBO-1 / TxSROM banks from `sel` and `r` (MMC3 layout; RAMBO-1 adds R8/R9/RF).
    fn update_mmc3_like(&mut self) {
        let r = self.r;
        let rambo = self.mapper == 64;
        let c: [u16; 8] = if rambo && self.sel & 0x20 != 0 {
            [r[0], r[8], r[1], r[9], r[2], r[3], r[4], r[5]].map(|v| v as u16)
        } else {
            let (a, b) = (r[0] as u16 & 0xFE, r[1] as u16 & 0xFE);
            [a, a + 1, b, b + 1, r[2] as u16, r[3] as u16, r[4] as u16, r[5] as u16]
        };
        let inv = self.sel & 0x80 != 0;
        for i in 0..8 {
            self.c[if inv { i ^ 4 } else { i }] = c[i];
        }
        let swap = self.sel & 0x40 != 0;
        let (r6, r7) = (r[6] as u16, r[7] as u16);
        self.p = if rambo {
            let rf = r[15] as u16;
            if swap { [rf, r6, r7, self.last(1)] } else { [r6, r7, rf, self.last(1)] }
        } else if swap {
            [self.last(2), r7, r6, self.last(1)]
        } else {
            [r6, r7, self.last(2), self.last(1)]
        };
    }

    /// TxSROM CIRAM page for nametable `addr`: bit 7 (CHR A17) of the 1 KiB bank in the
    /// pattern window with the same A10-A11 ($0000-$0FFF), whichever register holds it.
    fn txsrom_page(&self, addr: u16) -> usize {
        (self.c[((addr >> 10) & 3) as usize] >> 7 & 1) as usize
    }

    /// One MMC3-style counter clock (A12 rise or RAMBO-1 cycle mode).
    fn clock_irq(&mut self) {
        if self.mapper == 64 {
            if self.irq_reload {
                self.irq_counter = self.irq_latch.wrapping_add((self.irq_latch != 0) as u8);
                self.irq_reload = false;
            } else if self.irq_counter == 0 {
                self.irq_counter = self.irq_latch;
            } else {
                self.irq_counter -= 1;
            }
        } else if self.irq_counter == 0 || self.irq_reload {
            self.irq_counter = self.irq_latch;
            self.irq_reload = false;
        } else {
            self.irq_counter -= 1;
        }
        if self.irq_counter == 0 && self.irq_enabled {
            self.irq = true;
        }
    }

    fn uses_a12_irq(&self) -> bool {
        matches!(self.mapper, 48 | 118) || (self.mapper == 64 && !self.irq_cycle_mode)
    }

    /// Writes to $6000-$7FFF that hit board registers (34 NINA-001, 87, 140, 184).
    fn write_reg_6000(&mut self, addr: u16, val: u8) -> bool {
        match self.mapper {
            34 if self.sub == 1 => match addr {
                0x7FFD => self.prg32(val & 1),
                0x7FFE => self.chr4(0, val & 0x0F),
                0x7FFF => self.chr4(1, val & 0x0F),
                _ => return false,
            },
            87 => self.chr8(((val & 1) << 1) | ((val >> 1) & 1)),
            140 => {
                self.prg32((val >> 4) & 3);
                self.chr8(val & 0x0F);
            }
            184 => {
                self.chr4(0, val & 7);
                self.chr4(1, (val >> 4) & 7);
            }
            _ => return false,
        }
        true
    }
}

impl BoardLogic for Misc {
    #[inline]
    fn prg_index(&self, a: usize, _: &Mem) -> usize {
        self.p[a >> 13] as usize * K8 + (a & (K8 - 1))
    }

    #[inline]
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        self.c[a >> 10] as usize * K1 + (a & (K1 - 1))
    }

    fn write(&mut self, addr: u16, val: u8, _: u64, mem: &Mem) {
        match self.mapper {
            13 => self.chr4(1, val & 3),
            34 => {
                if self.sub != 1 {
                    self.prg32(val);
                }
            }
            32 => match addr & 0xF000 {
                0x8000 => {
                    self.r[0] = val & 0x1F;
                    self.update_g101();
                }
                0x9000 => {
                    if self.sub != 1 {
                        self.mir = if val & 1 != 0 { M_H } else { M_V };
                        self.sel = val & 2;
                    }
                    self.update_g101();
                }
                0xA000 => self.p[1] = (val & 0x1F) as u16,
                0xB000 => self.c[(addr & 7) as usize] = val as u16,
                _ => {}
            },
            33 | 48 => match addr & 0xE003 {
                0x8000 => {
                    self.p[0] = (val & 0x3F) as u16;
                    if self.mapper == 33 {
                        self.mir = if val & 0x40 != 0 { M_H } else { M_V };
                    }
                }
                0x8001 => self.p[1] = (val & 0x3F) as u16,
                0x8002 => self.chr2(0, val),
                0x8003 => self.chr2(1, val),
                0xA000..=0xA003 => self.c[4 + (addr & 3) as usize] = val as u16,
                0xC000 if self.mapper == 48 => self.irq_latch = val ^ 0xFF,
                0xC001 if self.mapper == 48 => {
                    self.irq_counter = 0;
                    self.irq_reload = true;
                }
                0xC002 if self.mapper == 48 => self.irq_enabled = true,
                0xC003 if self.mapper == 48 => {
                    self.irq_enabled = false;
                    self.irq = false;
                }
                0xE000 if self.mapper == 48 => {
                    self.mir = if val & 0x40 != 0 { M_H } else { M_V };
                }
                _ => {}
            },
            64 => match addr & 0xE001 {
                0x8000 => {
                    self.sel = val;
                    self.update_mmc3_like();
                }
                0x8001 => {
                    self.r[(self.sel & 0x0F) as usize] = val;
                    self.update_mmc3_like();
                }
                0xA000 => self.mir = if val & 1 != 0 { M_H } else { M_V },
                0xC000 => self.irq_latch = val,
                0xC001 => {
                    self.irq_cycle_mode = val & 1 != 0;
                    self.prescaler = 0;
                    self.irq_reload = true;
                }
                0xE000 => {
                    self.irq_enabled = false;
                    self.irq = false;
                }
                0xE001 => self.irq_enabled = true,
                _ => {}
            },
            118 => match addr & 0xE001 {
                0x8000 => {
                    self.sel = val;
                    self.update_mmc3_like();
                }
                0x8001 => {
                    self.r[(self.sel & 7) as usize] = val;
                    self.update_mmc3_like();
                }
                0xA001 => self.ram_on = val & 0x80 != 0,
                0xC000 => self.irq_latch = val,
                0xC001 => {
                    self.irq_counter = 0;
                    self.irq_reload = true;
                }
                0xE000 => {
                    self.irq_enabled = false;
                    self.irq = false;
                }
                0xE001 => self.irq_enabled = true,
                _ => {}
            },
            68 => match addr & 0xF000 {
                0x8000 => self.chr2(0, val),
                0x9000 => self.chr2(1, val),
                0xA000 => self.chr2(2, val),
                0xB000 => self.chr2(3, val),
                0xE000 => self.mir = [M_V, M_H, M_LO, M_HI][(val & 3) as usize],
                0xF000 => {
                    self.prg16(0, val & 0x0F);
                    self.ram_on = val & 0x10 != 0;
                }
                _ => {}
            },
            70 => {
                self.prg16(0, val >> 4);
                self.chr8(val & 0x0F);
            }
            152 => {
                self.prg16(0, (val >> 4) & 7);
                self.chr8(val & 0x0F);
                self.mir = if val & 0x80 != 0 { M_HI } else { M_LO };
            }
            78 => {
                self.prg16(0, val & 7);
                self.chr8(val >> 4);
                let b = val & 8 != 0;
                self.mir = match (self.sub, b) {
                    (3, true) => M_V,
                    (3, false) => M_H,
                    (_, true) => M_HI,
                    (_, false) => M_LO,
                };
            }
            89 => {
                self.prg16(0, (val >> 4) & 7);
                self.chr8((val & 7) | ((val >> 4) & 8));
                self.mir = if val & 8 != 0 { M_HI } else { M_LO };
            }
            93 => self.prg16(0, (val >> 4) & 7),
            180 => self.prg16(1, val & 7),
            185 => {
                self.chr_on = match self.sub {
                    4..=7 => (val & 3) == (self.sub & 3),
                    _ => (val & 3) != 0 && val != 0x13,
                };
            }
            210 => match addr & 0xF800 {
                0x8000..=0xB800 => self.c[((addr - 0x8000) >> 11) as usize] = val as u16,
                0xC000 => self.ram_on = val & 1 != 0,
                0xE000 => {
                    self.p[0] = (val & 0x3F) as u16;
                    if self.sub == 2 {
                        self.mir = [M_LO, M_V, M_HI, M_H][(val >> 6) as usize];
                    }
                }
                0xE800 => self.p[1] = (val & 0x3F) as u16,
                0xF000 => self.p[2] = (val & 0x3F) as u16,
                _ => {}
            },
            _ => {}
        }
        let _ = mem;
    }

    fn write_low(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        if addr & 0xE100 != 0x4100 {
            return;
        }
        match self.mapper {
            79 => {
                self.prg32((val >> 3) & 1);
                self.chr8(val & 7);
            }
            113 => {
                self.prg32((val >> 3) & 7);
                self.chr8((val & 7) | ((val >> 3) & 8));
                self.mir = if val & 0x80 != 0 { M_V } else { M_H };
            }
            _ => {}
        }
    }

    fn write_ram_area(&mut self, addr: u16, val: u8) -> bool {
        self.write_reg_6000(addr, val)
    }

    fn prg_ram_bank(&self, _: &Mem) -> Option<usize> {
        let has = match self.mapper {
            68 | 118 => self.ram_on,
            210 => self.sub == 1 && self.ram_on,
            87 | 140 | 184 | 79 | 113 => false,
            _ => true,
        };
        has.then_some(0)
    }

    fn ppu_access(&mut self, addr: u16, ppu_dot: u64) {
        if !self.uses_a12_irq() {
            return;
        }
        if addr & 0x1000 != 0 {
            if !self.a12_high {
                self.a12_high = true;
                if ppu_dot.wrapping_sub(self.a12_low_since) >= 10 {
                    self.clock_irq();
                }
            }
        } else if self.a12_high {
            self.a12_high = false;
            self.a12_low_since = ppu_dot;
        }
    }

    fn cpu_tick(&mut self) {
        if self.mapper == 64 && self.irq_cycle_mode {
            self.prescaler += 1;
            if self.prescaler == 4 {
                self.prescaler = 0;
                self.clock_irq();
            }
        }
    }

    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        Some(match self.mir {
            M_HEADER => return None,
            M_H => Mirroring::Horizontal,
            M_V => Mirroring::Vertical,
            M_LO => Mirroring::SingleLower,
            _ => Mirroring::SingleUpper,
        })
    }

    fn irq(&self) -> bool {
        self.irq
    }

    /// TxSROM wires CIRAM A10 to CHR A17, so each nametable needs its own page: hook them.
    fn ppu_hooks(&self) -> bool {
        self.mapper == 118
    }

    fn nt_read(&mut self, addr: u16) -> super::Nt {
        super::Nt::Ciram(self.txsrom_page(addr))
    }

    fn nt_write(&mut self, addr: u16, _val: u8) -> Option<usize> {
        Some(self.txsrom_page(addr))
    }
}

impl Misc {
    /// Irem G-101 PRG layout from reg0 ($8000), reg1 ($A000, written directly) and mode.
    fn update_g101(&mut self) {
        let r0 = self.r[0] as u16;
        if self.sel & 2 != 0 {
            self.p[0] = self.last(2);
            self.p[2] = r0;
        } else {
            self.p[0] = r0;
            self.p[2] = self.last(2);
        }
        self.p[3] = self.last(1);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::Cartridge;
    use crate::cart::Mirroring;

    // test_util: 16 KiB PRG bank b holds b; 4 KiB CHR bank b holds b.

    #[test]
    fn mapper_bnrom_32k() {
        let mut c = Cartridge::new(&info(34, 8, 0)).unwrap();
        c.cpu_write(0x8000, 2, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(4));
        assert_eq!(c.cpu_peek(0xC000), Some(5));
    }

    #[test]
    fn mapper_nina001_regs_in_ram_area() {
        let mut c = Cartridge::new(&info(34, 4, 16)).unwrap();
        c.cpu_write(0x7FFD, 1, 0);
        c.cpu_write(0x7FFE, 3, 0);
        c.cpu_write(0x7FFF, 9, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.ppu_read(0x0000, 0), 3);
        assert_eq!(c.ppu_read(0x1000, 0), 9);
        assert_eq!(c.cpu_peek(0x7FFF), Some(9));
    }

    #[test]
    fn mapper_rambo1_banks_and_cycle_irq() {
        let mut c = Cartridge::new(&info(64, 8, 32)).unwrap();
        c.cpu_write(0x8000, 6, 0);
        c.cpu_write(0x8001, 5, 0); // 8 KiB bank 5 -> 16 KiB bank 2
        c.cpu_write(0x8000, 15, 0);
        c.cpu_write(0x8001, 8, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.cpu_peek(0xC000), Some(4));
        assert_eq!(c.cpu_peek(0xE000), Some(7));
        c.cpu_write(0x8000, 2, 0);
        c.cpu_write(0x8001, 12, 0); // 1 KiB bank 12 -> 4 KiB bank 3
        assert_eq!(c.ppu_read(0x1000, 0), 3);
        c.cpu_write(0xA000, 1, 0);
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        // Cycle mode: latch 2 -> reload to 3, IRQ after 3 more clocks (4 CPU cycles each).
        c.cpu_write(0xC000, 2, 0);
        c.cpu_write(0xC001, 1, 0);
        c.cpu_write(0xE001, 0, 0);
        let mut n = 0;
        while !c.irq() {
            c.cpu_tick();
            n += 1;
            assert!(n < 100);
        }
        assert_eq!(n, 16);
    }

    #[test]
    fn mapper_g101_modes() {
        let mut c = Cartridge::new(&info(32, 8, 32)).unwrap();
        c.cpu_write(0x8000, 4, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.cpu_peek(0xC000), Some(7));
        c.cpu_write(0x9000, 2, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(7));
        assert_eq!(c.cpu_peek(0xC000), Some(2));
        assert_eq!(c.mirroring(), Mirroring::Vertical);
    }

    #[test]
    fn mapper_taito_and_48_irq() {
        let mut c = Cartridge::new(&info(48, 8, 32)).unwrap();
        c.cpu_write(0x8000, 6, 0);
        c.cpu_write(0x8002, 6, 0); // 2 KiB bank 6 -> 4 KiB bank 3
        assert_eq!(c.cpu_peek(0x8000), Some(3));
        assert_eq!(c.ppu_read(0x0000, 0), 3);
        c.cpu_write(0xE000, 0x40, 0);
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0xC000, 0xFF ^ 1, 0); // latch 1
        c.cpu_write(0xC001, 0, 0);
        c.cpu_write(0xC002, 0, 0);
        let mut dot = 0;
        for _ in 0..2 {
            c.ppu_read(0x0000, dot);
            c.ppu_read(0x1000, dot + 20);
            dot += 341;
        }
        assert!(c.irq());
    }

    #[test]
    fn mapper_txsrom_mirroring_from_chr() {
        use super::super::Nt;
        let mut c = Cartridge::new(&info(118, 8, 64)).unwrap();
        assert!(c.ppu_hooks());
        let pages = |c: &mut Cartridge| [0x2000, 0x2400, 0x2800, 0x2C00].map(|a| c.nt_read(a));
        c.cpu_write(0x8000, 0, 0);
        c.cpu_write(0x8001, 0x80, 0);
        c.cpu_write(0x8000, 1, 0);
        c.cpu_write(0x8001, 0x00, 0);
        assert_eq!(pages(&mut c), [Nt::Ciram(1), Nt::Ciram(1), Nt::Ciram(0), Nt::Ciram(0)]);
        c.cpu_write(0x8001, 0x80, 0);
        assert_eq!(pages(&mut c), [Nt::Ciram(1); 4]);
        // CHR mode 1: R2-R5 sit at $0000-$0FFF and pick each nametable independently.
        for (r, v) in [(2, 0x80), (3, 0), (4, 0), (5, 0x80)] {
            c.cpu_write(0x8000, 0x80 | r, 0);
            c.cpu_write(0x8001, v, 0);
        }
        assert_eq!(pages(&mut c), [Nt::Ciram(1), Nt::Ciram(0), Nt::Ciram(0), Nt::Ciram(1)]);
        assert_eq!(c.nt_write(0x2C05, 0), Some(1));
    }

    #[test]
    fn mapper_discrete_misc() {
        let mut c = Cartridge::new(&info(79, 4, 16)).unwrap();
        c.cpu_write(0x4100, 0x0B, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.ppu_read(0x0000, 0), 6);

        let mut c = Cartridge::new(&info(140, 8, 32)).unwrap();
        c.cpu_write(0x6000, 0x23, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(4));
        assert_eq!(c.ppu_read(0x0000, 0), 6);

        let mut c = Cartridge::new(&info(180, 8, 0)).unwrap();
        c.cpu_write(0x8000, 5, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(0));
        assert_eq!(c.cpu_peek(0xC000), Some(5));

        let mut c = Cartridge::new(&info(89, 8, 32)).unwrap();
        c.cpu_write(0x8000, 0x38 | 0x81, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(3));
        assert_eq!(c.cpu_peek(0xC000), Some(7));
        assert_eq!(c.ppu_read(0x0000, 0), 18);
        assert_eq!(c.mirroring(), Mirroring::SingleUpper);

        let mut c = Cartridge::new(&info(184, 2, 16)).unwrap();
        c.cpu_write(0x6000, 0x52, 0);
        assert_eq!(c.ppu_read(0x0000, 0), 2);
        assert_eq!(c.ppu_read(0x1000, 0), 5);

        let mut c = Cartridge::new(&info(210, 8, 32)).unwrap();
        c.cpu_write(0xE000, 0x40 | 4, 0);
        c.cpu_write(0x8800, 8, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.ppu_read(0x0400, 0), 2);
    }
}
