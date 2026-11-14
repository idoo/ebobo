//! ebobo-m68k: the Motorola 68000 of the Mega Drive (the plain 68000, not the 68010 or later).
//!
//! Contract (the Mega Drive system and the single-step tests are written against it; change
//! it only together with them):
//! - [`Cpu::step`] runs one instruction, or takes one pending interrupt or exception, and
//!   returns its length in CPU clock cycles as on the real chip, prefetch included (the
//!   `length` of the SingleStepTests cases).
//! - All memory traffic goes through [`Bus`]. Addresses are 24 bits wide. Word accesses are
//!   always even: a word or long access at an odd address raises the address error exception
//!   and never reaches the bus. A long is two word accesses, high word first.
//! - `pc` is the address of the next instruction. The two-word prefetch queue is not
//!   modelled: an opcode is read when it runs.
//! - Deterministic plain data: no clocks, no floats, no randomness.
//!
//! Written from the Motorola manuals and published hardware research, not from another
//! emulator's code.

use serde::{Deserialize, Serialize};

/// The 68000's view of the machine.
pub trait Bus {
    fn read8(&mut self, addr: u32) -> u8;
    /// `addr` is even.
    fn read16(&mut self, addr: u32) -> u16;
    fn write8(&mut self, addr: u32, val: u8);
    /// `addr` is even.
    fn write16(&mut self, addr: u32, val: u16);
    /// Interrupt acknowledge cycle for `level` (1-7): returns the vector number. The default
    /// is the autovector, which is what the Mega Drive uses; the system clears the device's
    /// request here.
    fn int_ack(&mut self, level: u8) -> u8 {
        24 + level
    }
    /// The RESET instruction pulsed the reset line of the external devices.
    fn reset_devices(&mut self) {}
}

/// Status register bits.
pub mod sr {
    pub const C: u16 = 0x0001;
    pub const V: u16 = 0x0002;
    pub const Z: u16 = 0x0004;
    pub const N: u16 = 0x0008;
    pub const X: u16 = 0x0010;
    /// Interrupt priority mask, bits 8-10.
    pub const INT_MASK: u16 = 0x0700;
    pub const S: u16 = 0x2000;
    pub const T: u16 = 0x8000;
}

/// Complete CPU state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cpu {
    pub d: [u32; 8],
    /// A0-A6. A7 is `ssp` in supervisor mode and `usp` in user mode.
    pub a: [u32; 7],
    pub usp: u32,
    pub ssp: u32,
    /// Address of the next instruction.
    pub pc: u32,
    pub sr: u16,
    /// STOP ran: waiting for an interrupt.
    pub stopped: bool,
    /// Double fault (an address error while taking an address error or the reset): halted
    /// until [`Cpu::reset`].
    pub halted: bool,
    /// Level on the interrupt pins, 0-7 (0 = no request). Level sensitive: the system keeps
    /// it up to date; the CPU takes it before an instruction when it is above the mask in
    /// `sr` (level 7 always, once per change to 7).
    pub irq: u8,
    /// Mega Drive quirk: TAS does not write its result back (the console's bus has no
    /// read-modify-write cycle). Flags are set as usual.
    pub tas_no_write: bool,
}

impl Cpu {
    /// Supervisor mode, interrupts masked, everything else zero. Call [`Cpu::reset`] next.
    pub fn new() -> Cpu {
        Cpu { sr: 0x2700, ..Cpu::default() }
    }

    /// Reset exception: supervisor mode, trace off, mask 7, SSP from vector 0 (address 0)
    /// and PC from vector 1 (address 4). Clears `stopped` and `halted`.
    pub fn reset(&mut self, _bus: &mut impl Bus) {
        todo!("owner: m68k slice")
    }

    /// One instruction, interrupt or exception; returns its length in CPU clock cycles.
    /// While stopped or halted it returns 4 without touching the bus.
    pub fn step(&mut self, _bus: &mut impl Bus) -> u32 {
        todo!("owner: m68k slice")
    }
}
