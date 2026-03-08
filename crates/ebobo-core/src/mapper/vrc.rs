//! Konami VRC family: VRC2/VRC4 (21, 22, 23, 25; address-line wiring per mapper+submapper,
//! VRC2a CHR shift for 22), VRC6 (24, 26 with A0/A1 swapped; 2 pulse + sawtooth audio) and
//! VRC7 (85; banking + IRQ, FM audio not emulated). VRC4/6/7 share the VRC IRQ counter
//! (scanline mode via a 341/3 prescaler, or CPU-cycle mode), clocked in `cpu_tick`.

use super::{BoardLogic, Mem, K1, K8};
use crate::cart::{Mirroring, RomInfo};

/// Mapper numbers routed to this file by `Cartridge::new`.
pub(crate) const MAPPERS: &[u16] = &[21, 22, 23, 25, 24, 26, 85];

#[derive(Clone, Copy, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Chip {
    Vrc2,
    Vrc4,
    Vrc6,
    Vrc7,
}

/// The VRC IRQ counter (https://www.nesdev.org/wiki/VRC_IRQ).
#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct VrcIrq {
    latch: u8,
    counter: u8,
    prescaler: i16,
    enable: bool,
    enable_after_ack: bool,
    cycle_mode: bool,
    irq: bool,
}

impl VrcIrq {
    fn control(&mut self, v: u8) {
        self.enable_after_ack = v & 1 != 0;
        self.enable = v & 2 != 0;
        self.cycle_mode = v & 4 != 0;
        if self.enable {
            self.counter = self.latch;
            self.prescaler = 341;
        }
        self.irq = false;
    }

    fn ack(&mut self) {
        self.irq = false;
        self.enable = self.enable_after_ack;
    }

    #[inline]
    fn tick(&mut self) {
        if !self.enable {
            return;
        }
        if !self.cycle_mode {
            self.prescaler -= 3;
            if self.prescaler > 0 {
                return;
            }
            self.prescaler += 341;
        }
        if self.counter == 0xFF {
            self.counter = self.latch;
            self.irq = true;
        } else {
            self.counter += 1;
        }
    }
}

/// VRC6 pulse channel.
#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Pulse {
    /// $x000: bit 7 constant (ignore duty), bits 4-6 duty, bits 0-3 volume.
    ctrl: u8,
    period: u16,
    enabled: bool,
    timer: u16,
    step: u8,
}

impl Pulse {
    fn output(&self) -> i32 {
        if !self.enabled {
            return 0;
        }
        let on = self.ctrl & 0x80 != 0 || self.step <= (self.ctrl >> 4 & 7);
        if on { (self.ctrl & 0x0F) as i32 } else { 0 }
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Saw {
    rate: u8,
    period: u16,
    enabled: bool,
    timer: u16,
    step: u8,
    acc: u8,
}

/// VRC6 expansion audio (https://www.nesdev.org/wiki/VRC6_audio).
#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Vrc6Audio {
    pulse: [Pulse; 2],
    saw: Saw,
    /// $9003: bit 0 halt, bit 1 period >> 4, bit 2 period >> 8.
    freq_ctrl: u8,
    level: i32,
}

/// VRC6 pulse at volume 15 ≈ APU pulse at volume 15 (0.1494 of full scale): 32767*0.1494/15.
const VRC6_SCALE: i32 = 326;

impl Vrc6Audio {
    fn shift(&self) -> u32 {
        if self.freq_ctrl & 4 != 0 {
            8
        } else if self.freq_ctrl & 2 != 0 {
            4
        } else {
            0
        }
    }

    fn mix(&mut self) {
        let sum = self.pulse[0].output() + self.pulse[1].output() + (self.saw.acc >> 3) as i32;
        self.level = sum * VRC6_SCALE;
    }

    fn write(&mut self, reg: u16, v: u8) {
        match reg {
            0x9003 => self.freq_ctrl = v,
            0x9000..=0xA002 => {
                let p = &mut self.pulse[(reg >> 12) as usize - 9];
                match reg & 3 {
                    0 => p.ctrl = v,
                    1 => p.period = p.period & 0xF00 | v as u16,
                    _ => {
                        p.period = p.period & 0x0FF | (v as u16 & 0x0F) << 8;
                        p.enabled = v & 0x80 != 0;
                        if !p.enabled {
                            p.step = 15;
                        }
                    }
                }
            }
            0xB000 => self.saw.rate = v & 0x3F,
            0xB001 => self.saw.period = self.saw.period & 0xF00 | v as u16,
            0xB002 => {
                let s = &mut self.saw;
                s.period = s.period & 0x0FF | (v as u16 & 0x0F) << 8;
                s.enabled = v & 0x80 != 0;
                if !s.enabled {
                    s.acc = 0;
                    s.step = 0;
                }
            }
            _ => {}
        }
        self.mix();
    }

