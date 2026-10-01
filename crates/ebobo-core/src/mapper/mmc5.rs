//! MMC5 / ExROM (mapper 5), per https://www.nesdev.org/wiki/MMC5.
//!
//! Implemented: PRG modes 0-3 ($5100) with ROM/RAM per 8 KiB ($5113-$5117, RAM also in
//! $8000-$DFFF), PRG RAM protect ($5102/$5103), CHR modes 0-3 ($5101) with separate sprite (A,
//! $5120-$5127) and BG (B, $5128-$512B) sets in 8x16 mode, $5130 upper CHR bits, ExRAM
//! $5C00-$5FFF in modes 0-3 ($5104: nametable, extended attributes, RAM, ROM), nametable mapping
//! $5105 with fill mode $5106/$5107, scanline IRQ $5203/$5204, multiplier $5205/$5206, and
//! expansion audio (https://www.nesdev.org/wiki/MMC5_audio): two sweepless pulses $5000-$5007
//! with length/envelope clocked at a fixed 240 Hz from the CPU clock, PCM write mode
//! $5010/$5011, enable/status $5015.
//!
//! PPU coupling goes through the `ppu_hooks` family: every nametable fetch reaches `nt_read`
//! (scanline detection = 3 identical consecutive nametable fetch addresses, as the real chip
//! does), `ppu_fetch` tells BG from sprite phases, and "PPU idle for 3 CPU cycles" (no PPU
//! fetches seen in `cpu_tick`) ends the frame.
//!
//! Not implemented: vertical split ($5200-$5202, writes are ignored; only Uchuu Keibitai SDF
//! uses it), PCM read mode (sampling $8000-$BFFF reads) and the PCM IRQ.

use super::{BoardLogic, K1, K4, K8, Mem, Nt};
use crate::cart::RomInfo;

/// Mapper numbers routed to this file by `Cartridge::new`.
pub(crate) const MAPPERS: &[u16] = &[5];

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Mmc5 {
    prg_mode: u8,
    chr_mode: u8,
    ram_protect: [u8; 2],
    exram_mode: u8,
    nt_map: u8,
    fill_tile: u8,
    fill_attr: u8,
    /// $5113-$5117.
    prg: [u8; 5],
    /// Sprite / 8x8 set ($5120-$5127), with $5130 bits in 8-9.
    chr_a: [u16; 8],
    /// BG set in 8x16 mode ($5128-$512B).
    chr_b: [u16; 4],
    chr_upper: u8,
    /// Last CHR register written was in set B.
    last_b: bool,
    #[serde(with = "crate::big_array")]
    exram: [u8; 1024],
    irq_target: u8,
    irq_enabled: bool,
    irq_pending: bool,
    in_frame: bool,
    line: u8,
    last_nt: u16,
    nt_matches: u8,
    /// CPU cycles since the last PPU fetch.
    idle: u8,
    /// PPU is rendering (a fetch phase was signalled and the PPU has not gone idle).
    active: bool,
    sprite_phase: bool,
    tall: bool,
    /// ExRAM byte of the BG tile being fetched (extended attribute mode).
    ex_latch: u8,
    mul: [u8; 2],
    audio: Mmc5Audio,
}

/// APU length counter load table.
const LENGTH: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22,
    192, 24, 72, 26, 16, 28, 32, 30,
];

/// APU pulse duty sequences.
const DUTY: [u8; 4] = [0b0100_0000, 0b0110_0000, 0b0111_1000, 0b1001_1111];

/// CPU cycles per 240 Hz length/envelope clock (1789773 / 240).
const FRAME_PERIOD: u16 = 7457;

/// Pulse at volume 15 ≈ APU pulse at volume 15 (0.1494 of full scale), as `vrc::VRC6_SCALE`.
const PULSE_SCALE: i32 = 326;
/// PCM 255 ≈ DMC 127 through the APU TND mixer (≈0.335 of full scale): 32767*0.335/255.
const PCM_SCALE: i32 = 43;

