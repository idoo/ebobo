//! YM2612 (OPN2) FM sound chip. Owner: sound slice.
//!
//! Contract with `system.rs`: the system calls [`Ym2612::tick`] once per output sample period
//! (`FM_SAMPLE_MCLK` master clocks), always, also during fast-forward: timers, envelopes and
//! phases are emulated state. Before a register access it ticks the chip up to the current
//! time. Integer arithmetic only.
//!
//! Six channels of four operators. An operator is a 20-bit phase counter, a 10-bit envelope
//! attenuation (0 = loudest, 0x3FF = silent, 0.09375 dB per step) and a log-sine / power table
//! pair that turns "phase + modulation" and "attenuation" into a signed 14-bit sample.
//! Operators are stored by operator number (1 to 4 at index 0 to 3); the registers address them
//! in the order 1, 3, 2, 4 and the chip computes them in that order too, so operator 2 reaches
//! operator 3 one sample late. Finer pipeline delays are not modelled, and neither is the
//! "ladder" step of the chip's DAC around zero.

use serde::{Deserialize, Serialize};

/// Quarter sine as attenuation in 4.8 fixed-point log2 units: `round(-log2(sin((i + 0.5) * pi / 512)) * 256)`.
/// Produced once with `python3 -c` from that formula and pasted, so no float is evaluated at build or run time.
const LOG_SIN: [u16; 256] = [
    2137, 1731, 1543, 1419, 1326, 1252, 1190, 1137, 1091, 1050, 1013, 979, 949, 920, 894, 869,
    846, 825, 804, 785, 767, 749, 732, 717, 701, 687, 672, 659, 646, 633, 621, 609,
    598, 587, 576, 566, 556, 546, 536, 527, 518, 509, 501, 492, 484, 476, 468, 461,
    453, 446, 439, 432, 425, 418, 411, 405, 399, 392, 386, 380, 375, 369, 363, 358,
    352, 347, 341, 336, 331, 326, 321, 316, 311, 307, 302, 297, 293, 289, 284, 280,
    276, 271, 267, 263, 259, 255, 251, 248, 244, 240, 236, 233, 229, 226, 222, 219,
    215, 212, 209, 205, 202, 199, 196, 193, 190, 187, 184, 181, 178, 175, 172, 169,
    167, 164, 161, 159, 156, 153, 151, 148, 146, 143, 141, 138, 136, 134, 131, 129,
    127, 125, 122, 120, 118, 116, 114, 112, 110, 108, 106, 104, 102, 100, 98, 96,
    94, 92, 91, 89, 87, 85, 83, 82, 80, 78, 77, 75, 74, 72, 70, 69,
    67, 66, 64, 63, 62, 60, 59, 57, 56, 55, 53, 52, 51, 49, 48, 47,
    46, 45, 43, 42, 41, 40, 39, 38, 37, 36, 35, 34, 33, 32, 31, 30,
    29, 28, 27, 26, 25, 24, 23, 23, 22, 21, 20, 20, 19, 18, 17, 17,
    16, 15, 15, 14, 13, 13, 12, 12, 11, 10, 10, 9, 9, 8, 8, 7,
    7, 7, 6, 6, 5, 5, 5, 4, 4, 4, 3, 3, 3, 2, 2, 2,
    2, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// Power table, 11 bits: `round(2 ** (1 - (i + 1) / 256) * 1024)`, the fraction of an octave below full
/// scale. Produced once with `python3 -c` from that formula and pasted.
const POW: [u16; 256] = [
    2042, 2037, 2031, 2026, 2020, 2015, 2010, 2004, 1999, 1993, 1988, 1983, 1977, 1972, 1966, 1961,
    1956, 1951, 1945, 1940, 1935, 1930, 1924, 1919, 1914, 1909, 1904, 1898, 1893, 1888, 1883, 1878,
    1873, 1868, 1863, 1858, 1853, 1848, 1843, 1838, 1833, 1828, 1823, 1818, 1813, 1808, 1803, 1798,
    1794, 1789, 1784, 1779, 1774, 1769, 1765, 1760, 1755, 1750, 1746, 1741, 1736, 1732, 1727, 1722,
    1717, 1713, 1708, 1704, 1699, 1694, 1690, 1685, 1681, 1676, 1672, 1667, 1663, 1658, 1654, 1649,
    1645, 1640, 1636, 1631, 1627, 1623, 1618, 1614, 1609, 1605, 1601, 1596, 1592, 1588, 1584, 1579,
    1575, 1571, 1566, 1562, 1558, 1554, 1550, 1545, 1541, 1537, 1533, 1529, 1525, 1520, 1516, 1512,
    1508, 1504, 1500, 1496, 1492, 1488, 1484, 1480, 1476, 1472, 1468, 1464, 1460, 1456, 1452, 1448,
    1444, 1440, 1436, 1433, 1429, 1425, 1421, 1417, 1413, 1409, 1406, 1402, 1398, 1394, 1391, 1387,
    1383, 1379, 1376, 1372, 1368, 1364, 1361, 1357, 1353, 1350, 1346, 1342, 1339, 1335, 1332, 1328,
    1324, 1321, 1317, 1314, 1310, 1307, 1303, 1300, 1296, 1292, 1289, 1286, 1282, 1279, 1275, 1272,
    1268, 1265, 1261, 1258, 1255, 1251, 1248, 1244, 1241, 1238, 1234, 1231, 1228, 1224, 1221, 1218,
    1214, 1211, 1208, 1205, 1201, 1198, 1195, 1192, 1188, 1185, 1182, 1179, 1176, 1172, 1169, 1166,
    1163, 1160, 1157, 1154, 1150, 1147, 1144, 1141, 1138, 1135, 1132, 1129, 1126, 1123, 1120, 1117,
    1114, 1111, 1108, 1105, 1102, 1099, 1096, 1093, 1090, 1087, 1084, 1081, 1078, 1075, 1072, 1069,
    1066, 1064, 1061, 1058, 1055, 1052, 1049, 1046, 1044, 1041, 1038, 1035, 1032, 1030, 1027, 1024,
];

/// Detune in phase-increment units by detune level (0-3) and key code (0-31), the table of the
/// Yamaha OPN application manual. Bit 2 of the detune field subtracts instead of adding.
const DETUNE: [[u8; 32]; 4] = [
    [0; 32],
    [0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 8, 8, 8],
    [1, 1, 1, 1, 2, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 8, 9, 10, 11, 12, 13, 14, 16, 16, 16, 16],
    [2, 2, 2, 2, 2, 3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 20, 22, 22, 22, 22],
];

/// Envelope increments over eight consecutive updates for rates 8-47, by `rate & 3`.
const EG_LOW: [[u8; 8]; 4] = [[0, 1, 0, 1, 0, 1, 0, 1], [0, 1, 0, 1, 1, 1, 0, 1], [0, 1, 1, 1, 0, 1, 1, 1], [0, 1, 1, 1, 1, 1, 1, 1]];

/// Which of eight consecutive updates are doubled for rates 48-59, by `rate & 3`.
const EG_HIGH: [[u8; 8]; 4] = [[0, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 1, 0, 0, 0, 1], [0, 1, 0, 1, 0, 1, 0, 1], [0, 1, 1, 1, 0, 1, 1, 1]];

/// Samples per LFO step (128 steps per cycle) for the eight LFO rates: 3.85 Hz to 83 Hz on this clock.
const LFO_PERIOD: [u8; 8] = [108, 77, 71, 67, 62, 44, 8, 5];

/// Right shift of the 0..126 tremolo value by AM sensitivity: 0, 1.4, 5.9 and 11.8 dB.
const AM_SHIFT: [u8; 4] = [8, 3, 1, 0];

/// Vibrato: what bit 10 of the F-number adds to the doubled F-number, by PM sensitivity (0, 3.4, 6.7,
/// 10, 14, 20, 40, 80 cents) and step of the rising quarter of the LFO cycle. Lower F-number bits add
/// the same value shifted right by their distance from bit 10.
const VIBRATO: [[u8; 8]; 8] = [
    [0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 4, 4, 4, 4],
    [0, 0, 0, 4, 4, 4, 8, 8],
    [0, 0, 4, 4, 8, 8, 12, 12],
    [0, 0, 4, 8, 8, 8, 12, 16],
    [0, 0, 8, 12, 16, 16, 20, 24],
    [0, 0, 16, 24, 32, 32, 40, 48],
    [0, 0, 32, 48, 64, 64, 80, 96],
];

/// Operator index for the register slot `(address >> 2) & 3`: the registers run 1, 3, 2, 4.
const SLOT_TO_OP: [usize; 4] = [0, 2, 1, 3];

/// Silent envelope level.
const MAX_LEVEL: u16 = 0x3FF;

/// Ticks the busy bit stays set after a data write (the chip's is about 1.3 sample periods).
const BUSY_TICKS: u8 = 2;

/// Mix gain: six 9-bit channels (-256..255) times 21 stay within ±32767.
const MIX_GAIN: i32 = 21;

/// Envelope increment for an effective rate (0-63) at a step (0-7) of its update pattern.
fn eg_increment(rate: u8, step: usize) -> u16 {
    let pattern = usize::from(rate & 3);
    match rate {
        0 | 1 => 0,
        2..=5 => u16::from(EG_LOW[0][step]),
        6 | 7 => u16::from(EG_LOW[2][step]),
        8..=47 => u16::from(EG_LOW[pattern][step]),
        48..=59 => (1 + u16::from(EG_HIGH[pattern][step])) << ((rate >> 2) - 12),
        _ => 8,
    }
}

/// Key code (0-31): the block and the top of the F-number; scales envelope rates and picks the detune.
fn key_code(fnum: u16, block: u8) -> u8 {
    let f11 = fnum >> 10 & 1;
    let f10 = fnum >> 9 & 1;
    let f9 = fnum >> 8 & 1;
    let f8 = fnum >> 7 & 1;
    let n3 = (f11 & (f10 | f9 | f8)) | ((f11 ^ 1) & f10 & f9 & f8);
    block << 2 | (f11 << 1 | n3) as u8
}

/// Signed change of the doubled F-number at an LFO step (0-31, a quarter cycle is 8 steps).
fn vibrato(fnum: u16, fms: u8, lfo_step: u8) -> i32 {
    let quarter = if lfo_step & 8 != 0 { 7 - (lfo_step & 7) } else { lfo_step & 7 };
    let base = i32::from(VIBRATO[usize::from(fms)][usize::from(quarter)]);
    if base == 0 {
        return 0;
    }
    let mut delta = 0;
    for bit in 4..=10 {
        if fnum >> bit & 1 != 0 {
            delta += base >> (10 - bit);
        }
    }
    if lfo_step & 16 != 0 { -delta } else { delta }
}

/// Phase of the envelope generator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum EgPhase {
    Attack,
    Decay,
    Sustain,
    Release,
}