    #[inline]
    fn tick(&mut self) {
        if self.freq_ctrl & 1 != 0 {
            return;
        }
        let shift = self.shift();
        let mut changed = false;
        for p in &mut self.pulse {
            if !p.enabled {
                continue;
            }
            if p.timer == 0 {
                p.timer = p.period >> shift;
                p.step = p.step.wrapping_sub(1) & 15;
                changed = true;
            } else {
                p.timer -= 1;
            }
        }
        let s = &mut self.saw;
        if s.enabled {
            if s.timer == 0 {
                s.timer = s.period >> shift;
                s.step += 1;
                if s.step == 14 {
                    s.step = 0;
                    s.acc = 0;
                } else if s.step & 1 == 0 {
                    s.acc = s.acc.wrapping_add(s.rate);
                }
                changed = true;
            } else {
                s.timer -= 1;
            }
        }
        if changed {
            self.mix();
        }
    }
}

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Vrc {
    chip: Chip,
    /// Address bits wired to the chip's A0 / A1 pins (VRC2/4/6), or VRC7's register-select bit.
    a0: u16,
    a1: u16,
    /// VRC2a (mapper 22): CHR bank registers ignore their low bit.
    chr_shift: u8,
    prg: [u8; 3],
    chr: [u16; 8],
    /// VRC4 $9002 bit 1: $8000 and $C000 swapped.
    prg_swap: bool,
    mirror: u8,
    /// VRC6 $B003.
    ppu_mode: u8,
    irq: VrcIrq,
    audio: Vrc6Audio,
}

impl Vrc {
    pub fn new(info: &RomInfo) -> Option<Self> {
        let sub = info.submapper;
        let (chip, a0, a1) = match (info.mapper, sub) {
            (21, 1) => (Chip::Vrc4, 0x02, 0x04),
            (21, 2) => (Chip::Vrc4, 0x40, 0x80),
            (21, _) => (Chip::Vrc4, 0x42, 0x84),
            (22, _) => (Chip::Vrc2, 0x02, 0x01),
            (23, 1) => (Chip::Vrc4, 0x01, 0x02),
            (23, 2) => (Chip::Vrc4, 0x04, 0x08),
            (23, 3) => (Chip::Vrc2, 0x01, 0x02),
            (23, _) => (Chip::Vrc4, 0x05, 0x0A),
            (25, 1) => (Chip::Vrc4, 0x02, 0x01),
            (25, 2) => (Chip::Vrc4, 0x08, 0x04),
            (25, 3) => (Chip::Vrc2, 0x02, 0x01),
            (25, _) => (Chip::Vrc4, 0x0A, 0x05),
            (24, _) => (Chip::Vrc6, 0x01, 0x02),
            (26, _) => (Chip::Vrc6, 0x02, 0x01),
            (85, 1) => (Chip::Vrc7, 0x08, 0),
            (85, 2) => (Chip::Vrc7, 0x10, 0),
            (85, _) => (Chip::Vrc7, 0x18, 0),
            _ => return None,
        };
        Some(Vrc {
            chip,
            a0,
            a1,
            chr_shift: (info.mapper == 22) as u8,
            prg: [0; 3],
            chr: [0; 8],
            prg_swap: false,
            mirror: 0,
            ppu_mode: 0,
            irq: VrcIrq::default(),
            audio: Vrc6Audio::default(),
        })
    }

