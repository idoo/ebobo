//! The ED table: IN r,(C) / OUT (C),r, SBC/ADC HL,rr, LD (nn),rr / LD rr,(nn), NEG, RETN/RETI,
//! IM 0/1/2, LD I,A / LD R,A / LD A,I / LD A,R, RRD / RLD, the block instructions
//! (LDI LDD LDIR LDDR, CPI CPD CPIR CPDR, INI IND INIR INDR, OUTI OUTD OTIR OTDR).
//!
//! PLACEHOLDER: the table is not written yet. Until it is, every ED opcode acts as a NOP of
//! 8 T-states (which is also what the chip does for the undefined ones). What goes here:
//! - Flags through `Cpu::set_f` and the helpers of `alu.rs` (`neg`, `adc16`, `sbc16`; drop
//!   their `#[allow(dead_code)]` once they are called).
//! - LD A,I and LD A,R set `cpu.p = true` (P/V = IFF2; an interrupt taken right after clears
//!   P/V, which `Cpu::step` does from `p`).
//! - A DD / FD prefix in front of ED has no effect: this table always works on HL.

use crate::{Bus, Cpu};

/// Runs one ED-table instruction. On entry `pc` is at the opcode byte after `ED` and R
/// already counts the ED byte; fetches the opcode itself through `Cpu::fetch_m1` (R counts it
/// too). Returns the T-states from the ED byte on (8 for the shortest instructions).
pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus) -> u32 {
    cpu.fetch_m1(bus);
    8
}
