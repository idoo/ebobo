//! SN76489 PSG as built into the Mega Drive's VDP (16-bit noise register). Owner: sound slice.
//!
//! Contract with `system.rs`: [`Psg::tick`] once per `PSG_SAMPLE_MCLK` master clocks, always
//! (emulated state). Integer arithmetic only.
//!
//! Output scale: a channel at attenuation 0 contributes 8191 while its output is high and 0
//! while it is low (the chip's outputs are unipolar), so the four channels together stay within
//! 0..=32764. The constant offset this leaves is removed by [`crate::audio::AudioOut`].

use serde::{Deserialize, Serialize};

/// Level of a channel per attenuator value: 2 dB a step (8191 * 10^(-n / 10)), 15 is off.
const VOLUME: [i16; 16] = [8191, 6506, 5168, 4105, 3261, 2590, 2057, 1634, 1298, 1031, 819, 651, 517, 411, 326, 0];

/// Bit 2 of the noise register: white noise (set) or periodic noise (clear).
const NOISE_WHITE: u8 = 0x04;
/// Value the noise shift register is loaded with by every write to the noise register.
const LFSR_RESET: u16 = 0x8000;

/// Complete emulated state of the chip.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Psg {
    /// Tone periods, 10 bits: ticks between two flips of the channel's output.
    period: [u16; 3],
    /// Tone down-counters, in ticks.
    counter: [u16; 3],
    /// Tone output flip-flops.
    flip: [bool; 3],
    /// Noise register: bit 2 white/periodic, bits 1-0 the shift rate.
    noise: u8,
    /// Down-counter of the noise channel for the three fixed rates, in ticks.
    noise_counter: u16,
    /// Clock input of the shift register: it shifts when this goes from low to high.
    noise_flip: bool,
    /// Noise shift register, 16 bits; bit 0 is the channel's output.
    lfsr: u16,
    /// Attenuators of the three tone channels and the noise channel, 0 (loud) to 15 (off).
    atten: [u8; 4],
    /// Register picked by the last latch byte: channel * 2, plus 1 for the attenuator.
    latch: u8,
}

impl Default for Psg {
    fn default() -> Psg {
        Psg::new()
    }
}

impl Psg {
    /// Power-on state: all four channels silent.
    pub fn new() -> Psg {
        Psg {
            period: [0; 3],
            counter: [0; 3],
            flip: [false; 3],
            noise: 0,
            noise_counter: 0,
            noise_flip: false,
            lfsr: LFSR_RESET,
            atten: [15; 4],
            latch: 0,
        }
    }

    /// A byte written to the chip (latch or data).
    ///
    /// A latch byte (bit 7 set) picks a register with bits 6-4 (channel, then tone/noise or
    /// attenuator) and writes its low four bits. A data byte writes the register picked last:
    /// the high six bits of a tone period, or the attenuator or noise register again.
    pub fn write(&mut self, val: u8) {
        let is_latch = val & 0x80 != 0;
        if is_latch {
            self.latch = (val >> 4) & 7;
        }
        let channel = usize::from(self.latch >> 1);
        if self.latch & 1 != 0 {
            self.atten[channel] = val & 0x0F;
        } else if channel == 3 {
            self.noise = val & 0x07;
            self.lfsr = LFSR_RESET;
        } else if is_latch {
            self.period[channel] = (self.period[channel] & 0x3F0) | u16::from(val & 0x0F);
        } else {
            self.period[channel] = (self.period[channel] & 0x00F) | (u16::from(val & 0x3F) << 4);
        }
    }

    /// One sample period: advances the tone and noise counters and returns the mix of the
    /// four channels, within ±32767 (0..=32764, see the module comment).
    ///
    /// Each counter counts down once and, on reaching zero, reloads and flips its output. A tone
    /// period of 0 or 1 gives a constant high output (games play samples through it).
    pub fn tick(&mut self) -> i16 {
        let mut mix = 0;
        for ch in 0..3 {
            self.counter[ch] = self.counter[ch].saturating_sub(1);
            if self.counter[ch] == 0 {
                self.counter[ch] = self.period[ch];
                self.flip[ch] = !self.flip[ch];
            }
            if self.period[ch] <= 1 || self.flip[ch] {
                mix += VOLUME[usize::from(self.atten[ch])];
            }
        }

        let rate = self.noise & 3;
        let clock = if rate == 3 {
            // Clocked by the output of tone channel 3.
            self.flip[2]
        } else {
            self.noise_counter = self.noise_counter.saturating_sub(1);
            if self.noise_counter == 0 {
                self.noise_counter = 0x10 << rate;
                !self.noise_flip
            } else {
                self.noise_flip
            }
        };
        if clock && !self.noise_flip {
            self.shift_noise();
        }
        self.noise_flip = clock;
        if self.lfsr & 1 != 0 {
            mix += VOLUME[usize::from(self.atten[3])];
        }
        mix
    }