    fn write_vrc24(&mut self, reg: u16, v: u8) {
        let vrc4 = self.chip == Chip::Vrc4;
        match reg {
            0x8000..=0x8003 => self.prg[0] = v & 0x1F,
            0x9000..=0x9003 => {
                if !vrc4 {
                    self.mirror = v & 1;
                } else if reg <= 0x9001 {
                    self.mirror = v & 3;
                } else if reg == 0x9002 {
                    self.prg_swap = v & 2 != 0;
                }
            }
            0xA000..=0xA003 => self.prg[1] = v & 0x1F,
            0xB000..=0xEFFF => {
                let i = (((reg >> 12) - 0xB) * 2 + (reg >> 1 & 1)) as usize;
                let c = &mut self.chr[i];
                *c = if reg & 1 == 0 {
                    *c & 0x1F0 | (v as u16 & 0x0F)
                } else {
                    let hi = if vrc4 { 0x1F } else { 0x0F };
                    *c & 0x0F | (v as u16 & hi) << 4
                };
            }
            _ if !vrc4 => {}
            0xF000 => self.irq.latch = self.irq.latch & 0xF0 | v & 0x0F,
            0xF001 => self.irq.latch = self.irq.latch & 0x0F | v << 4,
            0xF002 => self.irq.control(v),
            _ => self.irq.ack(),
        }
    }

    fn write_vrc6(&mut self, reg: u16, v: u8) {
        match reg {
            0x8000..=0x8003 => self.prg[0] = v & 0x0F,
            0xB003 => {
                self.ppu_mode = v;
                self.mirror = v >> 2 & 3;
            }
            0x9000..=0xB002 => self.audio.write(reg, v),
            0xC000..=0xC003 => self.prg[1] = v & 0x1F,
            0xD000..=0xE003 => self.chr[((reg >> 12) as usize - 0xD) * 4 + (reg & 3) as usize] = v as u16,
            0xF000 => self.irq.latch = v,
            0xF001 => self.irq.control(v),
            0xF002 => self.irq.ack(),
            _ => {}
        }
    }

    fn write_vrc7(&mut self, reg: u16, v: u8) {
        match reg {
            0x8000 => self.prg[0] = v & 0x3F,
            0x8010 => self.prg[1] = v & 0x3F,
            0x9000 => self.prg[2] = v & 0x3F,
            0xA000..=0xD010 => {
                self.chr[((reg >> 12) as usize - 0xA) * 2 + (reg >> 4 & 1) as usize] = v as u16
            }
            0xE000 => self.mirror = v & 3,
            0xE010 => self.irq.latch = v,
            0xF000 => self.irq.control(v),
            0xF010 => self.irq.ack(),
            _ => {} // $9010/$9030 FM audio: not emulated
        }
    }
}

impl BoardLogic for Vrc {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        let slot = a / K8;
        let last = mem.prg_8k() - 1;
        let bank = match self.chip {
            Chip::Vrc2 | Chip::Vrc4 => match (slot, self.prg_swap) {
                (0, false) | (2, true) => self.prg[0] as usize,
                (1, _) => self.prg[1] as usize,
                (3, _) => last,
                _ => last - 1,
            },
            Chip::Vrc6 => match slot {
                0 | 1 => self.prg[0] as usize * 2 + slot,
                2 => self.prg[1] as usize,
                _ => last,
            },
            Chip::Vrc7 => if slot == 3 { last } else { self.prg[slot] as usize },
        };
        bank * K8 + (a & (K8 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        let slot = a / K1;
        let bank = match self.chip {
            Chip::Vrc6 => match (self.ppu_mode & 3, slot) {
                (0, _) => self.chr[slot] as usize,
                (1, _) => self.chr[slot >> 1] as usize & !1 | slot & 1,
                (_, 0..=3) => self.chr[slot] as usize,
                _ => self.chr[4 + ((slot - 4) >> 1)] as usize & !1 | slot & 1,
            },
            _ => (self.chr[slot] >> self.chr_shift) as usize,
        };
        bank * K1 + (a & (K1 - 1))
    }

    fn write(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        let hi = addr & 0xF000;
        if self.chip == Chip::Vrc7 {
            let sel = if addr & self.a0 != 0 { 0x10 } else { 0 };
            return self.write_vrc7(hi | sel, val);
        }
        let reg = hi | (addr & self.a0 != 0) as u16 | ((addr & self.a1 != 0) as u16) << 1;
        if self.chip == Chip::Vrc6 { self.write_vrc6(reg, val) } else { self.write_vrc24(reg, val) }
    }

    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        Some(match self.mirror {
            0 => Mirroring::Vertical,
            1 => Mirroring::Horizontal,
            2 => Mirroring::SingleLower,
            _ => Mirroring::SingleUpper,
        })
    }

    fn irq(&self) -> bool {
        self.irq.irq
    }

