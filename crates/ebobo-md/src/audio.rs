//! Mixes the FM and PSG streams and resamples them to the host rate. Owner: sound slice.
//!
//! Not emulated state: floats are fine here, and nothing in it feeds back into the machine.
//! The FM stream arrives at `MCLK / FM_SAMPLE_MCLK` Hz (about 53.3 kHz), the PSG stream at
//! `MCLK / PSG_SAMPLE_MCLK` Hz (about 223.7 kHz), interleaved in time order.

/// Output side of the sound hardware.
pub struct AudioOut {
    _todo: (),
}

impl AudioOut {
    /// `mclk`: the console's master clock in Hz (`MCLK_NTSC` or `MCLK_PAL`).
    pub fn new(_mclk: u32) -> AudioOut {
        todo!("owner: sound slice")
    }

    /// Host sample rate in Hz. The frontend nudges it by fractions of a percent every frame
    /// to keep its queue level, so a change must not click.
    pub fn set_sample_rate(&mut self, _hz: f64) {
        todo!("owner: sound slice")
    }

    /// Muted: incoming samples are dropped at once (fast-forward and rollback re-simulation).
    pub fn set_muted(&mut self, _muted: bool) {
        todo!("owner: sound slice")
    }

    /// One FM sample, as [`crate::ym2612::Ym2612::tick`] returned it.
    pub fn fm(&mut self, _left: i16, _right: i16) {
        todo!("owner: sound slice")
    }

    /// One PSG sample, as [`crate::psg::Psg::tick`] returned it.
    pub fn psg(&mut self, _sample: i16) {
        todo!("owner: sound slice")
    }

    /// Appends the samples produced since the last drain: interleaved stereo (left, right),
    /// range about -1.0..1.0.
    pub fn drain(&mut self, _out: &mut Vec<f32>) {
        todo!("owner: sound slice")
    }

    /// Drops buffered output.
    pub fn clear(&mut self) {
        todo!("owner: sound slice")
    }
}
