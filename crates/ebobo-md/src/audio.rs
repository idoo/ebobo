//! Mixes the FM and PSG streams and resamples them to the host rate. Owner: sound slice.
//!
//! Not emulated state: floats are fine here, and nothing in it feeds back into the machine.
//! The FM stream arrives at `MCLK / FM_SAMPLE_MCLK` Hz (about 53.3 kHz), the PSG stream at
//! `MCLK / PSG_SAMPLE_MCLK` Hz (about 223.7 kHz), interleaved in time order.
//!
//! Each stream goes through a one-pole low-pass at its own input rate (the console's output
//! stage, which also takes the edges off the PSG's square waves), then a box filter that averages
//! the input over each output period (zero-order hold integrated exactly, fractional overlaps
//! included), so nothing above the host rate folds back as a loud alias. The two streams are
//! mixed per output frame, a slow DC blocker removes the PSG's unipolar offset, and a soft limit
//! keeps the result inside -1.0..=1.0.

use crate::{FM_SAMPLE_MCLK, PSG_SAMPLE_MCLK};

/// Gain of the PSG stream relative to the FM stream. One PSG channel at full volume swings
/// 8191 (unipolar, ±4096 once the DC is gone), one FM channel at full volume ±5355: at 0.5 a PSG
/// channel sits about 8 dB under an FM channel, near what a Model 1 console's mixer gives.
const PSG_TO_FM: f32 = 0.5;
/// Corner of the output low-pass, Hz: the first-order RC filter of the Model 1 output stage.
const LOWPASS_HZ: f64 = 3390.0;
/// Corner of the DC blocker, Hz.
const DC_HZ: f64 = 5.0;
/// Frames one stream may run ahead of the other before the missing one counts as silence.
const SLACK: usize = 64;
/// Frames buffered without a drain before everything buffered is dropped.
const MAX_FRAMES: usize = 1 << 16;
/// Filter states below this are snapped to zero (no denormals once the sound stops).
const TINY: f32 = 1e-20;
/// The soft limit is linear up to this level and bends smoothly towards 1.0 above it.
const KNEE: f32 = 0.75;

/// Low-pass, then box-average resampling of one stream with `C` channels.
struct Resampler<const C: usize> {
    /// Input rate, Hz.
    rate: f64,
    /// Output periods covered by one input sample (host rate / input rate).
    step: f64,
    /// Part of the current output period already accumulated, 0..1.
    pos: f64,
    /// Weighted sum of the current output period so far.
    acc: [f32; C],
    /// Low-pass state.
    lp: [f32; C],
    /// Low-pass coefficient at the input rate.
    lp_k: f32,
    /// Finished output frames.
    out: Vec<[f32; C]>,
}

impl<const C: usize> Resampler<C> {
    fn new(rate: f64, host: f64) -> Self {
        Resampler {
            rate,
            step: host / rate,
            pos: 0.0,
            acc: [0.0; C],
            lp: [0.0; C],
            lp_k: (1.0 - (-2.0 * std::f64::consts::PI * LOWPASS_HZ / rate).exp()) as f32,
            out: Vec::with_capacity(4096),
        }
    }

    fn set_host_rate(&mut self, host: f64) {
        self.step = host / self.rate;
    }

    fn push(&mut self, x: [f32; C]) {
        for (lp, x) in self.lp.iter_mut().zip(x) {
            *lp += self.lp_k * (x - *lp);
            if lp.abs() < TINY {
                *lp = 0.0;
            }
        }
        let mut left = self.step;
        while self.pos + left >= 1.0 {
            let take = (1.0 - self.pos) as f32;
            let mut frame = self.acc;
            for (f, lp) in frame.iter_mut().zip(self.lp) {
                *f += lp * take;
            }
            self.out.push(frame);
            self.acc = [0.0; C];
            left -= 1.0 - self.pos;
            self.pos = 0.0;
        }
        let take = left as f32;
        for (acc, lp) in self.acc.iter_mut().zip(self.lp) {
            *acc += lp * take;
        }
        self.pos += left;
    }

    /// Forgets the output period in progress.
    fn reset_partial(&mut self) {
        self.acc = [0.0; C];
        self.pos = 0.0;
    }
}

/// Output side of the sound hardware.
pub struct AudioOut {
    fm: Resampler<2>,
    psg: Resampler<1>,
    /// DC blocker: last input and output per channel.
    dc_in: [f32; 2],
    dc_out: [f32; 2],
    /// DC blocker pole at the host rate.
    dc_r: f32,
    muted: bool,
}

