//! Sunsoft FME-7 / 5A / 5B (mapper 69): command/parameter ports $8000/$A000, 8x1 KiB CHR,
//! 3x8 KiB PRG + fixed last bank, $6000 ROM/RAM bank (command 8), mirroring (command C),
//! 16-bit CPU-cycle IRQ counter (commands D-F), Sunsoft 5B audio ($C000 address / $E000 data:
//! the three AY-3-8910 square channels with logarithmic volume; noise and envelope unused by
//! the released games and not emulated).

use super::{BoardLogic, Mem, K1, K8};
use crate::cart::{Mirroring, RomInfo};

/// Mapper numbers routed to this file by `Cartridge::new`.
pub(crate) const MAPPERS: &[u16] = &[69];

/// 5B channel level per 4-bit volume: 3 dB per step, full volume ≈ 0.15 of APU full scale.
const VOL: [i32; 16] =
    [0, 39, 55, 78, 110, 155, 219, 309, 437, 617, 871, 1231, 1739, 2456, 3469, 4900];

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Tone {
    period: u16,
    counter: u16,
    high: bool,
}

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Fme7 {
    cmd: u8,
    chr: [u8; 8],
    /// $8000/$A000/$C000 8 KiB banks.
    prg: [u8; 3],
    /// Command 8: bits 0-5 bank, bit 6 RAM (else ROM), bit 7 RAM enable.
    low: u8,
    mirror: u8,
    irq_enable: bool,
    counter_enable: bool,
    counter: u16,
    irq: bool,
    // Sunsoft 5B.
    audio_addr: u8,
    /// Register 7: bits 0-2 tone disable (1 = off, output held high).
    mixer: u8,
    vol: [u8; 3],
    tones: [Tone; 3],
    prescale: u8,
    level: i32,
}

impl Fme7 {
    pub fn new(_info: &RomInfo) -> Option<Self> {
        Some(Fme7 {
            cmd: 0,
            chr: [0; 8],
            prg: [0; 3],
            low: 0,
            mirror: 0,
            irq_enable: false,
            counter_enable: false,
            counter: 0,
            irq: false,
            audio_addr: 0,
            mixer: 0xFF,
            vol: [0; 3],
            tones: Default::default(),
            prescale: 0,
            level: 0,
        })
    }

    fn mix(&mut self) {
        let mut sum = 0;
        for (i, t) in self.tones.iter().enumerate() {
            let off = self.mixer >> i & 1 != 0;
            if off || t.high {
                sum += VOL[(self.vol[i] & 0x0F) as usize];
            }
        }
        self.level = sum;
    }

    fn audio_write(&mut self, val: u8) {
        let r = self.audio_addr;
        match r {
            0..=5 => {
                let t = &mut self.tones[(r >> 1) as usize];
                t.period = if r & 1 == 0 {
                    t.period & 0xF00 | val as u16
                } else {
                    t.period & 0x0FF | ((val as u16 & 0x0F) << 8)
                };
            }
            7 => self.mixer = val,
            8..=10 => self.vol[(r - 8) as usize] = val & 0x1F,
            _ => {}
        }
        self.mix();
    }
}

