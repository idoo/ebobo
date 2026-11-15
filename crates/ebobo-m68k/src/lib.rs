//! ebobo-m68k: the Motorola 68000 of the Mega Drive (the plain 68000, not the 68010 or later).
//!
//! Contract (the Mega Drive system and the single-step tests are written against it; change
//! it only together with them):
//! - [`Cpu::step`] runs one instruction, or takes one pending trace exception or interrupt,
//!   and returns its length in CPU clock cycles as on the real chip, prefetch included (the
//!   `length` of the SingleStepTests cases). It ends on an instruction boundary: an
//!   instruction that began with T set leaves [`Cpu::trace_pending`] set, and the trace
//!   exception is the whole of the next step (34 cycles), ahead of any interrupt. The
//!   exceptions of an instruction itself (illegal, privilege, address error, TRAP, CHK,
//!   divide by zero) are part of that instruction's step.
//! - All memory traffic goes through [`Bus`]. Addresses are 24 bits wide. Word accesses are
//!   always even: a word or long access at an odd address raises the address error exception
//!   and never reaches the bus. A long is two word accesses, high word first.
//! - `pc` is the address of the next instruction. The two-word prefetch queue is not
//!   modelled: an opcode is read when it runs.
//! - Deterministic plain data: no clocks, no floats, no randomness.
//!
//! Written from the Motorola manuals and published hardware research, not from another
//! emulator's code.
//!
//! Layout: `core.rs` is the framework (operands, memory, flags, exceptions; its header lists
//! the helpers); one file per group of opcode lines holds the instructions; this file holds
//! the state, reset, and the step loop with interrupts and exception entry.

use serde::{Deserialize, Serialize};

use crate::core::{Fault, Res, Size};

mod arith;
mod core;
mod flow;
mod line0;
mod line4;
mod logic;
mod moves;
mod shift;
mod sys;

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
    /// The last instruction began with T set and ran to its end: the trace exception
    /// (vector 9) is taken by the next [`Cpu::step`] as a step of its own, before a pending
    /// interrupt and before any fetch. Not set by an instruction that was refused or
    /// aborted (illegal, privilege violation, line A/F, address error); set after one that
    /// ran into its own trap (TRAP, TRAPV, CHK, divide by zero), as the manual has it.
    pub trace_pending: bool,
    /// Level on the interrupt pins, 0-7 (0 = no request). Level sensitive: the system keeps
    /// it up to date; the CPU takes it before an instruction when it is above the mask in
    /// `sr` (level 7 always, once per change to 7).
    pub irq: u8,
    /// Mega Drive quirk: TAS does not write its result back (the console's bus has no
    /// read-modify-write cycle). Flags are set as usual.
    pub tas_no_write: bool,
    /// Clock cycles of the step in progress (of the last step, between steps). Bus accesses
    /// add their 4 by themselves; see `core.rs`.
    pub(crate) cyc: u32,
    /// Address of the opcode of the instruction in progress: what the illegal instruction
    /// and privilege violation frames hold.
    pub(crate) op_pc: u32,
    /// `irq` as the previous step saw it: level 7 is taken on the change to 7.
    pub(crate) irq_prev: u8,
}

/// Cycles of bus traffic inside [`Cpu::exception`]: three words pushed, two read.
const EXCEPTION_BUS_CYCLES: u32 = 20;

/// A long of the vector table, read straight from the bus (the address is even).
fn vector_at(bus: &mut impl Bus, addr: u32) -> u32 {
    (u32::from(bus.read16(addr)) << 16) | u32::from(bus.read16(addr + 2))
}

impl Cpu {
    /// Supervisor mode, interrupts masked, everything else zero. Call [`Cpu::reset`] next.
    pub fn new() -> Cpu {
        Cpu { sr: 0x2700, ..Cpu::default() }
    }

    /// Reset exception: supervisor mode, trace off, mask 7, SSP from vector 0 (address 0)
    /// and PC from vector 1 (address 4). Clears `stopped`, `halted` and `trace_pending`.
    pub fn reset(&mut self, bus: &mut impl Bus) {
        self.sr = 0x2700;
        self.ssp = vector_at(bus, 0);
        self.pc = vector_at(bus, 4);
        self.stopped = false;
        self.halted = false;
        self.trace_pending = false;
        self.cyc = 0;
    }

    /// One instruction, or one pending trace exception, or one interrupt; returns its length
    /// in CPU clock cycles. Order: a pending trace exception first (34 cycles, it also ends
    /// a STOP), then an interrupt above the mask (44 cycles), then the instruction at `pc`.
    /// An instruction that began with T set and was not refused or aborted sets
    /// `trace_pending` for the next call. While stopped or halted it returns 4 without
    /// touching the bus.
    pub fn step(&mut self, bus: &mut impl Bus) -> u32 {
        if self.halted {
            return 4;
        }
        self.cyc = 0;

        if self.trace_pending {
            self.trace_pending = false;
            self.stopped = false;
            if let Err(fault) = self.trace(bus) {
                self.raise(bus, fault, 0, true);
            }
            return self.cyc;
        }

        let level = self.irq & 7;
        let became_7 = level == 7 && self.irq_prev != 7;
        self.irq_prev = level;
        if became_7 || u16::from(level) > (self.sr & sr::INT_MASK) >> 8 {
            self.stopped = false;
            if let Err(fault) = self.interrupt(bus, level) {
                self.raise(bus, fault, 0, true);
            }
            return self.cyc;
        }
        if self.stopped {
            return 4;
        }

        self.op_pc = self.pc;
        // The trace exception follows an instruction that began with T set, whatever it did to T.
        let trace = self.sr & sr::T != 0;
        let mut op = 0;
        let run = match self.fetch16(bus) {
            Ok(word) => {
                op = word;
                self.exec(bus, word)
            }
            Err(fault) => Err(fault),
        };
        match run {
            Ok(()) => self.trace_pending = trace,
            Err(fault) => self.raise(bus, fault, op, false),
        }
        self.cyc
    }