    fn cpu_tick(&mut self) {
        if self.chip == Chip::Vrc2 {
            return;
        }
        self.irq.tick();
        if self.chip == Chip::Vrc6 {
            self.audio.tick();
        }
    }

    fn has_audio(&self) -> bool {
        self.chip == Chip::Vrc6
    }

    fn audio_level(&self) -> i32 {
        self.audio.level
    }
}

#[cfg(test)]
mod tests {
    use crate::apu::{Apu, AudioOut};
    use crate::cart::{Mirroring, Region};
    use crate::mapper::test_util::info;
    use crate::mapper::Cartridge;

    fn cart(mapper: u16, sub: u8, prg16: usize, chr4: usize) -> Cartridge {
        let mut i = info(mapper, prg16, chr4);
        i.submapper = sub;
        Cartridge::new(&i).unwrap()
    }

    #[test]
    fn vrc4b_prg_swap_and_mirroring() {
        // Mapper 25 sub 1 (VRC4b): chip A0 = CPU A1, chip A1 = CPU A0.
        let mut c = cart(25, 1, 16, 2);
        c.cpu_write(0x8000, 4, 0); // 8 KiB bank 4 = 16 KiB bank 2
        c.cpu_write(0xA000, 6, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.cpu_peek(0xA000), Some(3));
        assert_eq!(c.cpu_peek(0xC000), Some(15));
        assert_eq!(c.cpu_peek(0xE000), Some(15));
        c.cpu_write(0x9001, 0x02, 0); // chip $9002: swap mode
        assert_eq!(c.cpu_peek(0x8000), Some(15));
        assert_eq!(c.cpu_peek(0xC000), Some(2));
        c.cpu_write(0x9000, 3, 0);
        assert_eq!(c.mirroring(), Mirroring::SingleUpper);
        c.cpu_write(0x9000, 1, 0);
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
    }

    #[test]
    fn vrc4_chr_nibbles_per_wiring() {
        // (mapper, sub, low-nibble addr, high-nibble addr, next register's low addr)
        for (m, s, lo, hi, next) in [
            (23, 2, 0xB000, 0xB004, 0xB008), // VRC4e A2,A3
            (23, 1, 0xB000, 0xB001, 0xB002), // VRC4f A0,A1
            (21, 2, 0xB000, 0xB040, 0xB080), // VRC4c A6,A7
            (21, 0, 0xB000, 0xB002, 0xB004), // VRC4a via OR wiring
            (25, 2, 0xB000, 0xB008, 0xB004), // VRC4d A3,A2
        ] {
            let mut c = cart(m, s, 2, 64); // 256 x 1 KiB
            c.cpu_write(lo, 0x4, 0); // bank $14 = 4 KiB bank 5
            c.cpu_write(hi, 0x1, 0);
            c.cpu_write(next, 0x8, 0); // bank 8 = 4 KiB bank 2
            assert_eq!(c.ppu_read(0x0000, 0), 5, "mapper {m}.{s}");
            assert_eq!(c.ppu_read(0x0400, 0), 2, "mapper {m}.{s}");
        }
    }

    #[test]
    fn vrc2a_chr_shift() {
        let mut c = cart(22, 0, 8, 32);
        c.cpu_write(0xE000, 0x8, 0); // reg 6, low nibble 8 → bank 4 → 4 KiB bank 1
        c.cpu_write(0xE002, 0x0, 0); // reg 6 high nibble (chip A0 = CPU A1)
        c.cpu_write(0x9000, 1, 0);
        assert_eq!(c.ppu_read(0x1800, 0), 1);
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0xF002, 0x02, 0); // VRC2: no IRQ
        for _ in 0..1000 {
            c.cpu_tick();
        }
        assert!(!c.irq());
    }

    #[test]
    fn vrc_irq_cycle_mode() {
        let mut c = cart(23, 1, 2, 2);
        c.cpu_write(0xF000, 0xE, 0);
        c.cpu_write(0xF001, 0xF, 0); // latch $FE
        c.cpu_write(0xF002, 0x06, 0); // enable, cycle mode
        c.cpu_tick();
        assert!(!c.irq());
        c.cpu_tick();
        assert!(c.irq());
        c.cpu_write(0xF003, 0, 0); // ack: E := A = 0 → stopped
        assert!(!c.irq());
        for _ in 0..1000 {
            c.cpu_tick();
        }
        assert!(!c.irq());
    }

