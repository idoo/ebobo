//! The 0x4E00-0x4EFF block of line 4: TRAP, LINK, UNLK, MOVE USP, RESET, NOP, STOP, RTE, RTS, TRAPV, RTR, JSR, JMP.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu};

pub(crate) fn exec(_cpu: &mut Cpu, _bus: &mut impl Bus, op: u16) -> Res<()> {
    match op {
        // NOP: 4 cycles, all of them the opcode fetch `step` already made.
        0x4E71 => Ok(()),
        _ => Err(Fault::Illegal),
    }
}
