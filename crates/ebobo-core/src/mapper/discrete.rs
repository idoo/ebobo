//! Discrete-logic boards: UxROM (2), CNROM (3), AxROM (7), Color Dreams (11), GxROM (66),
//! Camerica/Codemasters (71). One latch register each, no IRQ.

use super::{BoardLogic, Mem, K16, K32, K8};
use crate::cart::Mirroring;

/// UxROM: switchable 16 KiB at $8000, last bank fixed at $C000.
#[inline]
fn ux_prg(prg: u8, a: usize, mem: &Mem) -> usize {
    if a < K16 { mem.prg_off(prg as usize, K16, a) } else { mem.prg_off(mem.prg_16k() - 1, K16, a - K16) }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Uxrom {
    prg: u8,
}

impl BoardLogic for Uxrom {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        ux_prg(self.prg, a, mem)
    }
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        a
    }
    fn write(&mut self, _: u16, val: u8, _: u64, _: &Mem) {
        self.prg = val;
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Cnrom {
    chr: u8,
}

impl BoardLogic for Cnrom {
    fn prg_index(&self, a: usize, _: &Mem) -> usize {
        a
    }
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        self.chr as usize * K8 + a
    }
    fn write(&mut self, _: u16, val: u8, _: u64, _: &Mem) {
        self.chr = val;
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Axrom {
    reg: u8,
}

impl BoardLogic for Axrom {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        mem.prg_off((self.reg & 0x0F) as usize, K32, a)
    }
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        a
    }
    fn write(&mut self, _: u16, val: u8, _: u64, _: &Mem) {
        self.reg = val;
    }
    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        Some(if self.reg & 0x10 != 0 { Mirroring::SingleUpper } else { Mirroring::SingleLower })
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ColorDreams {
    reg: u8,
}

impl BoardLogic for ColorDreams {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        mem.prg_off((self.reg & 3) as usize, K32, a)
    }
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        (self.reg >> 4) as usize * K8 + a
    }
    fn write(&mut self, _: u16, val: u8, _: u64, _: &Mem) {
        self.reg = val;
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Gxrom {
    reg: u8,
}

impl BoardLogic for Gxrom {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        mem.prg_off(((self.reg >> 4) & 3) as usize, K32, a)
    }
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        (self.reg & 3) as usize * K8 + a
    }
    fn write(&mut self, _: u16, val: u8, _: u64, _: &Mem) {
        self.reg = val;
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Camerica {
    prg: u8,
    mirroring: Option<Mirroring>,
}

impl BoardLogic for Camerica {
    fn prg_index(&self, a: usize, mem: &Mem) -> usize {
        ux_prg(self.prg, a, mem)
    }
    fn chr_index(&self, a: usize, _: &Mem) -> usize {
        a
    }
    fn write(&mut self, addr: u16, val: u8, _: u64, mem: &Mem) {
        if addr >= 0xC000 {
            self.prg = val;
        } else if addr < 0xA000 && mem.submapper == 1 {
            // Fire Hawk (submapper 1): single-screen select via bit 4.
            self.mirroring = Some(if val & 0x10 != 0 {
                Mirroring::SingleUpper
            } else {
                Mirroring::SingleLower
            });
        }
    }
    fn mirroring(&self, _: &Mem) -> Option<Mirroring> {
        self.mirroring
    }
}