/// One operator: its registers and its phase and envelope generators.
#[derive(Clone, Hash, Serialize, Deserialize)]
struct Operator {
    /// 0x30 bits 4-6: detune, bit 2 = downwards.
    detune: u8,
    /// 0x30 bits 0-3: frequency multiple, 0 = one half.
    multiple: u8,
    /// 0x40: total level, 0.75 dB per step.
    total_level: u8,
    /// 0x50 bits 6-7: rate scaling by key code.
    key_scale: u8,
    /// 0x50 bits 0-4.
    attack_rate: u8,
    /// 0x60 bit 7: tremolo from the LFO.
    am: bool,
    /// 0x60 bits 0-4.
    decay_rate: u8,
    /// 0x70 bits 0-4: second decay rate.
    sustain_rate: u8,
    /// 0x80 bits 4-7: level where decay turns into sustain, 3 dB per step.
    sustain_level: u8,
    /// 0x80 bits 0-3.
    release_rate: u8,
    /// 0x90 bits 0-3: SSG-EG enable, attack (inverted), alternate, hold.
    ssg: u8,
    /// Own F-number (11 bits) of operators 1-3 of channel 3 in its special mode.
    fnum: u16,
    /// Own block (octave) for the same.
    block: u8,
    /// Key state from register 0x28.
    key: bool,
    /// Phase counter, 20 bits; the top 10 index the sine.
    phase: u32,
    /// Envelope phase.
    eg: EgPhase,
    /// Envelope attenuation, 10 bits.
    level: u16,
    /// SSG-EG: the envelope shape is currently mirrored.
    ssg_inverted: bool,
    /// Output of the last sample, signed 14 bits.
    out: i16,
    /// Output of the sample before.
    prev_out: i16,
}