impl BoardLogic for Fme7 {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        let slot = a / K8;
        let bank = if slot == 3 { mem.prg_8k() - 1 } else { self.prg[slot] as usize & 0x3F };
        bank * K8 + (a & (K8 - 1))
    }

    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        self.chr[a / K1] as usize * K1 + (a & (K1 - 1))
    }

    fn write(&mut self, addr: u16, val: u8, _: u64, _: &Mem) {
        match addr & 0xE000 {
            0x8000 => self.cmd = val & 0x0F,
            0xA000 => match self.cmd {
                0..=7 => self.chr[self.cmd as usize] = val,
                8 => self.low = val,
                9..=11 => self.prg[(self.cmd - 9) as usize] = val,
                12 => self.mirror = val & 3,
                13 => {
                    self.irq_enable = val & 1 != 0;
                    self.counter_enable = val & 0x80 != 0;
                    self.irq = false;
                }
                14 => self.counter = self.counter & 0xFF00 | val as u16,
                _ => self.counter = self.counter & 0x00FF | (val as u16) << 8,
            },
            0xC000 => self.audio_addr = val & 0x0F,
            _ => self.audio_write(val),
        }
    }

    fn prg_low_rom(&self, a: usize, _: &Mem) -> Option<usize> {
        (self.low & 0x40 == 0).then(|| (self.low & 0x3F) as usize * K8 + (a & (K8 - 1)))
    }

    fn prg_ram_bank(&self, _: &Mem) -> Option<usize> {
        // RAM selected and enabled; larger-than-8 KiB RAM is banked by the low bits.
        (self.low & 0xC0 == 0xC0).then_some((self.low & 0x3F) as usize)
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
        self.irq
    }

    fn cpu_tick(&mut self) {
        if self.counter_enable {
            let (c, wrapped) = self.counter.overflowing_sub(1);
            self.counter = c;
            if wrapped && self.irq_enable {
                self.irq = true;
            }
        }
        self.prescale += 1;
        if self.prescale == 16 {
            self.prescale = 0;
            let mut changed = false;
            for t in &mut self.tones {
                t.counter += 1;
                if t.counter >= t.period.max(1) {
                    t.counter = 0;
                    t.high = !t.high;
                    changed = true;
                }
            }
            if changed {
                self.mix();
            }
        }
    }

    fn has_audio(&self) -> bool {
        true
    }

    fn audio_level(&self) -> i32 {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use crate::cart::Mirroring;
    use crate::mapper::test_util::info;
    use crate::mapper::Cartridge;

    fn cmd(c: &mut Cartridge, cmd: u8, val: u8) {
        c.cpu_write(0x8000, cmd, 0);
        c.cpu_write(0xA000, val, 0);
    }

    #[test]
    fn fme7_prg_chr_banking_and_mirroring() {
        // 16 x 16 KiB PRG (32 8 KiB banks), 32 x 4 KiB CHR (128 1 KiB banks).
        let mut c = Cartridge::new(&info(69, 16, 32)).unwrap();
        cmd(&mut c, 9, 5); // 8 KiB bank 5 = 16 KiB bank 2
        cmd(&mut c, 10, 6);
        cmd(&mut c, 11, 9);
        assert_eq!(c.cpu_peek(0x8000), Some(2));
        assert_eq!(c.cpu_peek(0xA000), Some(3));
        assert_eq!(c.cpu_peek(0xC000), Some(4));
        assert_eq!(c.cpu_peek(0xE000), Some(15));
        for i in 0..8 {
            cmd(&mut c, i, 4 * (i + 1)); // 1 KiB bank 4n = 4 KiB bank n
        }
        for i in 0..8u16 {
            assert_eq!(c.ppu_read(i * 0x400, 0), i as u8 + 1);
        }
        for (v, m) in [
            (0, Mirroring::Vertical),
            (1, Mirroring::Horizontal),
            (2, Mirroring::SingleLower),
            (3, Mirroring::SingleUpper),
        ] {
            cmd(&mut c, 12, v);
            assert_eq!(c.mirroring(), m);
        }
    }

    #[test]
    fn fme7_low_bank_rom_ram_select() {
        let mut c = Cartridge::new(&info(69, 16, 2)).unwrap();
        cmd(&mut c, 8, 0x07); // ROM bank 7 = 16 KiB bank 3
        assert_eq!(c.cpu_peek(0x6000), Some(3));
        c.cpu_write(0x6000, 0x55, 0);
        assert_eq!(c.cpu_peek(0x6000), Some(3));
        cmd(&mut c, 8, 0x40); // RAM selected but disabled: open bus
        assert_eq!(c.cpu_peek(0x6000), None);
        cmd(&mut c, 8, 0xC0);
        c.cpu_write(0x6123, 0x55, 0);
        assert_eq!(c.cpu_peek(0x6123), Some(0x55));
    }

    #[test]
    fn fme7_irq_fires_on_wrap() {
        let mut c = Cartridge::new(&info(69, 2, 2)).unwrap();
        cmd(&mut c, 14, 3);
        cmd(&mut c, 15, 0);
        cmd(&mut c, 13, 0x81);
        for _ in 0..3 {
            c.cpu_tick();
        }
        assert!(!c.irq(), "counter at 0, not yet wrapped");
        c.cpu_tick();
        assert!(c.irq());
        cmd(&mut c, 13, 0x81); // ack
        assert!(!c.irq());
        // Counter keeps running from $FFFF; IRQ disabled = no IRQ.
        cmd(&mut c, 13, 0x80);
        for _ in 0..0x10000 {
            c.cpu_tick();
        }
        assert!(!c.irq());
    }

    #[test]
    fn fme7_5b_tone_period() {
        let mut c = Cartridge::new(&info(69, 2, 2)).unwrap();
        assert!(c.has_expansion_audio());
        let aw = |c: &mut Cartridge, r: u8, v: u8| {
            c.cpu_write(0xC000, r, 0);
            c.cpu_write(0xE000, v, 0);
        };
        aw(&mut c, 0, 10); // period 10 → toggle every 160 cycles
        aw(&mut c, 1, 0);
        aw(&mut c, 7, 0xFE); // tone A on
        aw(&mut c, 8, 15);
        let mut last = c.expansion_audio();
        let mut edges = Vec::new();
        for cyc in 0..2000 {
            c.cpu_tick();
            let l = c.expansion_audio();
            if l != last {
                edges.push(cyc);
                last = l;
            }
        }
        assert!(edges.len() >= 10);
        assert!(edges.windows(2).all(|w| w[1] - w[0] == 160), "{edges:?}");
        assert!(c.expansion_audio() == 0 || c.expansion_audio() == 4900);
    }
}