/// MMC5 pulse: an APU pulse without sweep.
#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Pulse {
    /// $5000/$5004: DDLC VVVV (duty, length halt / envelope loop, constant volume, volume).
    ctrl: u8,
    period: u16,
    timer: u16,
    step: u8,
    length: u8,
    env_start: bool,
    env_divider: u8,
    env_decay: u8,
}

impl Pulse {
    fn output(&self) -> i32 {
        if self.length == 0 || DUTY[(self.ctrl >> 6) as usize] & (0x80 >> self.step) == 0 {
            return 0;
        }
        (if self.ctrl & 0x10 != 0 { self.ctrl & 0x0F } else { self.env_decay }) as i32
    }

    fn clock_frame(&mut self) {
        let vol = self.ctrl & 0x0F;
        if self.env_start {
            self.env_start = false;
            self.env_decay = 15;
            self.env_divider = vol;
        } else if self.env_divider == 0 {
            self.env_divider = vol;
            if self.env_decay > 0 {
                self.env_decay -= 1;
            } else if self.ctrl & 0x20 != 0 {
                self.env_decay = 15;
            }
        } else {
            self.env_divider -= 1;
        }
        if self.ctrl & 0x20 == 0 && self.length > 0 {
            self.length -= 1;
        }
    }
}

/// MMC5 expansion audio state.
#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Mmc5Audio {
    pulse: [Pulse; 2],
    /// $5015 bits 0-1.
    enable: u8,
    /// $5010.
    pcm_ctrl: u8,
    pcm: u8,
    /// Pulse timers run every other CPU cycle.
    odd: bool,
    frame: u16,
    level: i32,
}

impl Mmc5Audio {
    fn mix(&mut self) {
        let pulses = self.pulse[0].output() + self.pulse[1].output();
        self.level = pulses * PULSE_SCALE + self.pcm as i32 * PCM_SCALE;
    }

    fn write(&mut self, addr: u16, v: u8) {
        match addr {
            0x5000..=0x5007 => {
                let i = (addr as usize >> 2) & 1;
                let p = &mut self.pulse[i];
                match addr & 3 {
                    0 => p.ctrl = v,
                    1 => {} // no sweep
                    2 => p.period = p.period & 0x700 | v as u16,
                    _ => {
                        p.period = p.period & 0x0FF | (v as u16 & 7) << 8;
                        if self.enable >> i & 1 != 0 {
                            p.length = LENGTH[(v >> 3) as usize];
                        }
                        p.step = 0;
                        p.env_start = true;
                    }
                }
            }
            0x5010 => self.pcm_ctrl = v,
            0x5011 if self.pcm_ctrl & 1 == 0 && v != 0 => self.pcm = v,
            0x5015 => {
                self.enable = v & 3;
                for (i, p) in self.pulse.iter_mut().enumerate() {
                    if v >> i & 1 == 0 {
                        p.length = 0;
                    }
                }
            }
            _ => return,
        }
        self.mix();
    }

    fn status(&self) -> u8 {
        (self.pulse[0].length > 0) as u8 | ((self.pulse[1].length > 0) as u8) << 1
    }

    #[inline]
    fn tick(&mut self) {
        self.odd = !self.odd;
        if self.odd {
            for p in &mut self.pulse {
                if p.timer == 0 {
                    p.timer = p.period;
                    p.step = (p.step + 1) & 7;
                } else {
                    p.timer -= 1;
                }
            }
        }
        self.frame += 1;
        if self.frame >= FRAME_PERIOD {
            self.frame = 0;
            for p in &mut self.pulse {
                p.clock_frame();
            }
        }
        self.mix();
    }
}

impl Mmc5 {
    pub fn new(_info: &RomInfo) -> Option<Self> {
        Some(Mmc5 {
            prg_mode: 3,
            chr_mode: 3,
            ram_protect: [0; 2],
            exram_mode: 0,
            nt_map: 0,
            fill_tile: 0,
            fill_attr: 0,
            prg: [0, 0, 0, 0, 0xFF],
            chr_a: [0; 8],
            chr_b: [0; 4],
            chr_upper: 0,
            last_b: false,
            exram: [0; 1024],
            irq_target: 0,
            irq_enabled: false,
            irq_pending: false,
            in_frame: false,
            line: 0,
            last_nt: 0xFFFF,
            nt_matches: 0,
            idle: 0,
            active: false,
            sprite_phase: false,
            tall: false,
            ex_latch: 0,
            mul: [0xFF, 0xFF],
            audio: Mmc5Audio::default(),
        })
    }