impl Operator {
    fn new() -> Operator {
        Operator {
            detune: 0,
            multiple: 0,
            total_level: 0,
            key_scale: 0,
            attack_rate: 0,
            am: false,
            decay_rate: 0,
            sustain_rate: 0,
            sustain_level: 0,
            release_rate: 0,
            ssg: 0,
            fnum: 0,
            block: 0,
            key: false,
            phase: 0,
            eg: EgPhase::Release,
            level: MAX_LEVEL,
            ssg_inverted: false,
            out: 0,
            prev_out: 0,
        }
    }

    /// Effective rate 0-63 of a 5-bit register rate: twice the rate plus the key scaling; rate 0 never moves.
    fn rate(&self, r: u8, key_code: u8) -> u8 {
        if r == 0 { 0 } else { (2 * r + (key_code >> (3 - self.key_scale))).min(63) }
    }

    /// Whether SSG-EG currently mirrors the output level around 0x200.
    fn ssg_output_inverted(&self) -> bool {
        self.ssg & 8 != 0 && self.ssg_inverted != (self.ssg & 4 != 0)
    }

    /// Starts the attack; the two fastest rates open the envelope at once.
    fn start_attack(&mut self, key_code: u8) {
        self.eg = EgPhase::Attack;
        if self.rate(self.attack_rate, key_code) >= 62 {
            self.level = 0;
        }
    }

    /// Key-on edge: phase restarts and the envelope attacks.
    fn key_on(&mut self, key_code: u8) {
        self.phase = 0;
        self.ssg_inverted = false;
        self.start_attack(key_code);
    }

    /// Key-off edge: release from the level that is audible now.
    fn key_off(&mut self) {
        if self.eg != EgPhase::Release {
            if self.ssg_output_inverted() {
                self.level = 0x200u16.wrapping_sub(self.level) & MAX_LEVEL;
            }
            self.eg = EgPhase::Release;
        }
    }

    /// SSG-EG, checked every sample: what happens when the envelope has fallen to the middle (0x200).
    fn tick_ssg(&mut self, key_code: u8) {
        if self.ssg & 8 == 0 || self.level < 0x200 {
            return;
        }
        if self.eg == EgPhase::Release {
            self.level = MAX_LEVEL;
            return;
        }
        let (hold, alternate, attack) = (self.ssg & 1 != 0, self.ssg & 2 != 0, self.ssg & 4 != 0);
        if hold {
            if alternate {
                self.ssg_inverted = true;
            }
            if self.eg != EgPhase::Attack && self.ssg_inverted == attack {
                self.level = MAX_LEVEL;
            }
        } else {
            if alternate {
                self.ssg_inverted = !self.ssg_inverted;
            } else {
                self.phase = 0;
            }
            if self.eg != EgPhase::Attack {
                self.start_attack(key_code);
            }
        }
    }

    /// One step of the global envelope counter (every third sample).
    fn tick_envelope(&mut self, counter: u16, key_code: u8) {
        if self.eg == EgPhase::Attack && self.level == 0 {
            self.eg = EgPhase::Decay;
        }
        let sustain = if self.sustain_level == 15 { 0x3E0 } else { u16::from(self.sustain_level) << 5 };
        if self.eg == EgPhase::Decay && self.level >= sustain {
            self.eg = EgPhase::Sustain;
        }
        let r = match self.eg {
            EgPhase::Attack => self.attack_rate,
            EgPhase::Decay => self.decay_rate,
            EgPhase::Sustain => self.sustain_rate,
            EgPhase::Release => self.release_rate * 2 + 1,
        };
        let rate = self.rate(r, key_code);
        let shift = 11u8.saturating_sub(rate >> 2);
        if counter & ((1 << shift) - 1) != 0 {
            return;
        }
        let inc = eg_increment(rate, usize::from(counter >> shift) & 7);
        if self.eg == EgPhase::Attack {
            if rate < 62 {
                let level = i32::from(self.level);
                self.level = (level + ((!level * i32::from(inc)) >> 4)).max(0) as u16;
            }
        } else if self.ssg & 8 != 0 {
            if self.level < 0x200 {
                self.level += 4 * inc;
            }
        } else {
            self.level = (self.level + inc).min(MAX_LEVEL);
        }
    }

    /// The sample at the current phase plus `modulation` (in 1/1024 of a cycle), signed 14 bits.
    /// `tremolo` is the LFO attenuation for this channel in envelope steps.
    fn output(&self, modulation: i32, tremolo: u16) -> i32 {
        let mut level = self.level;
        if self.eg != EgPhase::Release && self.ssg_output_inverted() {
            level = 0x200u16.wrapping_sub(level) & MAX_LEVEL;
        }
        let mut attenuation = level + (u16::from(self.total_level) << 3);
        if self.am {
            attenuation += tremolo;
        }
        let attenuation = attenuation.min(MAX_LEVEL);
        let phase = ((self.phase >> 10) as i32 + modulation) & 0x3FF;
        let mut index = (phase & 0xFF) as usize;
        if phase & 0x100 != 0 {
            index ^= 0xFF;
        }
        let total = u32::from(LOG_SIN[index]) + (u32::from(attenuation) << 2);
        let shift = total >> 8;
        let magnitude = if shift >= 13 { 0 } else { (i32::from(POW[(total & 0xFF) as usize]) << 2) >> shift };
        if phase & 0x200 != 0 { -magnitude } else { magnitude }
    }

    /// Adds one sample's phase increment for the given frequency, vibrato included.
    fn advance_phase(&mut self, fnum: u16, block: u8, key_code: u8, fms: u8, lfo_step: u8) {
        let doubled = ((i32::from(fnum) << 1) + vibrato(fnum, fms, lfo_step)) & 0xFFF;
        let mut inc = ((doubled as u32) << block) >> 2;
        let detune = u32::from(DETUNE[usize::from(self.detune & 3)][usize::from(key_code)]);
        inc = if self.detune & 4 != 0 { inc.wrapping_sub(detune) } else { inc + detune } & 0x1_FFFF;
        inc = if self.multiple == 0 { inc >> 1 } else { inc * u32::from(self.multiple) };
        self.phase = (self.phase + inc) & 0xF_FFFF;
    }
}