impl AudioOut {
    /// `mclk`: the console's master clock in Hz (`MCLK_NTSC` or `MCLK_PAL`).
    pub fn new(mclk: u32) -> AudioOut {
        let host = 48_000.0;
        let mclk = f64::from(mclk);
        AudioOut {
            fm: Resampler::new(mclk / FM_SAMPLE_MCLK as f64, host),
            psg: Resampler::new(mclk / PSG_SAMPLE_MCLK as f64, host),
            dc_in: [0.0; 2],
            dc_out: [0.0; 2],
            dc_r: dc_pole(host),
            muted: false,
        }
    }

    /// Host sample rate in Hz. The frontend nudges it by fractions of a percent every frame
    /// to keep its queue level, so a change must not click.
    ///
    /// Only the step changes; the phase of the output period in progress carries over.
    /// Values that are not finite and positive are ignored.
    pub fn set_sample_rate(&mut self, hz: f64) {
        if !hz.is_finite() || hz <= 0.0 {
            return;
        }
        self.fm.set_host_rate(hz);
        self.psg.set_host_rate(hz);
        self.dc_r = dc_pole(hz);
    }

    /// Muted: incoming samples are dropped at once (fast-forward and rollback re-simulation).
    pub fn set_muted(&mut self, muted: bool) {
        if muted == self.muted {
            return;
        }
        self.muted = muted;
        self.fm.reset_partial();
        self.psg.reset_partial();
        let common = self.fm.out.len().min(self.psg.out.len());
        self.fm.out.truncate(common);
        self.psg.out.truncate(common);
    }

    /// One FM sample, as [`crate::ym2612::Ym2612::tick`] returned it.
    pub fn fm(&mut self, left: i16, right: i16) {
        if self.muted {
            return;
        }
        self.fm.push([f32::from(left), f32::from(right)]);
        self.cap();
    }

    /// One PSG sample, as [`crate::psg::Psg::tick`] returned it.
    pub fn psg(&mut self, sample: i16) {
        if self.muted {
            return;
        }
        self.psg.push([f32::from(sample) * PSG_TO_FM]);
        self.cap();
    }

    /// Appends the samples produced since the last drain: interleaved stereo (left, right),
    /// range -1.0..=1.0.
    ///
    /// Frames both streams have reached are mixed; a stream more than [`SLACK`] frames behind
    /// the other counts as silent for the difference (a machine without PSG writes still drains).
    pub fn drain(&mut self, out: &mut Vec<f32>) {
        let (nf, np) = (self.fm.out.len(), self.psg.out.len());
        let (lo, hi) = (nf.min(np), nf.max(np));
        let n = if hi - lo > SLACK { hi - SLACK } else { lo };
        out.reserve(2 * n);
        for i in 0..n {
            let fm = self.fm.out.get(i).copied().unwrap_or([0.0; 2]);
            let psg = self.psg.out.get(i).map_or(0.0, |p| p[0]);
            for (ch, fm) in fm.into_iter().enumerate() {
                let x = (fm + psg) / 32768.0;
                let mut y = x - self.dc_in[ch] + self.dc_r * self.dc_out[ch];
                if y.abs() < TINY {
                    y = 0.0;
                }
                self.dc_in[ch] = x;
                self.dc_out[ch] = y;
                out.push(soft_limit(y));
            }
        }
        self.fm.out.drain(..n.min(nf));
        self.psg.out.drain(..n.min(np));
    }

    /// Drops buffered output.
    pub fn clear(&mut self) {
        self.fm.out.clear();
        self.psg.out.clear();
        self.fm.reset_partial();
        self.psg.reset_partial();
    }

    /// Drops everything buffered once nobody has drained for [`MAX_FRAMES`] frames.
    fn cap(&mut self) {
        if self.fm.out.len() >= MAX_FRAMES || self.psg.out.len() >= MAX_FRAMES {
            self.fm.out.clear();
            self.psg.out.clear();
        }
    }
}

fn dc_pole(host: f64) -> f32 {
    (1.0 - 2.0 * std::f64::consts::PI * DC_HZ / host) as f32
}

