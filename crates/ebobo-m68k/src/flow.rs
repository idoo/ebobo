//! Lines 5 and 6: ADDQ, SUBQ, Scc and DBcc; Bcc, BRA and BSR.

use crate::core::{Fault, Res};
use crate::{Bus, Cpu};

pub(crate) fn exec(_cpu: &mut Cpu, _bus: &mut impl Bus, _op: u16) -> Res<()> {
    Err(Fault::Illegal)
}