/// One channel: four operators and the registers they share.
#[derive(Clone, Hash, Serialize, Deserialize)]
struct Channel {
    /// Operators 1-4.
    ops: [Operator; 4],
    /// F-number, 11 bits.
    fnum: u16,
    /// Block (octave), 3 bits.
    block: u8,
    /// Feedback of operator 1, 0 = none, 7 = strongest.
    feedback: u8,
    /// Algorithm 0-7.
    algorithm: u8,
    /// Heard on the left.
    pan_l: bool,
    /// Heard on the right.
    pan_r: bool,
    /// Tremolo sensitivity 0-3.
    ams: u8,
    /// Vibrato sensitivity 0-7.
    fms: u8,
}

impl Channel {
    fn new() -> Channel {
        Channel {
            ops: [Operator::new(), Operator::new(), Operator::new(), Operator::new()],
            fnum: 0,
            block: 0,
            feedback: 0,
            algorithm: 0,
            pan_l: true,
            pan_r: true,
            ams: 0,
            fms: 0,
        }
    }

    /// F-number and block of an operator; `special` gives operators 1-3 their own (channel 3 only).
    fn frequency(&self, op: usize, special: bool) -> (u16, u8) {
        if special && op < 3 { (self.ops[op].fnum, self.ops[op].block) } else { (self.fnum, self.block) }
    }

    /// One sample: envelopes (when `eg_step` carries the global counter), operators, algorithm.
    /// Returns the channel as the chip's 9-bit DAC sees it, -256..255.
    fn tick(&mut self, special: bool, eg_step: Option<u16>, lfo_am: u16, lfo_step: u8) -> i32 {
        let tremolo = lfo_am >> AM_SHIFT[usize::from(self.ams)];
        let late_op2 = i32::from(self.ops[1].out);
        let mut outs = [0i32; 4];
        for index in SLOT_TO_OP {
            let (fnum, block) = self.frequency(index, special);
            let key_code = key_code(fnum, block);
            let modulation = match (index, self.algorithm) {
                (0, _) if self.feedback != 0 => (i32::from(self.ops[0].out) + i32::from(self.ops[0].prev_out)) >> (10 - self.feedback),
                (1, 0 | 3 | 4 | 5 | 6) => outs[0] >> 1,
                (2, 0 | 2) => late_op2 >> 1,
                (2, 1) => (outs[0] + late_op2) >> 1,
                (2, 5) => outs[0] >> 1,
                (3, 0 | 1 | 4) => outs[2] >> 1,
                (3, 2) => (outs[0] + outs[2]) >> 1,
                (3, 3) => (outs[1] + outs[2]) >> 1,
                (3, 5) => outs[0] >> 1,
                _ => 0,
            };
            let fms = self.fms;
            let op = &mut self.ops[index];
            op.tick_ssg(key_code);
            if let Some(counter) = eg_step {
                op.tick_envelope(counter, key_code);
            }
            let out = op.output(modulation, tremolo);
            op.advance_phase(fnum, block, key_code, fms, lfo_step);
            op.prev_out = op.out;
            op.out = out as i16;
            outs[index] = out;
        }
        let sum = match self.algorithm {
            0..=3 => outs[3],
            4 => outs[1] + outs[3],
            5 | 6 => outs[1] + outs[2] + outs[3],
            _ => outs[0] + outs[1] + outs[2] + outs[3],
        };
        sum.clamp(-8192, 8191) >> 5
    }
}

/// Complete emulated state of the chip.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Ym2612 {
    /// Channels 1-6; part I addresses 1-3, part II 4-6.
    channels: [Channel; 6],
    /// Address latch of part I and of part II.
    addr: [u8; 2],
    /// 0xA4-0xA6: block and F-number high bits, waiting for the low byte.
    fnum_latch: u8,
    /// 0xAC-0xAE: the same for the operator frequencies of channel 3.
    ch3_latch: u8,
    /// 0x22 bit 3.
    lfo_enabled: bool,
    /// 0x22 bits 0-2.
    lfo_rate: u8,
    /// Samples since the last LFO step.
    lfo_divider: u8,
    /// LFO position, 7 bits; held at 0 while the LFO is off.
    lfo_counter: u8,
    /// Timer A reload value N, 10 bits: overflows every 1024 - N ticks.
    timer_a_period: u16,
    /// Timer A counter, counts up to 1024.
    timer_a_counter: u16,
    /// Timer B reload value N, 8 bits: overflows every 16 x (256 - N) ticks.
    timer_b_period: u8,
    /// Timer B counter, counts up to 256.
    timer_b_counter: u16,
    /// Ticks since timer B last counted, 0-15.
    timer_b_divider: u8,
    /// 0x27 bits 0-3: run A, run B, flag enable A, flag enable B.
    timer_control: u8,
    /// 0x27 bits 6-7: 0 normal, 1 and 3 per-operator frequencies on channel 3, 2 the same plus CSM.
    ch3_mode: u8,
    /// Overflow flags: bit 0 timer A, bit 1 timer B.
    status: u8,
    /// Ticks the busy bit still shows.
    busy: u8,
    /// CSM keyed channel 3 on at the last timer A overflow; the next tick keys it off.
    csm_keyed: bool,
    /// 0x2A: DAC sample, unsigned 8 bits. Starts at 0x80, the centre: a sound driver that
    /// enables the DAC before its first sample byte must not put a full-scale step on channel 6.
    dac: u8,
    /// 0x2B bit 7: the DAC replaces channel 6.
    dac_enabled: bool,
    /// Samples since the envelope counter stepped, 0-2.
    eg_divider: u8,
    /// Global envelope counter, 1-4095.
    eg_counter: u16,
}

impl Default for Ym2612 {
    fn default() -> Ym2612 {
        Ym2612::new()
    }
}