    /// Sends an opcode to the file of its line (its top four bits).
    fn exec(&mut self, bus: &mut impl Bus, op: u16) -> Res<()> {
        match op >> 12 {
            0x0 => line0::exec(self, bus, op),
            0x1 | 0x2 | 0x3 | 0x7 => moves::exec(self, bus, op),
            0x4 => line4::exec(self, bus, op),
            0x5 | 0x6 => flow::exec(self, bus, op),
            0x8 | 0xC => logic::exec(self, bus, op),
            0x9 | 0xB | 0xD => arith::exec(self, bus, op),
            0xE => shift::exec(self, bus, op),
            0xA => Err(Fault::LineA),
            _ => Err(Fault::LineF),
        }
    }

    /// Interrupt acceptance: S on, T off, the mask raised to `level`, the acknowledge cycle,
    /// PC and the old SR pushed, PC from the vector the bus named. 44 cycles.
    fn interrupt(&mut self, bus: &mut impl Bus, level: u8) -> Res<()> {
        let old = self.sr;
        self.sr = (old & !(sr::T | sr::INT_MASK)) | sr::S | (u16::from(level) << 8);
        let vector = bus.int_ack(level);
        self.push32(bus, self.pc)?;
        self.push16(bus, old)?;
        self.pc = self.read(bus, u32::from(vector) * 4, Size::Long)?;
        self.cyc = 44;
        Ok(())
    }

    /// The pending trace exception, a step of its own: vector 9, 34 cycles.
    fn trace(&mut self, bus: &mut impl Bus) -> Res<()> {
        self.exception(bus, 9)?;
        self.idle(34 - EXCEPTION_BUS_CYCLES);
        Ok(())
    }

    /// Illegal instruction exception: vector 4, 34 cycles for the whole step; the PC of the
    /// frame is the address of the illegal opcode (`op_pc`).
    pub(crate) fn illegal(&mut self, bus: &mut impl Bus) -> Res<()> {
        self.refuse(bus, 4)
    }

    /// The exception of an instruction that was not run (illegal 4, privilege violation 8,
    /// line A 10, line F 11): the frame holds the address of its opcode; 34 cycles for the
    /// whole step.
    fn refuse(&mut self, bus: &mut impl Bus, vector: u8) -> Res<()> {
        self.pc = self.op_pc;
        self.exception(bus, vector)?;
        self.cyc = 34;
        Ok(())
    }

    /// Turns the fault of an instruction into its exception. `op` is the instruction register
    /// (0 when no opcode was fetched); `in_exception`: the fault happened during exception
    /// processing, not in an instruction. A second fault on the way halts the CPU.
    fn raise(&mut self, bus: &mut impl Bus, fault: Fault, op: u16, in_exception: bool) {
        // A fault of the instruction itself, or of the stack or vector of the exception it asked for.
        let (fault, in_exception) = match fault {
            Fault::Address { .. } => (fault, in_exception),
            _ => {
                let refused = match fault {
                    Fault::Privilege => self.refuse(bus, 8),
                    Fault::LineA => self.refuse(bus, 10),
                    Fault::LineF => self.refuse(bus, 11),
                    _ => self.illegal(bus),
                };
                let Err(second) = refused else { return };
                (second, true)
            }
        };
        let taken = match fault {
            Fault::Address { addr, write, program } => self.address_error(bus, addr, write, program, op, in_exception),
            _ => Err(fault),
        };
        if taken.is_err() {
            self.halted = true;
            self.cyc = self.cyc.max(4);
        }
    }

    /// Address error exception, vector 3, with the 14-byte frame of the manual, from the top
    /// of the stack: status word (bit 4 read, bit 3 not an instruction, bits 2-0 function
    /// code: 1 data or 2 program, plus 4 in supervisor mode; the bits above are those of the
    /// instruction register), access address, instruction register, SR, PC.
    ///
    /// The stacked PC is `pc - 2` (the chip's program counter runs one prefetched word
    /// behind ours); an instruction whose frame differs moves `pc` before it returns the
    /// fault. The step costs 50 cycles plus what the instruction spent before the fault,
    /// its opcode fetch not counted.
    fn address_error(&mut self, bus: &mut impl Bus, addr: u32, write: bool, program: bool, op: u16, in_exception: bool) -> Res<()> {
        let old = self.sr;
        self.sr = (old | sr::S) & !sr::T;
        let function_code = if old & sr::S != 0 { 4 } else { 0 } | if program { 2 } else { 1 };
        // A fetch from an odd address (the target of a jump, branch or return) shows as "not an instruction" too.
        let status = (op & 0xFFE0) | (u16::from(!write) << 4) | (u16::from(in_exception || program) << 3) | function_code;
        let spent = self.cyc;
        self.push32(bus, self.pc.wrapping_sub(2))?;
        self.push16(bus, old)?;
        self.push16(bus, op)?;
        self.push32(bus, addr)?;
        self.push16(bus, status)?;
        self.pc = self.read(bus, 12, Size::Long)?;
        // A fault on the opcode fetch itself has spent nothing yet and still costs 50.
        self.cyc = spent.max(4) + 46;
        Ok(())
    }
}
