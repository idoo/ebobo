//! Lines 9, B and D: SUB, SUBA and SUBX; CMP, CMPA, CMPM and EOR; ADD, ADDA and ADDX.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu};

pub(crate) fn exec(_cpu: &mut Cpu, _bus: &mut impl Bus, _op: u16) -> Res<()> {
    Err(Fault::Illegal)
}