    /// One step of the noise shift register: shifts right, feeding bit 15 with bits 0 and 3
    /// exclusive-ored (white noise) or with bit 0 alone (periodic noise).
    fn shift_noise(&mut self) {
        let feedback = if self.noise & NOISE_WHITE != 0 { (self.lfsr ^ (self.lfsr >> 3)) & 1 } else { self.lfsr & 1 };
        self.lfsr = (self.lfsr >> 1) | (feedback << 15);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{DefaultHasher, Hash, Hasher};

    /// Ticks in one second of an NTSC console: 53 693 175 / 240, rounded up.
    const TICKS_PER_SECOND: u32 = 223_722;

    fn hash(psg: &Psg) -> u64 {
        let mut hasher = DefaultHasher::new();
        psg.hash(&mut hasher);
        hasher.finish()
    }

    /// Sets a tone channel's period with a latch and a data byte.
    fn set_period(psg: &mut Psg, channel: u8, period: u16) {
        psg.write(0x80 | (channel << 5) | (period & 0x0F) as u8);
        psg.write((period >> 4) as u8 & 0x3F);
    }

    #[test]
    fn tone_is_a_square_wave_of_the_period() {
        for (channel, period) in [(0u8, 254u16), (1, 1), (1, 2), (2, 1023), (0, 17)] {
            let mut psg = Psg::new();
            set_period(&mut psg, channel, period);
            psg.write(0x90 | (channel << 5));
            let mut last = psg.tick();
            let mut edges = 0u32;
            for _ in 1..TICKS_PER_SECOND {
                let now = psg.tick();
                assert!(now == 0 || now == 8191, "level {now}");
                edges += u32::from(now != last);
                last = now;
            }
            if period == 1 {
                assert_eq!(edges, 0, "period 1 is a constant level");
                assert_eq!(last, 8191);
            } else {
                // 440.4 Hz for period 254: 3 579 545 Hz / (32 * 254), two edges a cycle.
                let expected = TICKS_PER_SECOND / u32::from(period);
                assert!(edges.abs_diff(expected) <= 1, "period {period}: {edges} edges, expected {expected}");
            }
        }
    }

    #[test]
    fn period_zero_is_a_constant_high_level() {
        let mut psg = Psg::new();
        psg.write(0x90);
        for _ in 0..100 {
            assert_eq!(psg.tick(), 8191);
        }
    }

    #[test]
    fn attenuation_steps_down_to_silence() {
        let mut psg = Psg::new();
        let mut levels = [0i16; 16];
        for (atten, level) in levels.iter_mut().enumerate() {
            psg.write(0x90 | atten as u8);
            *level = psg.tick();
        }
        assert_eq!(levels[0], 8191);
        assert_eq!(levels[15], 0);
        for pair in levels.windows(2) {
            assert!(pair[1] < pair[0], "{levels:?}");
        }
        // 2 dB a step: each level is 0.794 of the one before.
        for pair in levels[..15].windows(2) {
            let ratio = i32::from(pair[1]) * 1000 / i32::from(pair[0]);
            assert!((790..=798).contains(&ratio), "{} / {}", pair[1], pair[0]);
        }
        assert!(i32::from(VOLUME[0]) * 4 <= 32767);
    }

    #[test]
    fn all_channels_at_full_volume_stay_in_range() {
        let mut psg = Psg::new();
        for channel in 0..4u8 {
            psg.write(0x90 | (channel << 5));
        }
        psg.write(0xE4);
        let mut max = 0;
        for _ in 0..20_000 {
            let sample = psg.tick();
            assert!((0..=32764).contains(&sample));
            max = max.max(sample);
        }
        assert_eq!(max, 32764);
    }

    #[test]
    fn white_noise_register_sequence() {
        // 0x8000 shifted right; bit 15 is fed with bit 0 xor bit 3 of the value before:
        // the single bit travels down for twelve steps, reaches bit 3 (0x0008 -> 0x8004),
        // then bit 0 (0x2001 -> 0x9000).
        const EXPECTED: [u16; 20] = [
            0x4000, 0x2000, 0x1000, 0x0800, 0x0400, 0x0200, 0x0100, 0x0080, 0x0040, 0x0020, 0x0010, 0x0008, 0x8004, 0x4002, 0x2001,
            0x9000, 0x4800, 0x2400, 0x1200, 0x0900,
        ];
        let mut psg = Psg::new();
        psg.write(0xE4);
        assert_eq!(psg.lfsr, 0x8000);
        psg.write(0xF0);
        let mut seen = Vec::new();
        let mut ticks_at_change = Vec::new();
        for tick in 0..32 * 20 {
            let before = psg.lfsr;
            let sample = psg.tick();
            assert_eq!(sample, if psg.lfsr & 1 != 0 { 8191 } else { 0 }, "the output is bit 0");
            if psg.lfsr != before {
                seen.push(psg.lfsr);
                ticks_at_change.push(tick);
            }
        }
        assert_eq!(seen, EXPECTED);
        // Rate 0: the counter flips every 16 ticks and the register shifts on every other flip.
        for pair in ticks_at_change.windows(2) {
            assert_eq!(pair[1] - pair[0], 32);
        }
        // Any write to the noise register, latch or data byte, starts over.
        psg.write(0xE4);
        assert_eq!(psg.lfsr, 0x8000);
        psg.tick_until_shift();
        psg.write(0xE5);
        psg.write(0x05);
        assert_eq!(psg.lfsr, 0x8000);
    }

    #[test]
    fn periodic_noise_cycles_one_bit_through_sixteen_steps() {
        let mut psg = Psg::new();
        psg.write(0xE0);
        for step in 1..=16u32 {
            psg.shift_noise();
            assert_eq!(psg.lfsr, 0x8000u16.rotate_right(step));
        }
        assert_eq!(psg.lfsr, 0x8000);
    }

    #[test]
    fn noise_rates() {
        // Fixed rates: a shift every 32, 64 and 128 ticks; rate 3: every two flips of tone 3.
        for (rate, expected) in [(0u8, 32u32), (1, 64), (2, 128), (3, 2 * 37)] {
            let mut psg = Psg::new();
            set_period(&mut psg, 2, 37);
            psg.write(0xE4 | rate);
            psg.tick_until_shift();
            assert_eq!(psg.tick_until_shift(), expected, "rate {rate}");
            assert_eq!(psg.tick_until_shift(), expected, "rate {rate}");
        }
    }

    #[test]
    fn latch_and_data_bytes() {
        let mut psg = Psg::new();
        psg.write(0x8E);
        assert_eq!(psg.period[0], 0x00E);
        psg.write(0x0F);
        assert_eq!(psg.period[0], 0x0FE);
        // A second data byte goes to the same register; bit 6 is ignored.
        psg.write(0x7F);
        assert_eq!(psg.period[0], 0x3FE);
        // A latch byte keeps the high six bits.
        psg.write(0x81);
        assert_eq!(psg.period[0], 0x3F1);
        psg.write(0xC3);
        psg.write(0x21);
        assert_eq!(psg.period, [0x3F1, 0, 0x213]);

        psg.write(0xB5);
        assert_eq!(psg.atten, [15, 5, 15, 15]);
        psg.write(0x0A);
        assert_eq!(psg.atten, [15, 10, 15, 15]);
        psg.write(0xF3);
        assert_eq!(psg.atten, [15, 10, 15, 3]);
        assert_eq!(psg.period, [0x3F1, 0, 0x213], "attenuator writes leave the periods alone");

        psg.write(0xE5);
        assert_eq!(psg.noise, 5);
        psg.tick_until_shift();
        assert_ne!(psg.lfsr, 0x8000);
        psg.write(0x02);
        assert_eq!(psg.noise, 2);
        assert_eq!(psg.lfsr, 0x8000);
    }

    #[test]
    fn same_writes_hash_equal() {
        let script = [0x8E, 0x0F, 0x90, 0xA3, 0x12, 0xB4, 0xC7, 0x01, 0xD2, 0xE7, 0xF1];
        let mut a = Psg::new();
        let mut b = Psg::new();
        for &byte in &script {
            a.write(byte);
            b.write(byte);
            for _ in 0..1000 {
                assert_eq!(a.tick(), b.tick());
            }
        }
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(hash(&a.clone()), hash(&b));
        b.write(0x9F);
        assert_ne!(hash(&a), hash(&b));
    }

    impl Psg {
        /// Ticks until the noise register shifts; returns the number of ticks it took.
        fn tick_until_shift(&mut self) -> u32 {
            let before = self.lfsr;
            let mut ticks = 0;
            while self.lfsr == before {
                self.tick();
                ticks += 1;
                assert!(ticks < 100_000, "the noise register never shifts");
            }
            ticks
        }
    }
}
