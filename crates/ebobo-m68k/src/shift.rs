//! Line E: ASL, ASR, LSL, LSR, ROL, ROR, ROXL and ROXR, on a data register or on a word in memory.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu};

pub(crate) fn exec(_cpu: &mut Cpu, _bus: &mut impl Bus, _op: u16) -> Res<()> {
    Err(Fault::Illegal)
}