    #[test]
    fn vrc_irq_scanline_mode() {
        let mut c = cart(23, 1, 2, 2);
        c.cpu_write(0xF000, 0xF, 0);
        c.cpu_write(0xF001, 0xF, 0); // latch $FF: every scanline
        c.cpu_write(0xF002, 0x03, 0); // enable, A=1, scanline mode
        let mut fired = Vec::new();
        for cyc in 1..=400 {
            c.cpu_tick();
            if c.irq() {
                fired.push(cyc);
                c.cpu_write(0xF003, 0, 0);
            }
        }
        // 341/3 prescaler: 113.67 CPU cycles per scanline.
        assert_eq!(fired, [114, 228, 341]);
    }

    #[test]
    fn vrc6_banking_irq_and_mirroring() {
        // Mapper 26: A0/A1 swapped.
        let mut c = cart(26, 0, 8, 32);
        c.cpu_write(0x8000, 3, 0);
        c.cpu_write(0xC000, 10, 0); // 8 KiB 10 = 16 KiB 5
        assert_eq!(c.cpu_peek(0x8000), Some(3));
        assert_eq!(c.cpu_peek(0xA000), Some(3));
        assert_eq!(c.cpu_peek(0xC000), Some(5));
        assert_eq!(c.cpu_peek(0xE000), Some(7));
        c.cpu_write(0xB003, 0x20 | 1 << 2, 0); // chip $B003 = CPU $B003 either way
        assert_eq!(c.mirroring(), Mirroring::Horizontal);
        c.cpu_write(0xD002, 12, 0); // CPU $D002 = chip $D001 on mapper 26
        assert_eq!(c.ppu_read(0x0400, 0), 3);
        c.cpu_write(0xF000, 0xFF, 0);
        c.cpu_write(0xF002, 0x06, 0); // CPU $F002 = chip $F001 control
        c.cpu_tick();
        assert!(c.irq());
        c.cpu_write(0xF001, 0, 0); // chip $F002 ack
        assert!(!c.irq());
    }

    #[test]
    fn vrc7_banking_variants() {
        for (sub, sel) in [(1u8, 0x08u16), (2, 0x10), (0, 0x10)] {
            let mut c = cart(85, sub, 16, 64);
            c.cpu_write(0x8000, 2, 0);
            c.cpu_write(0x8000 | sel, 5, 0);
            c.cpu_write(0x9000, 8, 0);
            assert_eq!(c.cpu_peek(0x8000), Some(1));
            assert_eq!(c.cpu_peek(0xA000), Some(2));
            assert_eq!(c.cpu_peek(0xC000), Some(4));
            assert_eq!(c.cpu_peek(0xE000), Some(15));
            c.cpu_write(0xD000 | sel, 8, 0); // CHR slot 7 = 4 KiB bank 2
            assert_eq!(c.ppu_read(0x1C00, 0), 2);
            c.cpu_write(0xE000, 2, 0);
            assert_eq!(c.mirroring(), Mirroring::SingleLower);
            c.cpu_write(0xE000 | sel, 0xFF, 0);
            c.cpu_write(0xF000, 0x06, 0);
            c.cpu_tick();
            assert!(c.irq());
            c.cpu_write(0xF000 | sel, 0, 0);
            assert!(!c.irq());
        }
    }

    #[test]
    fn vrc6_pulse_through_audio_out() {
        let mut c = cart(24, 0, 2, 2);
        assert!(c.has_expansion_audio());
        c.cpu_write(0x9000, 0x7F, 0); // duty 7/16 → 50%, volume 15
        c.cpu_write(0x9001, 0xFF, 0);
        c.cpu_write(0x9002, 0x80, 0); // enable, period $0FF → 1789773 / (16 * 256) ≈ 437 Hz
        let mut apu = Apu::new(Region::Ntsc);
        let mut out = AudioOut::new(Region::Ntsc, 48_000.0);
        let cycles = 1_789_773; // one second
        for _ in 0..cycles {
            out.push_ext(c.expansion_audio());
            apu.tick(&mut out);
            c.cpu_tick();
        }
        let mut s = Vec::new();
        out.drain(&mut s);
        let s = &s[4800..]; // skip filter settling
        let peak = s.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.05, "silent: peak {peak}");
        let crossings = s.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count() as f64;
        let hz = crossings / (s.len() as f64 / 48_000.0);
        assert!((hz - 437.0).abs() < 5.0, "frequency {hz}");
    }
}