impl Ym2612 {
    /// Power-on state.
    pub fn new() -> Ym2612 {
        Ym2612 {
            channels: [Channel::new(), Channel::new(), Channel::new(), Channel::new(), Channel::new(), Channel::new()],
            addr: [0; 2],
            fnum_latch: 0,
            ch3_latch: 0,
            lfo_enabled: false,
            lfo_rate: 0,
            lfo_divider: 0,
            lfo_counter: 0,
            timer_a_period: 0,
            timer_a_counter: 0,
            timer_b_period: 0,
            timer_b_counter: 0,
            timer_b_divider: 0,
            timer_control: 0,
            ch3_mode: 0,
            status: 0,
            busy: 0,
            csm_keyed: false,
            dac: 0x80,
            dac_enabled: false,
            eg_divider: 0,
            eg_counter: 0,
        }
    }

    /// The reset line (the 68000's Z80-reset register pulls it too): all channels silent,
    /// registers cleared.
    pub fn reset(&mut self) {
        *self = Ym2612::new();
    }

    /// `port` 0-3 (0x4000-0x4003): 0 = address of part I, 1 = data, 2 = address of part II,
    /// 3 = data.
    pub fn write(&mut self, port: u8, val: u8) {
        let part = usize::from(port >> 1 & 1);
        if port & 1 == 0 {
            self.addr[part] = val;
            return;
        }
        self.busy = BUSY_TICKS;
        let addr = self.addr[part];
        match addr {
            0x20..=0x2F if part == 0 => self.write_global(addr, val),
            0x30..=0x9F => self.write_operator(part, addr, val),
            0xA0..=0xB6 => self.write_channel(part, addr, val),
            _ => {}
        }
    }

    /// Registers 0x20-0x2F (part I only): LFO, timers, channel 3 mode, key on/off, DAC.
    fn write_global(&mut self, addr: u8, val: u8) {
        match addr {
            0x22 => {
                self.lfo_enabled = val & 8 != 0;
                self.lfo_rate = val & 7;
                if !self.lfo_enabled {
                    self.lfo_divider = 0;
                    self.lfo_counter = 0;
                }
            }
            0x24 => self.timer_a_period = (self.timer_a_period & 3) | u16::from(val) << 2,
            0x25 => self.timer_a_period = (self.timer_a_period & 0x3FC) | u16::from(val & 3),
            0x26 => self.timer_b_period = val,
            0x27 => {
                let started = val & !self.timer_control;
                if started & 1 != 0 {
                    self.timer_a_counter = self.timer_a_period;
                }
                if started & 2 != 0 {
                    self.timer_b_counter = u16::from(self.timer_b_period);
                    self.timer_b_divider = 0;
                }
                self.timer_control = val & 0x0F;
                if val & 0x10 != 0 {
                    self.status &= !1;
                }
                if val & 0x20 != 0 {
                    self.status &= !2;
                }
                self.ch3_mode = val >> 6;
            }
            0x28 => {
                if val & 3 == 3 {
                    return;
                }
                let index = usize::from(val & 3) + if val & 4 != 0 { 3 } else { 0 };
                let special = index == 2 && self.ch3_mode != 0;
                let channel = &mut self.channels[index];
                for op in 0..4 {
                    let on = val >> (4 + op) & 1 != 0;
                    let (fnum, block) = channel.frequency(op, special);
                    let operator = &mut channel.ops[op];
                    if on && !operator.key {
                        operator.key_on(key_code(fnum, block));
                    } else if !on && operator.key {
                        operator.key_off();
                    }
                    operator.key = on;
                }
            }
            0x2A => self.dac = val,
            0x2B => self.dac_enabled = val & 0x80 != 0,
            _ => {}
        }
    }

    /// Registers 0x30-0x9F: one operator of one channel.
    fn write_operator(&mut self, part: usize, addr: u8, val: u8) {
        if addr & 3 == 3 {
            return;
        }
        let channel = &mut self.channels[usize::from(addr & 3) + part * 3];
        let op = &mut channel.ops[SLOT_TO_OP[usize::from(addr >> 2 & 3)]];
        match addr & 0xF0 {
            0x30 => {
                op.detune = val >> 4 & 7;
                op.multiple = val & 0x0F;
            }
            0x40 => op.total_level = val & 0x7F,
            0x50 => {
                op.key_scale = val >> 6;
                op.attack_rate = val & 0x1F;
            }
            0x60 => {
                op.am = val & 0x80 != 0;
                op.decay_rate = val & 0x1F;
            }
            0x70 => op.sustain_rate = val & 0x1F,
            0x80 => {
                op.sustain_level = val >> 4;
                op.release_rate = val & 0x0F;
            }
            _ => op.ssg = val & 0x0F,
        }
    }

    /// Registers 0xA0-0xB6: frequency, channel 3 operator frequencies, feedback/algorithm, pan/LFO.
    fn write_channel(&mut self, part: usize, addr: u8, val: u8) {
        if addr & 3 == 3 {
            return;
        }
        let ch = usize::from(addr & 3);
        let channel = &mut self.channels[ch + part * 3];
        match addr & 0xFC {
            0xA0 => {
                channel.fnum = u16::from(self.fnum_latch & 7) << 8 | u16::from(val);
                channel.block = self.fnum_latch >> 3 & 7;
            }
            0xA4 => self.fnum_latch = val,
            0xA8 if part == 0 => {
                // 0xA8 is operator 3, 0xA9 operator 1, 0xAA operator 2; operator 4 follows the channel.
                let op = &mut self.channels[2].ops[[2, 0, 1][ch]];
                op.fnum = u16::from(self.ch3_latch & 7) << 8 | u16::from(val);
                op.block = self.ch3_latch >> 3 & 7;
            }
            0xAC if part == 0 => self.ch3_latch = val,
            0xB0 => {
                channel.feedback = val >> 3 & 7;
                channel.algorithm = val & 7;
            }
            0xB4 => {
                channel.pan_l = val & 0x80 != 0;
                channel.pan_r = val & 0x40 != 0;
                channel.ams = val >> 4 & 3;
                channel.fms = val & 7;
            }
            _ => {}
        }
    }