/// Linear up to [`KNEE`], then a tanh curve that approaches ±1.0 and never passes it.
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        return x;
    }
    let room = 1.0 - KNEE;
    (KNEE + room * ((a - KNEE) / room).tanh()).min(1.0).copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MCLK: u32 = 53_693_175;
    /// Greatest common divisor of the two sample periods, in master clocks.
    const GRAIN: u64 = 48;

    /// Feeds `seconds` of input, interleaved by master clock; `fm(t)` and `psg(t)` give the
    /// sample at time `t` in seconds.
    fn feed(audio: &mut AudioOut, start: f64, seconds: f64, fm: impl Fn(f64) -> i16, psg: impl Fn(f64) -> i16) {
        let begin = (start * f64::from(MCLK)) as u64 / GRAIN * GRAIN;
        let end = ((start + seconds) * f64::from(MCLK)) as u64;
        let mut t = begin;
        while t < end {
            let secs = t as f64 / f64::from(MCLK);
            if t % PSG_SAMPLE_MCLK == 0 {
                audio.psg(psg(secs));
            }
            if t % FM_SAMPLE_MCLK == 0 {
                let s = fm(secs);
                audio.fm(s, s);
            }
            t += GRAIN;
        }
    }

    fn square(t: f64, hz: f64, low: i16, high: i16) -> i16 {
        if (t * hz).fract() < 0.5 { high } else { low }
    }

    #[test]
    fn one_second_is_48000_frames() {
        let mut audio = AudioOut::new(MCLK);
        feed(&mut audio, 0.0, 1.0, |_| 1000, |_| 1000);
        let mut out = Vec::new();
        audio.drain(&mut out);
        assert_eq!(out.len() % 2, 0);
        let frames = out.len() / 2;
        assert!(frames.abs_diff(48_000) <= 2, "{frames} frames");
    }

    #[test]
    fn rate_change_does_not_click() {
        let mut audio = AudioOut::new(MCLK);
        let sine = |t: f64| ((t * 440.0 * std::f64::consts::TAU).sin() * 16000.0) as i16;
        let mut first = Vec::new();
        feed(&mut audio, 0.0, 0.5, sine, |_| 0);
        audio.drain(&mut first);
        audio.set_sample_rate(48_000.0 * 0.995);
        let mut second = Vec::new();
        feed(&mut audio, 0.5, 0.5, sine, |_| 0);
        audio.drain(&mut second);
        let max_step = |s: &[f32]| s.chunks(2).zip(s.chunks(2).skip(1)).map(|(a, b)| (b[0] - a[0]).abs()).fold(0.0f32, f32::max);
        let own = max_step(&first);
        let mut all = first.clone();
        all.extend_from_slice(&second);
        assert!(own > 0.0);
        assert!(max_step(&all) <= own * 1.05, "{} vs {own}", max_step(&all));
    }

    #[test]
    fn full_scale_stays_inside() {
        let mut audio = AudioOut::new(MCLK);
        let mut out = Vec::new();
        feed(&mut audio, 0.0, 0.5, |t| square(t, 110.0, -32767, 32767), |t| square(t, 1000.0, 0, 32764));
        audio.drain(&mut out);
        assert!(!out.is_empty());
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        assert!(out.iter().any(|s| s.abs() > 0.9), "the limit bends, it does not shrink everything");
    }

    #[test]
    fn muted_input_produces_nothing() {
        let mut audio = AudioOut::new(MCLK);
        audio.set_muted(true);
        feed(&mut audio, 0.0, 0.1, |_| 5000, |_| 5000);
        let mut out = Vec::new();
        audio.drain(&mut out);
        assert!(out.is_empty());
        audio.set_muted(false);
        feed(&mut audio, 0.1, 0.1, |_| 5000, |_| 5000);
        audio.drain(&mut out);
        assert!(out.len() / 2 > 4700);
    }

    #[test]
    fn silence_in_silence_out() {
        let mut audio = AudioOut::new(MCLK);
        feed(&mut audio, 0.0, 0.2, |_| 0, |_| 0);
        let mut out = Vec::new();
        audio.drain(&mut out);
        assert!(!out.is_empty());
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn psg_is_quieter_than_fm() {
        let rms = |fm: bool| {
            let mut audio = AudioOut::new(MCLK);
            if fm {
                feed(&mut audio, 0.0, 0.5, |t| square(t, 440.0, -4096, 4095), |_| 0);
            } else {
                feed(&mut audio, 0.0, 0.5, |_| 0, |t| square(t, 440.0, 0, 8191));
            }
            let mut out = Vec::new();
            audio.drain(&mut out);
            let tail = &out[out.len() / 2..];
            (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let (fm, psg) = (rms(true), rms(false));
        assert!(psg < fm * 0.7, "psg {psg}, fm {fm}");
        assert!(psg > fm * 0.2, "psg {psg}, fm {fm}");
    }

    #[test]
    fn dc_is_removed() {
        let mut audio = AudioOut::new(MCLK);
        feed(&mut audio, 0.0, 1.0, |_| 0, |_| 32764);
        let mut out = Vec::new();
        audio.drain(&mut out);
        assert!(out[200] > 0.3, "the step itself comes through");
        assert!(out[out.len() - 1].abs() < 0.01);
    }

    #[test]
    fn missing_stream_counts_as_silence_and_clear_empties() {
        let mut audio = AudioOut::new(MCLK);
        for _ in 0..53_267 / 10 {
            audio.fm(1000, 1000);
        }
        let mut out = Vec::new();
        audio.drain(&mut out);
        assert!(out.len() / 2 >= 4800 - SLACK - 2);
        feed(&mut audio, 0.0, 0.05, |_| 1000, |_| 1000);
        audio.clear();
        let len = out.len();
        audio.drain(&mut out);
        assert_eq!(out.len(), len);
    }
}
