//! 2A03 APU and the audio output path. Owner: APU slice.

use crate::cart::Region;

const LENGTH_TABLE: [u8; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 14, 12, 26, 14, 12, 16, 24, 18, 48, 20, 96, 22,
    192, 24, 72, 26, 16, 28, 32, 30,
];

/// Duty waveforms as bit masks indexed by sequencer position.
const DUTY: [u8; 4] = [0b0000_0010, 0b0000_0110, 0b0001_1110, 0b1111_1001];

const TRIANGLE: [u8; 32] = [
    15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    13, 14, 15,
];

/// Noise periods in CPU cycles.
const NOISE_NTSC: [u16; 16] = [
    4, 8, 16, 32, 64, 96, 128, 160, 202, 254, 380, 508, 762, 1016, 2034, 4068,
];
const NOISE_PAL: [u16; 16] = [
    4, 8, 14, 30, 60, 88, 118, 148, 188, 236, 354, 472, 708, 944, 1890, 3778,
];

/// DMC rates in CPU cycles.
const DMC_NTSC: [u16; 16] = [
    428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54,
];
const DMC_PAL: [u16; 16] = [
    398, 354, 316, 298, 276, 236, 210, 198, 176, 148, 132, 118, 98, 78, 66, 50,
];

/// Frame sequencer timings in CPU cycles after the counter reset:
/// quarter 1, quarter+half 2, quarter 3, IRQ, quarter+half+IRQ (4-step end),
/// IRQ + wrap (4-step), quarter+half (5-step end), wrap (5-step).
#[derive(Clone, Copy)]
struct FrameTimes {
    q1: u32,
    q2: u32,
    q3: u32,
    irq0: u32,
    end4: u32,
    wrap4: u32,
    end5: u32,
    wrap5: u32,
}

const FRAME_NTSC: FrameTimes = FrameTimes {
    q1: 7457,
    q2: 14913,
    q3: 22371,
    irq0: 29828,
    end4: 29829,
    wrap4: 29830,
    end5: 37281,
    wrap5: 37282,
};
const FRAME_PAL: FrameTimes = FrameTimes {
    q1: 8313,
    q2: 16627,
    q3: 24939,
    irq0: 33252,
    end4: 33253,
    wrap4: 33254,
    end5: 41565,
    wrap5: 41566,
};

/// Length-counter halt and reload writes, latched until the end of the next APU tick
/// (after that cycle's frame-sequencer clock): a halt change takes effect after a length
/// clock in the same cycle, and a reload in that cycle is dropped if the clock changed
/// the counter (blargg len_halt_timing / len_reload_timing).
#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct LenLatch {
    halt: bool,
    new_halt: bool,
    /// Pending reload value (0 = none; the length table has no zero entry).
    reload: u8,
    /// Counter value when the reload was written.
    prev: u8,
}

impl LenLatch {
    fn write_reload(&mut self, enabled: bool, val: u8, length: u8) {
        if enabled {
            self.reload = LENGTH_TABLE[(val >> 3) as usize];
            self.prev = length;
        }
    }

    fn clock(&self, length: &mut u8) {
        if !self.halt && *length > 0 {
            *length -= 1;
        }
    }