    /// Status (the same on all four ports): bit 7 busy, bit 1 timer B overflowed, bit 0
    /// timer A overflowed.
    pub fn read(&mut self) -> u8 {
        self.status | if self.busy > 0 { 0x80 } else { 0 }
    }

    /// Timer A counts every tick, timer B every 16th; an overflow reloads and, when enabled, sets its flag.
    fn tick_timers(&mut self) {
        if self.timer_control & 1 != 0 {
            self.timer_a_counter += 1;
            if self.timer_a_counter >= 1024 {
                self.timer_a_counter = self.timer_a_period;
                if self.timer_control & 4 != 0 {
                    self.status |= 1;
                }
                if self.ch3_mode == 2 {
                    let channel = &mut self.channels[2];
                    for op in 0..4 {
                        let (fnum, block) = channel.frequency(op, true);
                        if !channel.ops[op].key {
                            channel.ops[op].key_on(key_code(fnum, block));
                        }
                    }
                    self.csm_keyed = true;
                }
            }
        }
        if self.timer_control & 2 != 0 {
            self.timer_b_divider += 1;
            if self.timer_b_divider == 16 {
                self.timer_b_divider = 0;
                self.timer_b_counter += 1;
                if self.timer_b_counter >= 256 {
                    self.timer_b_counter = u16::from(self.timer_b_period);
                    if self.timer_control & 8 != 0 {
                        self.status |= 2;
                    }
                }
            }
        }
    }

