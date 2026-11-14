//! YM2612 (OPN2) FM sound chip. Owner: sound slice.
//!
//! Contract with `system.rs`: the system calls [`Ym2612::tick`] once per output sample period
//! (`FM_SAMPLE_MCLK` master clocks), always, also during fast-forward: timers, envelopes and
//! phases are emulated state. Before a register access it ticks the chip up to the current
//! time. Integer arithmetic only.

use serde::{Deserialize, Serialize};

/// Complete emulated state of the chip.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Ym2612 {
    _todo: (),
}

impl Ym2612 {
    /// Power-on state.
    pub fn new() -> Ym2612 {
        todo!("owner: sound slice")
    }

    /// The reset line (the 68000's Z80-reset register pulls it too): all channels silent,
    /// registers cleared.
    pub fn reset(&mut self) {
        todo!("owner: sound slice")
    }

    /// `port` 0-3 (0x4000-0x4003): 0 = address of part I, 1 = data, 2 = address of part II,
    /// 3 = data.
    pub fn write(&mut self, _port: u8, _val: u8) {
        todo!("owner: sound slice")
    }

    /// Status (the same on all four ports): bit 7 busy, bit 1 timer B overflowed, bit 0
    /// timer A overflowed.
    pub fn read(&mut self) -> u8 {
        todo!("owner: sound slice")
    }

    /// One output sample period: advances timers, LFO, envelopes and phases and returns the
    /// mix of the six channels as (left, right), panning applied, each within ±32767.
    pub fn tick(&mut self) -> (i16, i16) {
        todo!("owner: sound slice")
    }
}