    /// 8 KiB bank and ROM flag of CPU slot `s` (0 = $8000 .. 3 = $E000).
    fn prg_slot(&self, s: usize) -> (usize, bool) {
        let r16 = |v: u8| ((v as usize & 0x7E) | (s & 1), v & 0x80 != 0);
        let r8 = |v: u8| (v as usize & 0x7F, v & 0x80 != 0);
        match (self.prg_mode & 3, s) {
            (0, _) => ((self.prg[4] as usize & 0x7C) | s, true),
            (1, 0 | 1) | (2, 0 | 1) => r16(self.prg[2]),
            (1, _) => (r16(self.prg[4]).0, true),
            (2, 2) => r8(self.prg[3]),
            (_, 3) => (r8(self.prg[4]).0, true),
            (_, s) => r8(self.prg[s + 1]),
        }
    }

    fn scanline_detected(&mut self) {
        if !self.in_frame {
            self.in_frame = true;
            self.line = 0;
            self.irq_pending = false;
        } else {
            self.line = self.line.wrapping_add(1);
            if self.line == self.irq_target {
                self.irq_pending = true;
            }
        }
    }

    fn ext_attr(&self) -> bool {
        self.exram_mode == 1 && self.active && !self.sprite_phase
    }
}

impl BoardLogic for Mmc5 {
    fn prg_index(&self, a: usize, _: &Mem) -> usize {
        self.prg_slot(a >> 13).0 * K8 + (a & (K8 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        let a = a & 0x1FFF;
        if self.ext_attr() {
            let bank = ((self.chr_upper as usize & 3) << 6) | (self.ex_latch as usize & 0x3F);
            return bank * K4 + (a & (K4 - 1));
        }
        let use_b = if self.tall && self.active { !self.sprite_phase } else { self.last_b };
        let (bank, unit) = match self.chr_mode & 3 {
            0 => (if use_b { self.chr_b[3] } else { self.chr_a[7] }, K8),
            1 => (if use_b { self.chr_b[3] } else { self.chr_a[(a >> 12) * 4 + 3] }, K4),
            2 => {
                let b = if use_b { self.chr_b[((a >> 11) & 1) * 2 + 1] } else { self.chr_a[(a >> 11) * 2 + 1] };
                (b, 2 * K1)
            }
            _ => (if use_b { self.chr_b[(a >> 10) & 3] } else { self.chr_a[a >> 10] }, K1),
        };
        bank as usize * unit + (a & (unit - 1))
    }

    fn write(&mut self, _: u16, _: u8, _: u64, _: &Mem) {}

    fn write_low(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        match addr {
            0x5100 => self.prg_mode = val & 3,
            0x5101 => self.chr_mode = val & 3,
            0x5102 => self.ram_protect[0] = val & 3,
            0x5103 => self.ram_protect[1] = val & 3,
            0x5104 => self.exram_mode = val & 3,
            0x5105 => self.nt_map = val,
            0x5106 => self.fill_tile = val,
            0x5107 => self.fill_attr = val & 3,
            0x5113..=0x5117 => self.prg[(addr - 0x5113) as usize] = val,
            0x5120..=0x5127 => {
                self.chr_a[(addr - 0x5120) as usize] = val as u16 | (self.chr_upper as u16) << 8;
                self.last_b = false;
            }
            0x5128..=0x512B => {
                self.chr_b[(addr - 0x5128) as usize] = val as u16 | (self.chr_upper as u16) << 8;
                self.last_b = true;
            }
            0x5130 => self.chr_upper = val & 3,
            0x5203 => self.irq_target = val,
            0x5204 => self.irq_enabled = val & 0x80 != 0,
            0x5205 => self.mul[0] = val,
            0x5206 => self.mul[1] = val,
            0x5000..=0x5015 => self.audio.write(addr, val),
            0x5C00..=0x5FFF => {
                let i = (addr - 0x5C00) as usize;
                match self.exram_mode {
                    0 | 1 => self.exram[i] = if self.in_frame { val } else { 0 },
                    2 => self.exram[i] = val,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn read_low(&mut self, addr: u16, mem: &Mem) -> Option<u8> {
        let v = self.peek_low(addr, mem);
        if addr == 0x5204 {
            self.irq_pending = false;
        }
        v
    }

    fn peek_low(&self, addr: u16, _: &Mem) -> Option<u8> {
        let product = self.mul[0] as u16 * self.mul[1] as u16;
        match addr {
            0x5204 => Some((self.irq_pending as u8) << 7 | (self.in_frame as u8) << 6),
            0x5015 => Some(self.audio.status()),
            0x5205 => Some(product as u8),
            0x5206 => Some((product >> 8) as u8),
            0x5C00..=0x5FFF if self.exram_mode >= 2 => Some(self.exram[(addr - 0x5C00) as usize]),
            _ => None,
        }
    }

    fn prg_ram_bank(&self, _: &Mem) -> Option<usize> {
        Some(self.prg[0] as usize & 7)
    }

    fn prg_ram_writable(&self) -> bool {
        self.ram_protect == [2, 1]
    }

    fn prg_high_ram(&self, a: usize) -> Option<usize> {
        let (bank, rom) = self.prg_slot(a >> 13);
        (!rom).then_some((bank & 7) * K8 + (a & (K8 - 1)))
    }

    fn ppu_hooks(&self) -> bool {
        true
    }

    fn ppu_fetch(&mut self, sprites: bool, tall: bool) {
        self.active = true;
        self.sprite_phase = sprites;
        self.tall = tall;
        self.idle = 0;
    }

    fn ppu_access(&mut self, _: u16, _: u64) {
        self.idle = 0;
    }

    fn nt_read(&mut self, addr: u16) -> Nt {
        self.idle = 0;
        if addr == self.last_nt {
            self.nt_matches = self.nt_matches.saturating_add(1);
            if self.nt_matches == 2 {
                self.scanline_detected();
            }
        } else {
            self.last_nt = addr;
            self.nt_matches = 0;
        }
        let off = (addr & 0x3FF) as usize;
        let attr = off >= 0x3C0;
        if self.ext_attr() {
            if attr {
                return Nt::Value((self.ex_latch >> 6) * 0x55);
            }
            self.ex_latch = self.exram[off];
        }
        match (self.nt_map >> (((addr >> 10) & 3) * 2)) & 3 {
            0 => Nt::Ciram(0),
            1 => Nt::Ciram(1),
            2 => Nt::Value(if self.exram_mode <= 1 { self.exram[off] } else { 0 }),
            _ => Nt::Value(if attr { self.fill_attr * 0x55 } else { self.fill_tile }),
        }
    }

    fn nt_write(&mut self, addr: u16, val: u8) -> Option<usize> {
        match (self.nt_map >> (((addr >> 10) & 3) * 2)) & 3 {
            0 => Some(0),
            1 => Some(1),
            2 => {
                if self.exram_mode <= 1 {
                    self.exram[(addr & 0x3FF) as usize] = val;
                }
                None
            }
            _ => None,
        }
    }

    fn irq(&self) -> bool {
        self.irq_pending && self.irq_enabled
    }

    fn cpu_tick(&mut self) {
        if self.active || self.in_frame {
            self.idle += 1;
            if self.idle >= 3 {
                self.active = false;
                self.in_frame = false;
                self.nt_matches = 0;
                self.last_nt = 0xFFFF;
            }
        }
        self.audio.tick();
    }

    fn has_audio(&self) -> bool {
        true
    }

    fn audio_level(&self) -> i32 {
        self.audio.level
    }

    fn reset(&mut self) {
        self.in_frame = false;
        self.active = false;
        self.irq_pending = false;
        self.irq_enabled = false;
        self.prg_mode = 3;
        self.prg[4] = 0xFF;
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::info;
    use super::super::{Cartridge, K8};
    use crate::cart::Region;
    use crate::ppu::Ppu;

    /// 512 KiB PRG (8 KiB banks hold their 8 KiB number), 256 KiB CHR (1 KiB banks numbered).
    fn cart() -> Cartridge {
        let mut i = info(5, 32, 64);
        let prg: Vec<u8> = (0..64).flat_map(|b| std::iter::repeat_n(b as u8, K8)).collect();
        let chr: Vec<u8> = (0..256).flat_map(|b| std::iter::repeat_n(b as u8, 0x400)).collect();
        i.prg_rom = crate::cart::RomBytes(std::sync::Arc::from(prg));
        i.chr_rom = crate::cart::RomBytes(std::sync::Arc::from(chr));
        i.prg_ram_size = 64 * 1024;
        Cartridge::new(&i).unwrap()
    }

    fn prg(c: &Cartridge) -> [u8; 4] {
        [0x8000, 0xA000, 0xC000, 0xE000].map(|a| c.cpu_peek(a).unwrap())
    }

    #[test]
    fn mmc5_prg_modes() {
        let mut c = cart();
        assert_eq!(prg(&c)[3], 63, "power-on: last bank at $E000");
        for (r, v) in [(0x5114, 0x85), (0x5115, 0x86), (0x5116, 0x87), (0x5117, 0x09)] {
            c.cpu_write(r, v, 0);
        }
        assert_eq!(prg(&c), [5, 6, 7, 9], "mode 3: 8K banks");
        c.cpu_write(0x5100, 2, 0);
        assert_eq!(prg(&c), [6, 7, 7, 9], "mode 2: 16K+8K+8K");
        c.cpu_write(0x5100, 1, 0);
        assert_eq!(prg(&c), [6, 7, 8, 9], "mode 1: 16K+16K");
        c.cpu_write(0x5100, 0, 0);
        assert_eq!(prg(&c), [8, 9, 10, 11], "mode 0: 32K");
        // RAM at $8000 in mode 3, write-protected until $5102=2/$5103=1.
        c.cpu_write(0x5100, 3, 0);
        c.cpu_write(0x5114, 0x03, 0);
        c.cpu_write(0x8000, 0x42, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(0), "protected");
        c.cpu_write(0x5102, 2, 0);
        c.cpu_write(0x5103, 1, 0);
        c.cpu_write(0x8000, 0x42, 0);
        assert_eq!(c.cpu_peek(0x8000), Some(0x42));
        c.cpu_write(0x5113, 0x03, 0);
        assert_eq!(c.cpu_peek(0x6000), Some(0x42), "same RAM bank 3 at $6000");
    }

    #[test]
    fn mmc5_chr_modes_and_8x16_sets() {
        let mut c = cart();
        c.cpu_write(0x5101, 3, 0);
        for i in 0..8u16 {
            c.cpu_write(0x5120 + i, 10 + i as u8, 0);
        }
        for i in 0..4u16 {
            c.cpu_write(0x5128 + i, 100 + i as u8, 0);
        }
        // Last write was set B: 8x8 CPU access uses it.
        assert_eq!(c.ppu_read(0x0400, 0), 101);
        assert_eq!(c.ppu_read(0x1400, 0), 101);
        c.ppu_fetch(true, true);
        assert_eq!(c.ppu_read(0x1400, 0), 15, "8x16 sprites use set A");
        c.ppu_fetch(false, true);
        assert_eq!(c.ppu_read(0x1400, 0), 101, "8x16 BG uses set B");
        c.cpu_write(0x5101, 1, 0);
        c.cpu_write(0x5130, 1, 0);
        c.cpu_write(0x512B, 2, 0);
        assert_eq!(c.ppu_read(0x0010, 0), (258 * 4 % 256) as u8, "4K BG bank 258 with upper bits");
        c.ppu_fetch(true, true);
        assert_eq!(c.ppu_read(0x1000, 0), 17 * 4, "4K sprite bank 17 at $1000");
        c.cpu_write(0x5101, 0, 0);
        assert_eq!(c.ppu_read(0x1C00, 0), 17 * 8 + 7, "8K sprite bank 17");
    }

    #[test]
    fn mmc5_multiplier() {
        let mut c = cart();
        c.cpu_write(0x5205, 200, 0);
        c.cpu_write(0x5206, 123, 0);
        let p = 200u16 * 123;
        assert_eq!(c.cpu_read(0x5205), Some(p as u8));
        assert_eq!(c.cpu_read(0x5206), Some((p >> 8) as u8));
    }

    #[test]
    fn mmc5_exram_and_fill_nametables_through_ppu() {
        let mut c = cart();
        let mut p = Ppu::new(Region::Ntsc);
        c.cpu_write(0x5104, 0, 0);
        // $2000 = CIRAM A, $2400 = ExRAM, $2800 = fill, $2C00 = CIRAM B.
        c.cpu_write(0x5105, 0b01_11_10_00, 0);
        c.cpu_write(0x5106, 0x77, 0);
        c.cpu_write(0x5107, 2, 0);
        let mut poke = |p: &mut Ppu, c: &mut Cartridge, a: u16, v: u8| {
            p.write_register(0x2006, (a >> 8) as u8, c);
            p.write_register(0x2006, a as u8, c);
            p.write_register(0x2007, v, c);
        };
        poke(&mut p, &mut c, 0x2405, 0x5A);
        poke(&mut p, &mut c, 0x2005, 0x11);
        poke(&mut p, &mut c, 0x2C05, 0x22);
        let read = |p: &mut Ppu, c: &mut Cartridge, a: u16| {
            p.write_register(0x2006, (a >> 8) as u8, c);
            p.write_register(0x2006, a as u8, c);
            p.read_register(0x2007, c);
            p.read_register(0x2007, c)
        };
        assert_eq!(read(&mut p, &mut c, 0x2405), 0x5A, "ExRAM as nametable");
        assert_eq!(read(&mut p, &mut c, 0x2005), 0x11);
        assert_eq!(read(&mut p, &mut c, 0x2C05), 0x22, "CIRAM B separate from A");
        assert_eq!(read(&mut p, &mut c, 0x2805), 0x77, "fill tile");
        assert_eq!(read(&mut p, &mut c, 0x2BC5), 0xAA, "fill attribute");
        // ExRAM mode 2: CPU RAM, nametable reads give 0.
        c.cpu_write(0x5104, 2, 0);
        c.cpu_write(0x5C05, 0x33, 0);
        assert_eq!(c.cpu_read(0x5C05), Some(0x33));
        assert_eq!(read(&mut p, &mut c, 0x2405), 0, "mode 2 ExRAM not a nametable");
    }

    #[test]
    fn mmc5_extended_attributes_select_bank_and_palette() {
        let mut c = cart();
        c.cpu_write(0x5104, 2, 0);
        c.cpu_write(0x5C00 + 37, 0b11_000101, 0); // palette 3, 4K bank 5
        c.cpu_write(0x5104, 1, 0);
        c.cpu_write(0x5130, 1, 0);
        c.ppu_fetch(false, false);
        assert_eq!(c.nt_read(0x2000 + 37), super::Nt::Ciram(0));
        assert_eq!(c.nt_read(0x23C9), super::Nt::Value(0xFF), "palette 3 in every quadrant");
        let bank_4k = (1 << 6) | 5;
        assert_eq!(c.ppu_read(0x0010, 0), (bank_4k * 4 % 256) as u8, "BG pattern from 4K bank");
        assert_eq!(c.ppu_read(0x1C10, 0), (bank_4k * 4 % 256 + 3) as u8, "same bank in both halves");
        c.ppu_fetch(true, false);
        c.cpu_write(0x5101, 3, 0);
        c.cpu_write(0x5127, 9, 0);
        assert_eq!(c.ppu_read(0x1C00, 0), 9, "sprites use normal banks");
    }

    #[test]
    fn mmc5_irq_fires_on_target_scanline() {
        let mut c = cart();
        let mut p = Ppu::new(Region::Ntsc);
        let mut fb = vec![0u16; 256 * 240];
        c.cpu_write(0x5203, 40, 0);
        c.cpu_write(0x5204, 0x80, 0);
        p.write_register(0x2001, 0x18, &mut c);
        let mut first = None;
        let mut frames = 0;
        for n in 0..(89_342 * 3) {
            p.tick(&mut c, &mut fb);
            if n % 3 == 2 {
                c.cpu_tick();
            }
            if p.take_frame_done() {
                frames += 1;
                c.cpu_read(0x5204); // drop the IRQ of the partial power-on frame
            }
            if frames >= 1 && c.irq() {
                first = Some((p.scanline(), p.dot()));
                break;
            }
        }
        let (line, dot) = first.expect("IRQ fired");
        assert_eq!(line, 40, "IRQ at scanline 40 (got {line}, dot {dot})");
        assert!(dot < 10);
        assert_eq!(c.cpu_read(0x5204).map(|v| v & 0xC0), Some(0xC0), "pending + in-frame");
        assert!(!c.irq(), "reading $5204 acknowledges");
    }

    /// Ticks `n` CPU cycles, returning the number of rising edges of `audio_level`.
    fn rising_edges(c: &mut Cartridge, n: usize) -> usize {
        let mut prev = c.expansion_audio();
        let mut edges = 0;
        for _ in 0..n {
            c.cpu_tick();
            let l = c.expansion_audio();
            edges += (l > prev) as usize;
            prev = l;
        }
        edges
    }

    #[test]
    fn mmc5_pulse_frequency_length_and_status() {
        let mut c = cart();
        assert!(c.has_expansion_audio());
        c.cpu_write(0x5015, 0x01, 0);
        c.cpu_write(0x5000, 0xB0 | 15, 0); // 50% duty, length halted, constant volume 15
        c.cpu_write(0x5002, 0xFF, 0);
        c.cpu_write(0x5003, 0x00, 0); // period $0FF → 1789773 / (16 * 256) ≈ 437 Hz
        assert_eq!(c.cpu_read(0x5015), Some(0x01));
        // One second: 437 periods, each with one rising edge to 15 * PULSE_SCALE.
        let edges = rising_edges(&mut c, 1_789_773);
        assert!((436..=438).contains(&edges), "frequency {edges}");
        assert!(c.expansion_audio() == 0 || c.expansion_audio() == 15 * super::PULSE_SCALE);

        // Length index 1 → 254 clocks at 240 Hz ≈ 1.058 s, with halt cleared.
        c.cpu_write(0x5000, 0x90 | 15, 0);
        c.cpu_write(0x5003, 0x08, 0);
        assert_eq!(c.cpu_read(0x5015), Some(0x01));
        rising_edges(&mut c, 253 * super::FRAME_PERIOD as usize);
        assert_eq!(c.cpu_read(0x5015), Some(0x01), "still running before 254 clocks");
        rising_edges(&mut c, 2 * super::FRAME_PERIOD as usize);
        assert_eq!(c.cpu_read(0x5015), Some(0x00), "length expired");
        assert_eq!(rising_edges(&mut c, 10_000), 0, "silent after length expiry");
        assert_eq!(c.expansion_audio(), 0);

        // Disabled channel ignores length loads; $5015 clears lengths.
        c.cpu_write(0x5007, 0x08, 0);
        assert_eq!(c.cpu_read(0x5015), Some(0x00));
        c.cpu_write(0x5015, 0x03, 0);
        c.cpu_write(0x5007, 0x08, 0);
        c.cpu_write(0x5003, 0x08, 0);
        assert_eq!(c.cpu_read(0x5015), Some(0x03));
        c.cpu_write(0x5015, 0x01, 0);
        assert_eq!(c.cpu_read(0x5015), Some(0x01));
    }

    #[test]
    fn mmc5_pcm_write_mode() {
        let mut c = cart();
        c.cpu_write(0x5011, 0x80, 0);
        assert_eq!(c.expansion_audio(), 0x80 * super::PCM_SCALE);
        c.cpu_write(0x5011, 0x00, 0); // zero is ignored
        assert_eq!(c.expansion_audio(), 0x80 * super::PCM_SCALE);
        c.cpu_write(0x5010, 0x01, 0); // read mode: writes ignored
        c.cpu_write(0x5011, 0x10, 0);
        assert_eq!(c.expansion_audio(), 0x80 * super::PCM_SCALE);
    }
}