    /// One output sample period: advances timers, LFO, envelopes and phases and returns the
    /// mix of the six channels as (left, right), panning applied, each within ±32767.
    pub fn tick(&mut self) -> (i16, i16) {
        self.busy = self.busy.saturating_sub(1);
        if self.csm_keyed {
            self.csm_keyed = false;
            for op in &mut self.channels[2].ops {
                if !op.key {
                    op.key_off();
                }
            }
        }
        self.tick_timers();
        if self.lfo_enabled {
            self.lfo_divider += 1;
            if self.lfo_divider >= LFO_PERIOD[usize::from(self.lfo_rate)] {
                self.lfo_divider = 0;
                self.lfo_counter = (self.lfo_counter + 1) & 0x7F;
            }
        }
        // Tremolo is an inverted triangle, 126 at the start of the cycle (and while the LFO is off).
        let lfo_am = u16::from(if self.lfo_counter & 0x40 == 0 { self.lfo_counter ^ 0x3F } else { self.lfo_counter & 0x3F }) << 1;
        let lfo_step = self.lfo_counter >> 2;
        self.eg_divider += 1;
        let eg_step = if self.eg_divider == 3 {
            self.eg_divider = 0;
            self.eg_counter = if self.eg_counter >= 0xFFF { 1 } else { self.eg_counter + 1 };
            Some(self.eg_counter)
        } else {
            None
        };
        let (mut left, mut right) = (0i32, 0i32);
        for (index, channel) in self.channels.iter_mut().enumerate() {
            let special = index == 2 && self.ch3_mode != 0;
            let mut out = channel.tick(special, eg_step, lfo_am, lfo_step);
            if index == 5 && self.dac_enabled {
                out = (i32::from(self.dac) - 128) << 1;
            }
            if channel.pan_l {
                left += out;
            }
            if channel.pan_r {
                right += out;
            }
        }
        ((left * MIX_GAIN) as i16, (right * MIX_GAIN) as i16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    /// Ticks per second on an NTSC machine: master clock / 7 / 144.
    const RATE: u64 = 53267;

    /// Peak of one channel at full volume in the mix: the largest 9-bit sample times the mix gain.
    const PEAK: i16 = 255 * 21;

    fn reg(chip: &mut Ym2612, part: u8, addr: u8, val: u8) {
        chip.write(part * 2, addr);
        chip.write(part * 2 + 1, val);
    }

    /// Register offset of an operator (0-3 = operators 1-4) within a 0x30-0x9F group.
    fn slot(op: usize) -> u8 {
        [0, 8, 4, 12][op]
    }

    /// Channel `ch` (0-2) of part I: fastest attack, no decay, release rate `rr`, every operator at total
    /// level 127 except `loud`, frequency set; not keyed yet.
    fn voice(chip: &mut Ym2612, ch: u8, algorithm: u8, loud: Option<usize>, fnum: u16, block: u8, rr: u8) {
        for op in 0..4 {
            let s = slot(op) + ch;
            reg(chip, 0, 0x30 + s, 0x01);
            reg(chip, 0, 0x40 + s, if loud == Some(op) { 0 } else { 127 });
            reg(chip, 0, 0x50 + s, 0x1F);
            reg(chip, 0, 0x60 + s, 0);
            reg(chip, 0, 0x70 + s, 0);
            reg(chip, 0, 0x80 + s, rr);
        }
        reg(chip, 0, 0xB0 + ch, algorithm);
        reg(chip, 0, 0xB4 + ch, 0xC0);
        reg(chip, 0, 0xA4 + ch, block << 3 | (fnum >> 8) as u8);
        reg(chip, 0, 0xA0 + ch, fnum as u8);
    }

    /// Whether any of the next `ticks` samples is not zero.
    fn sounds(chip: &mut Ym2612, ticks: u32) -> bool {
        let mut any = false;
        for _ in 0..ticks {
            any |= chip.tick() != (0, 0);
        }
        any
    }

    /// Rising zero crossings of the left output over one second.
    fn frequency(chip: &mut Ym2612) -> u64 {
        let mut crossings = 0;
        let mut prev = chip.tick().0;
        for _ in 0..RATE {
            let cur = chip.tick().0;
            if prev < 0 && cur >= 0 {
                crossings += 1;
            }
            prev = cur;
        }
        crossings
    }

    fn expected_hz(fnum: u16, block: u8) -> u64 {
        ((u64::from(fnum) << block) * RATE) >> 21
    }

    #[test]
    fn timer_a_overflows_after_1024_minus_n_ticks() {
        for n in [0u16, 1, 500, 1000, 1023] {
            let mut chip = Ym2612::new();
            reg(&mut chip, 0, 0x24, (n >> 2) as u8);
            reg(&mut chip, 0, 0x25, (n & 3) as u8);
            reg(&mut chip, 0, 0x27, 0x05);
            for _ in 0..1024 - n - 1 {
                chip.tick();
                assert_eq!(chip.read() & 1, 0, "N = {n}");
            }
            chip.tick();
            assert_eq!(chip.read() & 3, 1, "N = {n}");
            // It reloads and keeps the same period.
            reg(&mut chip, 0, 0x27, 0x15);
            assert_eq!(chip.read() & 1, 0);
            for _ in 0..1024 - n - 1 {
                chip.tick();
                assert_eq!(chip.read() & 1, 0, "N = {n}, second period");
            }
            chip.tick();
            assert_eq!(chip.read() & 1, 1, "N = {n}, second period");
        }
    }

    #[test]
    fn timer_b_overflows_after_16_times_256_minus_n_ticks() {
        for n in [0u32, 200, 255] {
            let mut chip = Ym2612::new();
            reg(&mut chip, 0, 0x26, n as u8);
            reg(&mut chip, 0, 0x27, 0x0A);
            for _ in 0..16 * (256 - n) - 1 {
                chip.tick();
                assert_eq!(chip.read() & 2, 0, "N = {n}");
            }
            chip.tick();
            assert_eq!(chip.read() & 3, 2, "N = {n}");
        }
    }

    #[test]
    fn timer_flags_need_their_enable_bits_and_clear_by_the_reset_bits() {
        let mut chip = Ym2612::new();
        reg(&mut chip, 0, 0x24, 0xFF);
        reg(&mut chip, 0, 0x25, 0x03);
        reg(&mut chip, 0, 0x26, 0xFF);
        reg(&mut chip, 0, 0x27, 0x03);
        for _ in 0..64 {
            chip.tick();
        }
        assert_eq!(chip.read() & 3, 0, "running without the enable bits sets no flag");
        reg(&mut chip, 0, 0x27, 0x0F);
        for _ in 0..64 {
            chip.tick();
        }
        assert_eq!(chip.read() & 3, 3);
        reg(&mut chip, 0, 0x27, 0x10);
        assert_eq!(chip.read() & 3, 2, "bit 4 clears only timer A's flag");
        reg(&mut chip, 0, 0x27, 0x20);
        assert_eq!(chip.read() & 3, 0, "bit 5 clears timer B's flag");
    }

    #[test]
    fn busy_shows_for_a_short_time_after_a_data_write() {
        let mut chip = Ym2612::new();
        assert_eq!(chip.read() & 0x80, 0);
        chip.write(0, 0x40);
        assert_eq!(chip.read() & 0x80, 0, "an address write is not busy");
        chip.write(1, 0x00);
        assert_eq!(chip.read() & 0x80, 0x80);
        for _ in 0..BUSY_TICKS {
            chip.tick();
        }
        assert_eq!(chip.read() & 0x80, 0);
    }

    #[test]
    fn carrier_is_a_sine_of_the_frequency_of_its_f_number_and_block() {
        for (fnum, block) in [(1083u16, 4u8), (617, 5), (1200, 2), (700, 7)] {
            let mut chip = Ym2612::new();
            voice(&mut chip, 0, 7, Some(0), fnum, block, 15);
            reg(&mut chip, 0, 0x28, 0x10);
            let got = frequency(&mut chip);
            let want = expected_hz(fnum, block);
            assert!(got.abs_diff(want) <= 1, "fnum {fnum} block {block}: {got} Hz, expected {want} Hz");
        }
    }

    #[test]
    fn carrier_reaches_a_stable_peak() {
        let mut chip = Ym2612::new();
        voice(&mut chip, 0, 7, Some(0), 1083, 4, 15);
        reg(&mut chip, 0, 0x28, 0x10);
        for _ in 0..1000 {
            chip.tick();
        }
        let mut peaks = [0i16; 2];
        for peak in &mut peaks {
            for _ in 0..4000 {
                let (l, r) = chip.tick();
                assert_eq!(l, r);
                *peak = (*peak).max(l);
            }
        }
        assert_eq!(peaks, [PEAK, PEAK]);
    }

    #[test]
    fn key_off_decays_to_silence_at_the_release_rate() {
        // Release rate 15 closes the envelope by 8 steps every third tick: silent within 400 ticks.
        let mut chip = Ym2612::new();
        voice(&mut chip, 0, 7, Some(0), 1083, 4, 15);
        reg(&mut chip, 0, 0x28, 0x10);
        assert!(sounds(&mut chip, 1000));
        reg(&mut chip, 0, 0x28, 0x00);
        assert!(sounds(&mut chip, 60), "still audible right after key-off");
        sounds(&mut chip, 340);
        assert!(!sounds(&mut chip, 3000), "silent after the release");

        // Release rate 8 is far slower: one step per 24 ticks on average.
        let mut chip = Ym2612::new();
        voice(&mut chip, 0, 7, Some(0), 1083, 4, 8);
        reg(&mut chip, 0, 0x28, 0x10);
        sounds(&mut chip, 1000);
        reg(&mut chip, 0, 0x28, 0x00);
        sounds(&mut chip, 5000);
        assert!(sounds(&mut chip, 500), "a slow release is still audible after 5000 ticks");
        sounds(&mut chip, 25000);
        assert!(!sounds(&mut chip, 3000), "and silent in the end");
    }

    #[test]
    fn total_level_127_is_silent() {
        let mut chip = Ym2612::new();
        voice(&mut chip, 0, 7, None, 1083, 4, 15);
        reg(&mut chip, 0, 0x28, 0xF0);
        assert!(!sounds(&mut chip, 5000));
    }

    #[test]
    fn each_algorithm_has_its_documented_carriers() {
        let carriers: [[bool; 4]; 8] = [
            [false, false, false, true],
            [false, false, false, true],
            [false, false, false, true],
            [false, false, false, true],
            [false, true, false, true],
            [false, true, true, true],
            [false, true, true, true],
            [true, true, true, true],
        ];
        for (algorithm, expected) in carriers.iter().enumerate() {
            for (op, &carrier) in expected.iter().enumerate() {
                let mut chip = Ym2612::new();
                voice(&mut chip, 1, algorithm as u8, Some(op), 1083, 4, 15);
                reg(&mut chip, 0, 0x28, 0xF1);
                assert_eq!(sounds(&mut chip, 2000), carrier, "algorithm {algorithm}, operator {}", op + 1);
            }
        }
    }

    #[test]
    fn dac_puts_its_byte_on_channel_6() {
        let mut chip = Ym2612::new();
        reg(&mut chip, 0, 0x2B, 0x80);
        assert_eq!(chip.tick(), (0, 0), "enabled before any sample byte: the centre, silence");
        reg(&mut chip, 0, 0x2B, 0x00);
        reg(&mut chip, 0, 0x2A, 0xFF);
        assert_eq!(chip.tick(), (0, 0), "the DAC is off until 0x2B enables it");
        reg(&mut chip, 0, 0x2B, 0x80);
        assert_eq!(chip.tick(), (254 * 21, 254 * 21));
        reg(&mut chip, 0, 0x2A, 0x00);
        assert_eq!(chip.tick(), (-256 * 21, -256 * 21));
        reg(&mut chip, 0, 0x2A, 0x80);
        assert_eq!(chip.tick(), (0, 0));
        reg(&mut chip, 0, 0x2A, 0xC0);
        reg(&mut chip, 0, 0x2B, 0x00);
        assert_eq!(chip.tick(), (0, 0));
    }

    #[test]
    fn pan_bits_gate_left_and_right() {
        let mut chip = Ym2612::new();
        reg(&mut chip, 0, 0x2B, 0x80);
        reg(&mut chip, 0, 0x2A, 0xFF);
        reg(&mut chip, 1, 0xB6, 0x80);
        assert_eq!(chip.tick(), (254 * 21, 0));
        reg(&mut chip, 1, 0xB6, 0x40);
        assert_eq!(chip.tick(), (0, 254 * 21));
        reg(&mut chip, 1, 0xB6, 0x00);
        assert_eq!(chip.tick(), (0, 0));

        let mut chip = Ym2612::new();
        voice(&mut chip, 0, 7, Some(0), 1083, 4, 15);
        reg(&mut chip, 0, 0xB4, 0x40);
        reg(&mut chip, 0, 0x28, 0x10);
        let (mut left, mut right) = (false, false);
        for _ in 0..2000 {
            let (l, r) = chip.tick();
            left |= l != 0;
            right |= r != 0;
        }
        assert!(!left && right);
    }

    #[test]
    fn channel_3_special_mode_gives_operators_their_own_frequency() {
        let mut chip = Ym2612::new();
        voice(&mut chip, 2, 7, Some(0), 1083, 4, 15);
        // Operator 1 of channel 3 through 0xAD/0xA9.
        reg(&mut chip, 0, 0xAD, 5 << 3 | (617u16 >> 8) as u8);
        reg(&mut chip, 0, 0xA9, 617u16 as u8);
        reg(&mut chip, 0, 0x28, 0x12);
        let normal = frequency(&mut chip);
        assert!(normal.abs_diff(expected_hz(1083, 4)) <= 1, "normal mode: {normal} Hz");
        reg(&mut chip, 0, 0x27, 0x40);
        let special = frequency(&mut chip);
        assert!(special.abs_diff(expected_hz(617, 5)) <= 1, "special mode: {special} Hz");
    }

    #[test]
    fn csm_keys_channel_3_from_timer_a() {
        let mut chip = Ym2612::new();
        voice(&mut chip, 2, 7, Some(0), 1083, 4, 2);
        reg(&mut chip, 0, 0xAD, 4 << 3 | (1083u16 >> 8) as u8);
        reg(&mut chip, 0, 0xA9, 1083u16 as u8);
        reg(&mut chip, 0, 0x24, 0xC0);
        assert!(!sounds(&mut chip, 2000), "nothing is keyed");
        reg(&mut chip, 0, 0x27, 0x81);
        assert!(sounds(&mut chip, 2000), "timer A overflows key the channel");
    }

    #[test]
    fn lfo_tremolo_and_vibrato_change_the_sound() {
        let run = |lfo: u8, sens: u8, am: u8| {
            let mut chip = Ym2612::new();
            voice(&mut chip, 0, 7, Some(0), 1083, 4, 15);
            reg(&mut chip, 0, 0x60, am);
            reg(&mut chip, 0, 0xB4, 0xC0 | sens);
            reg(&mut chip, 0, 0x22, lfo);
            reg(&mut chip, 0, 0x28, 0x10);
            let mut samples = Vec::with_capacity(20000);
            for _ in 0..20000 {
                samples.push(chip.tick().0);
            }
            samples
        };
        let plain = run(0x0F, 0x00, 0x00);
        let tremolo = run(0x0F, 0x30, 0x80);
        let vibrato = run(0x0F, 0x07, 0x00);
        assert_ne!(plain, tremolo);
        assert_ne!(plain, vibrato);
        // Tremolo only ever lowers the level; its loudest point is the plain peak.
        assert_eq!(plain.iter().max(), Some(&PEAK));
        assert_eq!(tremolo.iter().max(), Some(&PEAK));
        assert!(tremolo.iter().map(|s| i64::from(s.unsigned_abs())).sum::<i64>() < plain.iter().map(|s| i64::from(s.unsigned_abs())).sum::<i64>());
    }

    #[test]
    fn two_chips_fed_the_same_writes_hash_equal() {
        fn hash(chip: &Ym2612) -> u64 {
            let mut hasher = DefaultHasher::new();
            chip.hash(&mut hasher);
            hasher.finish()
        }
        let mut a = Ym2612::new();
        let mut b = Ym2612::new();
        let mut seed = 0x2545_F491u32;
        let mut heard = false;
        for tick in 0..100_000u32 {
            if tick % 7 == 0 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let (port, val) = ((seed >> 8) as u8 & 3, (seed >> 16) as u8);
                // Every other write is a key-on of some channel so the envelopes keep moving.
                let (port, val) = if tick % 2 == 0 && port == 0 { (0, 0x28) } else { (port, val) };
                a.write(port, val);
                b.write(port, val);
            }
            let out = a.tick();
            assert_eq!(out, b.tick());
            heard |= out != (0, 0);
        }
        assert!(heard, "the random program made sound");
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(a.read(), b.read());
    }
}
