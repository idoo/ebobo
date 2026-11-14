//! Lines 8 and C: OR, DIVU, DIVS and SBCD; AND, MULU, MULS, ABCD and EXG.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu};

pub(crate) fn exec(_cpu: &mut Cpu, _bus: &mut impl Bus, _op: u16) -> Res<()> {
    Err(Fault::Illegal)
}
