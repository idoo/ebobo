//! Line 4: LEA, PEA, CLR, NEG, NEGX, NOT, TST, TAS, NBCD, EXT, SWAP, MOVEM, CHK, JMP, JSR, RTS, RTE, RTR, TRAP, TRAPV,
//! LINK, UNLK, MOVE to and from SR, CCR and USP, STOP, RESET, NOP and ILLEGAL.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu};

pub(crate) fn exec(_cpu: &mut Cpu, _bus: &mut impl Bus, op: u16) -> Res<()> {
    match op {
        // NOP: 4 cycles, all of them the opcode fetch `step` already made.
        0x4E71 => Ok(()),
        _ => Err(Fault::Illegal),
    }
}
