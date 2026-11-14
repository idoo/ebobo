//! Lines 1, 2, 3 and 7: MOVE and MOVEA (byte, long, word) and MOVEQ.

use crate::core::{Fault, Res, Size, reg9};
use crate::{Bus, Cpu};

pub(crate) fn exec(cpu: &mut Cpu, _bus: &mut impl Bus, op: u16) -> Res<()> {
    match op >> 12 {
        // Bit 8 of a line 7 opcode is always 0.
        7 if op & 0x0100 == 0 => moveq(cpu, op),
        _ => Err(Fault::Illegal),
    }
}

/// MOVEQ #d8,Dn: the byte of the opcode, sign-extended, fills the register; N and Z from it, V and C cleared.
/// 4 cycles: the opcode fetch, which `step` already made.
fn moveq(cpu: &mut Cpu, op: u16) -> Res<()> {
    let val = Size::Byte.sext(u32::from(op));
    cpu.set_d(reg9(op), Size::Long, val);
    cpu.nz(val, Size::Long);
    Ok(())
}