    fn apply(&mut self, enabled: bool, length: &mut u8) {
        self.halt = self.new_halt;
        if self.reload != 0 {
            if enabled && *length == self.prev {
                *length = self.reload;
            }
            self.reload = 0;
        }
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Envelope {
    start: bool,
    looped: bool,
    constant: bool,
    volume: u8,
    divider: u8,
    decay: u8,
}

impl Envelope {
    fn write(&mut self, val: u8) {
        self.looped = val & 0x20 != 0;
        self.constant = val & 0x10 != 0;
        self.volume = val & 0x0F;
    }

    fn clock(&mut self) {
        if self.start {
            self.start = false;
            self.decay = 15;
            self.divider = self.volume;
        } else if self.divider == 0 {
            self.divider = self.volume;
            if self.decay > 0 {
                self.decay -= 1;
            } else if self.looped {
                self.decay = 15;
            }
        } else {
            self.divider -= 1;
        }
    }

    #[inline]
    fn output(&self) -> u8 {
        if self.constant { self.volume } else { self.decay }
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Pulse {
    ones_complement: bool,
    enabled: bool,
    duty: u8,
    pos: u8,
    period: u16,
    timer: u16,
    length: u8,
    #[serde(default)]
    len: LenLatch,
    env: Envelope,
    sweep_enabled: bool,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    sweep_reload: bool,
    sweep_divider: u8,
}

impl Pulse {
    fn target(&self) -> u16 {
        let change = self.period >> self.sweep_shift;
        if self.sweep_negate {
            let sub = change + self.ones_complement as u16;
            self.period.saturating_sub(sub)
        } else {
            self.period + change
        }
    }

    #[inline]
    fn muted(&self) -> bool {
        self.period < 8 || self.target() > 0x7FF
    }

    fn write(&mut self, reg: u16, val: u8) {
        match reg & 3 {
            0 => {
                self.duty = val >> 6;
                self.env.write(val);
                self.len.new_halt = val & 0x20 != 0;
            }
            1 => {
                self.sweep_enabled = val & 0x80 != 0;
                self.sweep_period = (val >> 4) & 7;
                self.sweep_negate = val & 0x08 != 0;
                self.sweep_shift = val & 7;
                self.sweep_reload = true;
            }
            2 => self.period = (self.period & 0x700) | val as u16,
            _ => {
                self.period = (self.period & 0xFF) | (((val & 7) as u16) << 8);
                self.len.write_reload(self.enabled, val, self.length);
                self.pos = 0;
                self.env.start = true;
            }
        }
    }

    /// Clocked every APU cycle (2 CPU cycles). Returns true when the sequencer stepped.
    #[inline]
    fn clock_timer(&mut self) -> bool {
        if self.timer == 0 {
            self.timer = self.period;
            self.pos = (self.pos + 1) & 7;
            true
        } else {
            self.timer -= 1;
            false
        }
    }

    fn clock_length(&mut self) {
        self.len.clock(&mut self.length);
    }

    fn clock_sweep(&mut self) {
        if self.sweep_divider == 0 && self.sweep_enabled && self.sweep_shift > 0 && !self.muted()
        {
            self.period = self.target();
        }
        if self.sweep_divider == 0 || self.sweep_reload {
            self.sweep_divider = self.sweep_period;
            self.sweep_reload = false;
        } else {
            self.sweep_divider -= 1;
        }
    }

    #[inline]
    fn output(&self) -> u8 {
        if self.length == 0 || (DUTY[self.duty as usize] >> self.pos) & 1 == 0 || self.muted() {
            0
        } else {
            self.env.output()
        }
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Triangle {
    enabled: bool,
    control: bool,
    reload_value: u8,
    linear: u8,
    reload_flag: bool,
    period: u16,
    timer: u16,
    step: u8,
    length: u8,
    #[serde(default)]
    len: LenLatch,
}

impl Triangle {
    fn write(&mut self, reg: u16, val: u8) {
        match reg & 3 {
            0 => {
                self.control = val & 0x80 != 0;
                self.len.new_halt = self.control;
                self.reload_value = val & 0x7F;
            }
            1 => {}
            2 => self.period = (self.period & 0x700) | val as u16,
            _ => {
                self.period = (self.period & 0xFF) | (((val & 7) as u16) << 8);
                self.len.write_reload(self.enabled, val, self.length);
                self.reload_flag = true;
            }
        }
    }

    /// Clocked every CPU cycle. Returns true when the sequencer stepped (output changed).
    #[inline]
    fn clock_timer(&mut self) -> bool {
        if self.timer == 0 {
            self.timer = self.period;
            if self.length > 0 && self.linear > 0 {
                self.step = (self.step + 1) & 31;
                return true;
            }
            false
        } else {
            self.timer -= 1;
            false
        }
    }

    fn clock_linear(&mut self) {
        if self.reload_flag {
            self.linear = self.reload_value;
        } else if self.linear > 0 {
            self.linear -= 1;
        }
        if !self.control {
            self.reload_flag = false;
        }
    }

    fn clock_length(&mut self) {
        self.len.clock(&mut self.length);
    }

    #[inline]
    fn output(&self) -> u8 {
        TRIANGLE[self.step as usize]
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Noise {
    enabled: bool,
    mode: bool,
    period_index: u8,
    timer: u16,
    shift: u16,
    length: u8,
    #[serde(default)]
    len: LenLatch,
    env: Envelope,
}

impl Noise {
    fn write(&mut self, reg: u16, val: u8) {
        match reg & 3 {
            0 => {
                self.env.write(val);
                self.len.new_halt = val & 0x20 != 0;
            }
            1 => {}
            2 => {
                self.mode = val & 0x80 != 0;
                self.period_index = val & 0x0F;
            }
            _ => {
                self.len.write_reload(self.enabled, val, self.length);
                self.env.start = true;
            }
        }
    }

    /// Clocked every CPU cycle with the CPU-cycle period table. Returns true on a shift.
    #[inline]
    fn clock_timer(&mut self, table: &[u16; 16]) -> bool {
        if self.timer == 0 {
            self.timer = table[self.period_index as usize] - 1;
            let tap = if self.mode { 6 } else { 1 };
            let fb = (self.shift ^ (self.shift >> tap)) & 1;
            self.shift = (self.shift >> 1) | (fb << 14);
            true
        } else {
            self.timer -= 1;
            false
        }
    }

    fn clock_length(&mut self) {
        self.len.clock(&mut self.length);
    }

    #[inline]
    fn output(&self) -> u8 {
        if self.length == 0 || self.shift & 1 != 0 { 0 } else { self.env.output() }
    }
}

#[derive(Clone, Hash, Default, serde::Serialize, serde::Deserialize)]
struct Dmc {
    irq_enabled: bool,
    looped: bool,
    irq: bool,
    rate_index: u8,
    timer: u16,
    level: u8,
    sample_addr: u16,
    sample_len: u16,
    addr: u16,
    remaining: u16,
    buffer: u8,
    buffer_full: bool,
    shift: u8,
    bits: u8,
    silence: bool,
}

impl Dmc {
    fn write(&mut self, reg: u16, val: u8) {
        match reg & 3 {
            0 => {
                self.irq_enabled = val & 0x80 != 0;
                if !self.irq_enabled {
                    self.irq = false;
                }
                self.looped = val & 0x40 != 0;
                self.rate_index = val & 0x0F;
            }
            1 => self.level = val & 0x7F,
            2 => self.sample_addr = 0xC000 | ((val as u16) << 6),
            _ => self.sample_len = ((val as u16) << 4) | 1,
        }
    }

    fn restart(&mut self) {
        self.addr = self.sample_addr;
        self.remaining = self.sample_len;
    }

    /// Clocked every CPU cycle with the CPU-cycle rate table. Returns true when the output
    /// clock could have changed `level` (not silenced).
    #[inline]
    fn clock_timer(&mut self, table: &[u16; 16]) -> bool {
        if self.timer == 0 {
            self.timer = table[self.rate_index as usize] - 1;
            let audible = !self.silence;
            self.clock_output();
            audible
        } else {
            self.timer -= 1;
            false
        }
    }

    fn clock_output(&mut self) {
        if !self.silence {
            if self.shift & 1 != 0 {
                if self.level <= 125 {
                    self.level += 2;
                }
            } else if self.level >= 2 {
                self.level -= 2;
            }
        }
        self.shift >>= 1;
        if self.bits > 0 {
            self.bits -= 1;
        }
        if self.bits == 0 {
            self.bits = 8;
            if self.buffer_full {
                self.silence = false;
                self.shift = self.buffer;
                self.buffer_full = false;
            } else {
                self.silence = true;
            }
        }
    }

    fn dma_complete(&mut self, val: u8) {
        if self.remaining == 0 {
            return;
        }
        self.buffer = val;
        self.buffer_full = true;
        self.addr = if self.addr == 0xFFFF { 0x8000 } else { self.addr + 1 };
        self.remaining -= 1;
        if self.remaining == 0 {
            if self.looped {
                self.restart();
            } else if self.irq_enabled {
                self.irq = true;
            }
        }
    }
}

/// Channel and frame-counter state, integers only. Part of the rollback snapshot.
#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub struct Apu {
    pal: bool,
    pulse1: Pulse,
    pulse2: Pulse,
    triangle: Triangle,
    noise: Noise,
    dmc: Dmc,
    /// CPU cycle parity: true on odd cycles (between APU cycles).
    odd: bool,
    /// CPU cycles since the frame counter was last reset.
    fc: u32,
    /// Next `fc` value at which the frame sequencer does something.
    fc_next: u32,
    five_step: bool,
    irq_inhibit: bool,
    frame_irq: bool,
    /// Last value written to $4017 (re-applied on reset).
    last_4017: u8,
    /// Delayed $4017 write: CPU cycles left (0 = none) and the value.
    pending_4017: u8,
    pending_val: u8,
    /// Cached mixer inputs (pulse1+pulse2, 3*tri+2*noise+dmc), recomputed only when a
    /// channel's output can have changed (`mix_dirty`). Derived from the fields above.
    mix_pulse: u8,
    mix_tnd: u8,
    mix_dirty: bool,
    /// A channel register write is latched in a `LenLatch`, applied at the end of the
    /// next tick.
    #[serde(default)]
    len_pending: bool,
}

/// Resampler + filters + sample buffer. Outside the snapshot: it only shapes what we hear,
/// never what the game sees.
pub struct AudioOut {
    cpu_rate: f64,
    sample_rate: f64,
    /// Output samples per CPU cycle.
    step: f64,
    phase: f64,
    acc: f32,
    /// Last (pulse, tnd) pair pushed and its mixed level.
    last_key: u16,
    level: f32,
    count: u32,
    pulse_table: [f32; 31],
    tnd_table: [f32; 203],
    hp90_a: f32,
    hp440_a: f32,
    lp14k_b: f32,
    hp90_prev_x: f32,
    hp90_prev_y: f32,
    hp440_prev_x: f32,
    hp440_prev_y: f32,
    lp_prev_y: f32,
    buffer: Vec<f32>,
    /// Fast-forward: drop samples (host setting, not emulated state).
    muted: bool,
    /// Cartridge expansion-audio level for the current cycle (0.0..=1.0 = APU full scale),
    /// set by the bus via `push_ext` before the APU pushes the cycle.
    ext: f32,
}

impl Apu {
    pub fn new(region: Region) -> Apu {
        let pal = matches!(region, Region::Pal);
        let mut apu = Apu {
            pal,
            pulse1: Pulse { ones_complement: true, ..Pulse::default() },
            pulse2: Pulse::default(),
            triangle: Triangle::default(),
            noise: Noise { shift: 1, ..Noise::default() },
            dmc: Dmc {
                sample_addr: 0xC000,
                sample_len: 1,
                bits: 8,
                silence: true,
                ..Dmc::default()
            },
            odd: false,
            fc: 0,
            fc_next: 0,
            five_step: false,
            irq_inhibit: false,
            frame_irq: false,
            last_4017: 0,
            pending_4017: 0,
            pending_val: 0,
            mix_pulse: 0,
            mix_tnd: 0,
            mix_dirty: true,
            len_pending: false,
        };
        apu.fc_next = apu.next_event();
        apu
    }

    /// Reset button.
    pub fn reset(&mut self) {
        self.mix_dirty = true;
        self.write_status(0);
        self.dmc.level &= 1;
        self.frame_irq = false;
        self.pending_val = self.last_4017;
        self.pending_4017 = self.delay_4017();
    }

    /// CPU cycles until a $4017 write (or the reset re-write) takes effect: one more when
    /// the write lands on an even (APU "get") cycle.
    fn delay_4017(&self) -> u8 {
        if self.odd { 4 } else { 5 }
    }

    /// $4015 read: channel length status, DMC active, frame IRQ (cleared by the read),
    /// DMC IRQ. Bit 5 is left 0; the bus fills it from open bus.
    pub fn read_status(&mut self) -> u8 {
        let v = self.peek_status();
        self.frame_irq = false;
        v
    }

    /// Side-effect-free $4015 read.
    pub fn peek_status(&self) -> u8 {
        (self.pulse1.length > 0) as u8
            | ((self.pulse2.length > 0) as u8) << 1
            | ((self.triangle.length > 0) as u8) << 2
            | ((self.noise.length > 0) as u8) << 3
            | ((self.dmc.remaining > 0) as u8) << 4
            | (self.frame_irq as u8) << 6
            | (self.dmc.irq as u8) << 7
    }

    /// Writes to $4000-$4013, $4015 and $4017.
    pub fn write_register(&mut self, addr: u16, val: u8) {
        self.mix_dirty = true;
        match addr {
            0x4000..=0x4003 => self.pulse1.write(addr, val),
            0x4004..=0x4007 => self.pulse2.write(addr, val),
            0x4008..=0x400B => self.triangle.write(addr, val),
            0x400C..=0x400F => self.noise.write(addr, val),
            0x4010..=0x4013 => self.dmc.write(addr, val),
            0x4015 => self.write_status(val),
            0x4017 => {
                self.last_4017 = val;
                if val & 0x40 != 0 {
                    self.frame_irq = false;
                }
                self.pending_val = val;
                self.pending_4017 = self.delay_4017();
            }
            _ => {}
        }
        self.len_pending |= addr < 0x4010;
    }

    #[cold]
    fn apply_len_latches(&mut self) {
        self.len_pending = false;
        self.pulse1.len.apply(self.pulse1.enabled, &mut self.pulse1.length);
        self.pulse2.len.apply(self.pulse2.enabled, &mut self.pulse2.length);
        self.triangle.len.apply(self.triangle.enabled, &mut self.triangle.length);
        self.noise.len.apply(self.noise.enabled, &mut self.noise.length);
    }

    fn write_status(&mut self, val: u8) {
        self.pulse1.enabled = val & 1 != 0;
        self.pulse2.enabled = val & 2 != 0;
        self.triangle.enabled = val & 4 != 0;
        self.noise.enabled = val & 8 != 0;
        if !self.pulse1.enabled {
            self.pulse1.length = 0;
        }
        if !self.pulse2.enabled {
            self.pulse2.length = 0;
        }
        if !self.triangle.enabled {
            self.triangle.length = 0;
        }
        if !self.noise.enabled {
            self.noise.length = 0;
        }
        if val & 0x10 != 0 {
            if self.dmc.remaining == 0 {
                self.dmc.restart();
            }
        } else {
            self.dmc.remaining = 0;
        }
        self.dmc.irq = false;
    }

    fn times(&self) -> &'static FrameTimes {
        if self.pal { &FRAME_PAL } else { &FRAME_NTSC }
    }

    fn next_event(&self) -> u32 {
        let t = self.times();
        let fc = self.fc;
        let list: [u32; 6] = if self.five_step {
            [t.q1, t.q2, t.q3, t.end5, t.wrap5, t.wrap5]
        } else {
            [t.q1, t.q2, t.q3, t.irq0, t.end4, t.wrap4]
        };
        for e in list {
            if e > fc {
                return e;
            }
        }
        u32::MAX
    }

    fn quarter_frame(&mut self) {
        self.pulse1.env.clock();
        self.pulse2.env.clock();
        self.noise.env.clock();
        self.triangle.clock_linear();
    }

    fn half_frame(&mut self) {
        self.pulse1.clock_length();
        self.pulse2.clock_length();
        self.triangle.clock_length();
        self.noise.clock_length();
        self.pulse1.clock_sweep();
        self.pulse2.clock_sweep();
    }

    #[cold]
    fn frame_event(&mut self) {
        self.mix_dirty = true;
        let t = *self.times();
        let fc = self.fc;
        if fc == t.q1 || fc == t.q3 {
            self.quarter_frame();
        } else if fc == t.q2 {
            self.quarter_frame();
            self.half_frame();
        } else if self.five_step {
            if fc == t.end5 {
                self.quarter_frame();
                self.half_frame();
            } else if fc == t.wrap5 {
                self.fc = 0;
            }
        } else {
            if fc == t.end4 {
                self.quarter_frame();
                self.half_frame();
            }
            if !self.irq_inhibit {
                self.frame_irq = true;
            }
            if fc == t.wrap4 {
                self.fc = 0;
            }
        }
        self.fc_next = self.next_event();
    }

    #[cold]
    fn apply_4017(&mut self) {
        self.mix_dirty = true;
        let val = self.pending_val;
        self.five_step = val & 0x80 != 0;
        self.irq_inhibit = val & 0x40 != 0;
        if self.irq_inhibit {
            self.frame_irq = false;
        }
        self.fc = 0;
        self.fc_next = self.next_event();
    }

    /// Advances one CPU cycle and feeds the mixed output level to `out`.
    pub fn tick(&mut self, out: &mut AudioOut) {
        // The mode-1 immediate clock below changes channel state without `mix_dirty`; the
        // cache then refreshes in full on any timer reload of this tick (pre-split rule).
        let mut early_clock = false;
        if self.pending_4017 != 0 {
            self.pending_4017 -= 1;
            if self.pending_4017 == 0 {
                self.apply_4017();
            } else if self.pending_4017 == 1 && self.pending_val & 0x80 != 0 {
                early_clock = true;
                // Mode 1's immediate quarter+half clock lands a cycle before the
                // sequencer restarts.
                self.quarter_frame();
                self.half_frame();
            }
        }
        self.fc += 1;
        if self.fc == self.fc_next {
            self.frame_event();
        }
        if self.len_pending {
            self.apply_len_latches();
        }

        let early_refresh = early_clock
            && ((self.odd && (self.pulse1.timer == 0 || self.pulse2.timer == 0))
                || self.triangle.timer == 0
                || self.noise.timer == 0
                || self.dmc.timer == 0);
        let mut pulse_dirty = false;
        if self.odd {
            pulse_dirty = self.pulse1.clock_timer();
            pulse_dirty |= self.pulse2.clock_timer();
        }
        self.odd = !self.odd;
        let mut tnd_dirty = self.triangle.clock_timer();
        let (noise_t, dmc_t) =
            if self.pal { (&NOISE_PAL, &DMC_PAL) } else { (&NOISE_NTSC, &DMC_NTSC) };
        tnd_dirty |= self.noise.clock_timer(noise_t);
        tnd_dirty |= self.dmc.clock_timer(dmc_t);

        let full = self.mix_dirty | early_refresh;
        if pulse_dirty | full {
            self.mix_pulse = self.pulse1.output() + self.pulse2.output();
        }
        if tnd_dirty | full {
            self.mix_tnd = 3 * self.triangle.output() + 2 * self.noise.output() + self.dmc.level;
        }
        self.mix_dirty = false;
        out.push(self.mix_pulse, self.mix_tnd);
    }

    /// Frame-counter IRQ OR DMC IRQ.
    pub fn irq(&self) -> bool {
        self.frame_irq | self.dmc.irq
    }

    /// Address of a pending DMC sample fetch. The bus reads it (stalling the CPU) and hands
    /// the byte back with `dmc_dma_complete`.
    pub fn dmc_dma_request(&self) -> Option<u16> {
        if !self.dmc.buffer_full && self.dmc.remaining > 0 { Some(self.dmc.addr) } else { None }
    }

    pub fn dmc_dma_complete(&mut self, val: u8) {
        self.dmc.dma_complete(val);
    }
}

impl AudioOut {
    pub fn new(region: Region, sample_rate: f64) -> AudioOut {
        let cpu_rate = if matches!(region, Region::Pal) { 1_662_607.0 } else { 1_789_773.0 };
        let mut pulse_table = [0f32; 31];
        for (i, v) in pulse_table.iter_mut().enumerate().skip(1) {
            *v = (95.52 / (8128.0 / i as f64 + 100.0)) as f32;
        }
        let mut tnd_table = [0f32; 203];
        for (i, v) in tnd_table.iter_mut().enumerate().skip(1) {
            *v = (163.67 / (24329.0 / i as f64 + 100.0)) as f32;
        }
        let mut out = AudioOut {
            cpu_rate,
            sample_rate: 0.0,
            step: 0.0,
            phase: 0.0,
            acc: 0.0,
            last_key: 0,
            level: 0.0,
            count: 0,
            pulse_table,
            tnd_table,
            hp90_a: 0.0,
            hp440_a: 0.0,
            lp14k_b: 0.0,
            hp90_prev_x: 0.0,
            hp90_prev_y: 0.0,
            hp440_prev_x: 0.0,
            hp440_prev_y: 0.0,
            lp_prev_y: 0.0,
            buffer: Vec::with_capacity(4096),
            muted: false,
            ext: 0.0,
        };
        out.set_sample_rate(sample_rate);
        out
    }

    pub fn set_sample_rate(&mut self, hz: f64) {
        if !(hz > 0.0) {
            self.sample_rate = 0.0;
            self.step = 0.0;
            return;
        }
        self.sample_rate = hz;
        self.step = hz / self.cpu_rate;
        let dt = 1.0 / hz;
        let rc = |f: f64| 1.0 / (2.0 * std::f64::consts::PI * f);
        let hp = |f: f64| (rc(f) / (rc(f) + dt)) as f32;
        self.hp90_a = hp(90.0);
        self.hp440_a = hp(440.0);
        self.lp14k_b = (dt / (rc(14_000.0) + dt)) as f32;
    }

    pub fn set_muted(&mut self, on: bool) {
        self.muted = on;
    }

    /// Sets the cartridge expansion-audio level (`BoardLogic::audio_level` scale, 0..=32767 =
    /// APU full scale) that is added to every following cycle until the next call.
    #[inline]
    pub fn push_ext(&mut self, level: i32) {
        self.ext = level as f32 * (1.0 / 32767.0);
    }

    /// One CPU cycle's channel levels: `pulse` = pulse1 + pulse2 (0..=30),
    /// `tnd` = 3*triangle + 2*noise + dmc (0..=202).
    #[inline]
    fn push(&mut self, pulse: u8, tnd: u8) {
        if self.muted {
            return;
        }
        let key = (pulse as u16) << 8 | tnd as u16;
        if key != self.last_key {
            self.last_key = key;
            self.level = self.pulse_table[pulse as usize] + self.tnd_table[tnd as usize];
        }
        self.acc += self.level + self.ext;
        self.count += 1;
        self.phase += self.step;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            self.emit();
        }
    }

    #[cold]
    fn emit(&mut self) {
        let x = self.acc / self.count as f32;
        self.acc = 0.0;
        self.count = 0;
        let y1 = self.hp90_a * (self.hp90_prev_y + x - self.hp90_prev_x);
        self.hp90_prev_x = x;
        self.hp90_prev_y = y1;
        let y2 = self.hp440_a * (self.hp440_prev_y + y1 - self.hp440_prev_x);
        self.hp440_prev_x = y1;
        self.hp440_prev_y = y2;
        let y3 = self.lp_prev_y + self.lp14k_b * (y2 - self.lp_prev_y);
        self.lp_prev_y = y3;
        self.buffer.push((y3 * 2.0).clamp(-1.0, 1.0));
    }

    /// Appends buffered samples to `out` and empties the buffer.
    pub fn drain(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.buffer);
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.acc = 0.0;
        self.count = 0;
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(apu: &mut Apu, out: &mut AudioOut, cycles: u32) {
        for _ in 0..cycles {
            apu.tick(out);
        }
    }

    #[test]
    fn apu_length_counter_load_and_status() {
        let mut apu = Apu::new(Region::Ntsc);
        let mut out = AudioOut::new(Region::Ntsc, 48_000.0);
        apu.write_register(0x4015, 0x01);
        apu.write_register(0x4003, 0x08);
        run(&mut apu, &mut out, 1); // the reload latches until the next cycle
        assert_eq!(apu.read_status() & 1, 1);
        assert_eq!(apu.pulse1.length, 254);
    }

    #[test]
    fn apu_disabled_channel_ignores_length_load() {
        let mut apu = Apu::new(Region::Ntsc);
        let mut out = AudioOut::new(Region::Ntsc, 48_000.0);
        apu.write_register(0x4015, 0x00);
        apu.write_register(0x4003, 0x08);
        run(&mut apu, &mut out, 1);
        assert_eq!(apu.read_status() & 1, 0);
    }

    #[test]
    fn apu_four_step_sets_frame_irq_and_read_clears() {
        let mut apu = Apu::new(Region::Ntsc);
        let mut out = AudioOut::new(Region::Ntsc, 48_000.0);
        apu.write_register(0x4017, 0x00);
        run(&mut apu, &mut out, 29_000);
        assert!(!apu.irq());
        run(&mut apu, &mut out, 29_830 + 4 - 29_000);
        assert!(apu.irq());
        assert_ne!(apu.read_status() & 0x40, 0);
        assert!(!apu.irq());
        assert_eq!(apu.peek_status() & 0x40, 0);
    }

    #[test]
    fn apu_five_step_never_sets_frame_irq() {
        let mut apu = Apu::new(Region::Ntsc);
        let mut out = AudioOut::new(Region::Ntsc, 48_000.0);
        apu.write_register(0x4017, 0x80);
        run(&mut apu, &mut out, 200_000);
        assert!(!apu.irq());
    }

    #[test]
    fn apu_dmc_address_wraps() {
        let mut apu = Apu::new(Region::Ntsc);
        apu.write_register(0x4012, 0xFF);
        apu.write_register(0x4013, 0x10);
        apu.write_register(0x4015, 0x10);
        assert_eq!(apu.dmc_dma_request(), Some(0xFFC0));
        for _ in 0..0x40 {
            apu.dmc_dma_complete(0);
            assert_eq!(apu.dmc_dma_request(), None);
            apu.dmc.buffer_full = false;
        }
        assert_eq!(apu.dmc_dma_request(), Some(0x8000));
    }

    #[test]
    fn apu_audio_sample_count_per_ntsc_frame() {
        let mut apu = Apu::new(Region::Ntsc);
        let mut out = AudioOut::new(Region::Ntsc, 48_000.0);
        run(&mut apu, &mut out, 29_780);
        let mut v = Vec::new();
        out.drain(&mut v);
        // 29780 * 48000 / 1789773 = 798.7
        assert!((797..=801).contains(&v.len()), "{}", v.len());
        out.drain(&mut v);
        assert!((797..=801).contains(&v.len()));
    }

    #[test]
    fn apu_pulse_440hz_zero_crossings() {
        let mut apu = Apu::new(Region::Ntsc);
        let mut out = AudioOut::new(Region::Ntsc, 48_000.0);
        apu.write_register(0x4015, 0x01);
        apu.write_register(0x4000, 0xBF); // 50% duty, halt, constant volume 15
        apu.write_register(0x4001, 0x08); // sweep off
        let period = 253u16; // 1789773 / (16 * 254) = 440.4 Hz
        apu.write_register(0x4002, period as u8);
        apu.write_register(0x4003, (period >> 8) as u8);
        run(&mut apu, &mut out, 1_789_773 / 5);
        out.clear();
        run(&mut apu, &mut out, 1_789_773);
        let mut v = Vec::new();
        out.drain(&mut v);
        let crossings = v.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        assert!((860..=900).contains(&crossings), "{crossings}");
        assert!(v.iter().any(|&s| s.abs() > 0.05));
    }
}
