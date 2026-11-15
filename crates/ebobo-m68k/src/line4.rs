//! Line 4 without its 0x4E00-0x4EFF block: NEGX, CLR, NEG, NOT, TST, TAS, NBCD, EXT, SWAP, PEA, LEA, CHK, MOVEM,
//! MOVE from SR, MOVE to CCR, MOVE to SR and ILLEGAL. The 0x4Exx block (TRAP, LINK, UNLK, MOVE USP, RESET, NOP,
//! STOP, RTE, RTS, TRAPV, RTR, JSR, JMP) is `sys.rs`.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu, sys};

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    if op & 0xFF00 == 0x4E00 {
        return sys::exec(cpu, bus, op);
    }
    Err(Fault::Illegal)
}
