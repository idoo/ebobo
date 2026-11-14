//! The CB table: rotates and shifts (RLC RRC RL RR SLA SRA SLL SRL), BIT, RES, SET.
//!
//! PLACEHOLDER: the table is not written yet. Until it is, every CB opcode acts as a NOP of
//! 8 T-states. What goes here:
//! - `addr == None` (plain `CB op`): the operand is the register numbered by bits 0-2 of the
//!   opcode (`Cpu::reg` / `Cpu::set_reg` with `Idx::Hl`), or (HL) for 6. 8 T for registers,
//!   15 T for (HL), 12 T for BIT n,(HL).
//! - `addr == Some(a)` (`DD CB d op` / `FD CB d op`): the operand is always the byte at `a`;
//!   every form except BIT also copies the result into the register of bits 0-2 (not for 6).
//!   19 T, 16 T for BIT (the caller adds the 4 T of the DD / FD prefix). BIT takes X/Y from
//!   the high byte of `a`; BIT n,(HL) takes them from the high byte of `wz`.
//! - Flags through `Cpu::set_f` and the helpers of `alu.rs`.

use crate::{Bus, Cpu};

/// Runs one CB-table instruction. On entry `pc` is at the opcode byte (after `CB`, or after
/// `DD CB d`), R already counts the CB byte, and for `Some(addr)` `wz` already equals `addr`.
/// Fetches the opcode itself: through `Cpu::fetch_m1` for `None` (R counts it), through
/// `Cpu::fetch` for `Some` (R does not). Returns the T-states from the CB byte on.
pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, addr: Option<u16>) -> u32 {
    match addr {
        None => cpu.fetch_m1(bus),
        Some(_) => cpu.fetch(bus),
    };
    8
}
