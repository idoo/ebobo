//! SN76489 PSG as built into the Mega Drive's VDP (16-bit noise register). Owner: sound slice.
//!
//! Contract with `system.rs`: [`Psg::tick`] once per `PSG_SAMPLE_MCLK` master clocks, always
//! (emulated state). Integer arithmetic only.

use serde::{Deserialize, Serialize};

/// Complete emulated state of the chip.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Psg {
    _todo: (),
}

impl Psg {
    /// Power-on state: all four channels silent.
    pub fn new() -> Psg {
        todo!("owner: sound slice")
    }

    /// A byte written to the chip (latch or data).
    pub fn write(&mut self, _val: u8) {
        todo!("owner: sound slice")
    }

    /// One sample period: advances the tone and noise counters and returns the mix of the
    /// four channels, within ±32767.
    pub fn tick(&mut self) -> i16 {
        todo!("owner: sound slice")
    }
}
