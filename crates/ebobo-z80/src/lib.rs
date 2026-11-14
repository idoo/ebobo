//! ebobo-z80: the Zilog Z80 (the Mega Drive's sound CPU).
//!
//! Contract (the Mega Drive system and the single-step tests are written against it; change
//! it only together with them):
//! - [`Cpu::step`] runs one instruction (prefixes included), or accepts one interrupt, and
//!   returns its length in T-states as on the real chip.
//! - Documented and undocumented instructions and flags (bits 3 and 5 of F, `wz`, `q`)
//!   behave as the SingleStepTests cases say.
//! - The field names match those cases, so a test loads and compares state directly.
//! - Deterministic plain data: no clocks, no floats, no randomness.
//!
//! Written from Zilog's manual and published hardware research, not from another
//! emulator's code.

use serde::{Deserialize, Serialize};

/// The Z80's view of the machine.
pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, val: u8);
    /// IN: the full 16-bit port address. The Mega Drive has no ports (reads give 0xFF).
    fn port_in(&mut self, _port: u16) -> u8 {
        0xFF
    }
    fn port_out(&mut self, _port: u16, _val: u8) {}
    /// Byte on the data bus during interrupt acknowledge (used by modes 0 and 2).
    fn int_ack(&mut self) -> u8 {
        0xFF
    }
}

/// Flag bits of F.
pub mod flag {
    pub const C: u8 = 0x01;
    pub const N: u8 = 0x02;
    pub const PV: u8 = 0x04;
    pub const X: u8 = 0x08;
    pub const H: u8 = 0x10;
    pub const Y: u8 = 0x20;
    pub const Z: u8 = 0x40;
    pub const S: u8 = 0x80;
}

/// Complete CPU state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cpu {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    /// Shadow pairs AF', BC', DE', HL'.
    pub af_: u16,
    pub bc_: u16,
    pub de_: u16,
    pub hl_: u16,
    pub ix: u16,
    pub iy: u16,
    pub sp: u16,
    pub pc: u16,
    /// Internal MEMPTR register (shows in undocumented flags of BIT n,(HL) and others).
    pub wz: u16,
    pub i: u8,
    pub r: u8,
    pub iff1: bool,
    pub iff2: bool,
    /// Interrupt mode 0-2.
    pub im: u8,
    /// EI ran last: maskable interrupts stay off until after the next instruction.
    pub ei: bool,
    /// LD A,I / LD A,R ran last (an interrupt accepted right after resets P/V).
    pub p: bool,
    /// F as the last instruction wrote it, or 0 when it did not touch the flags (SCF/CCF
    /// bits 3 and 5 depend on it).
    pub q: u8,
    pub halted: bool,
    /// The /INT line is low. Level sensitive: the system keeps it up to date.
    pub irq: bool,
    /// An /NMI edge is waiting to be taken.
    pub nmi: bool,
}

impl Cpu {
    /// State after power-on followed by [`Cpu::reset`].
    pub fn new() -> Cpu {
        let mut cpu = Cpu { a: 0xFF, f: 0xFF, sp: 0xFFFF, ..Cpu::default() };
        cpu.reset();
        cpu
    }

    /// /RESET: PC, I and R to zero, interrupts off, mode 0, not halted. The other registers
    /// keep their values.
    pub fn reset(&mut self) {
        self.pc = 0;
        self.i = 0;
        self.r = 0;
        self.iff1 = false;
        self.iff2 = false;
        self.im = 0;
        self.ei = false;
        self.p = false;
        self.q = 0;
        self.halted = false;
        self.nmi = false;
    }

    /// One instruction or one interrupt acceptance; returns its length in T-states. While
    /// halted it runs NOPs of 4 T-states (R still counts).
    pub fn step(&mut self, _bus: &mut impl Bus) -> u32 {
        todo!("owner: z80 slice")
    }
}
