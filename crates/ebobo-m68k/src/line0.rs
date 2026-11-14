//! Line 0: ORI, ANDI, SUBI, ADDI, EORI, CMPI (also to CCR and SR), BTST, BCHG, BCLR, BSET and MOVEP.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu};

pub(crate) fn exec(_cpu: &mut Cpu, _bus: &mut impl Bus, _op: u16) -> Res<()> {
    Err(Fault::Illegal)
}
