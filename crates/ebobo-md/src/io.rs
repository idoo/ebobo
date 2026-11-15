//! Controller ports: 3- and 6-button pads, four-player adapters. Owner: periph slice.
//!
//! Contract with `system.rs`: the system hands over the pads of the frame with
//! [`Io::set_pads`] and forwards the 68000's byte accesses to 0xA10001-0xA1001F. Time is
//! master clocks since power-on (the 6-button pad forgets its place after about 1.5 ms
//! without a TH edge).
//!
//! Pins of a port, bit 0 to bit 6 of its data register: D0 (Up), D1 (Down), D2 (Left),
//! D3 (Right), TL, TR, TH. Buttons are active low. A pin that nobody drives is pulled high.

use serde::{Deserialize, Serialize};

use crate::Region;

/// Controller bits of one pad as `Md::run_frame` takes them.
pub mod pad {
    pub const UP: u16 = 0x0001;
    pub const DOWN: u16 = 0x0002;
    pub const LEFT: u16 = 0x0004;
    pub const RIGHT: u16 = 0x0008;
    pub const A: u16 = 0x0010;
    pub const B: u16 = 0x0020;
    pub const C: u16 = 0x0040;
    pub const START: u16 = 0x0080;
    pub const X: u16 = 0x0100;
    pub const Y: u16 = 0x0200;
    pub const Z: u16 = 0x0400;
    pub const MODE: u16 = 0x0800;
}

/// What is plugged into the two ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Multitap {
    /// Pad 1 in port 1, pad 2 in port 2.
    None,
    /// Sega Team Player in port 1 with pads 1-4.
    TeamPlayer,
    /// Electronic Arts 4 Way Play across both ports with pads 1-4.
    EaFourWay,
}

/// Master clocks without a TH edge after which a 6-button pad starts its sequence over:
/// 1.5 ms × 53.693175 MHz.
const SIX_BUTTON_TIMEOUT: u64 = 80_540;

/// TL, TR and TH in a data or control register.
const TL: u8 = 0x10;
const TR: u8 = 0x20;
const TH: u8 = 0x40;

/// All seven pins high: nothing is plugged in.
const UNCONNECTED: u8 = 0x7F;

/// What port 1 of the 4 Way Play shows while port 2 selects no pad (0x7C). plutiedev's detection only needs bits 1-0 low
/// there (and not low with pad 1 selected); the other pins are not documented and are taken as the four direction pins low,
/// which no pad can produce.
const EA_DETECT: u8 = 0x70;

/// Team Player type nibbles.
const TP_THREE_BUTTON: u8 = 0x0;
const TP_SIX_BUTTON: u8 = 0x1;

/// Complete emulated state of the I/O chip and what is plugged in.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Io {
    /// Register 0.
    version: u8,
    /// Data registers of ports 1, 2 and expansion as written.
    data: [u8; 3],
    /// Control registers: bit 0-6 set = that pin is an output, bit 7 = TH interrupt enable.
    ctrl: [u8; 3],
    /// Serial TxData registers.
    tx: [u8; 3],
    /// Serial control registers (the writable bits 3-7).
    sctrl: [u8; 3],
    /// Held buttons of players 1-4.
    pads: [u16; 4],
    tap: Multitap,
    six_button: bool,
    /// TH level the device in port 1 and 2 saw last.
    th: [bool; 2],
    /// 6-button pad: TH falling edges since the sequence started (0-4).
    falls: [u8; 2],
    /// 6-button pad: master clock of the last TH edge.
    last_edge: [u64; 2],
    /// Team Player: TR level seen last.
    tp_tr: bool,
    /// Team Player: TR edges since TH went low.
    tp_step: u8,
}

impl Io {
    /// Power-on state for a console of `region` (the version register tells games), 6-button
    /// pads, no adapter.
    pub fn new(region: Region) -> Io {
        // Bit 7: not Japan. Bit 6: 50 Hz. Bit 5: no expansion unit. Bits 0-3: hardware version 0.
        let version = match region {
            Region::Japan => 0x20,
            Region::Americas => 0xA0,
            Region::Europe => 0xE0,
        };
        Io {
            version,
            data: [0x7F; 3],
            ctrl: [0; 3],
            tx: [0xFF; 3],
            sctrl: [0; 3],
            pads: [0; 4],
            tap: Multitap::None,
            six_button: true,
            th: [true; 2],
            falls: [0; 2],
            last_edge: [0; 2],
            tp_tr: true,
            tp_step: 0,
        }
    }

    /// Buttons held during the coming frame; `pads[i]` is player i+1 (see [`pad`]).
    pub fn set_pads(&mut self, pads: [u16; 4]) {
        self.pads = pads;
    }

    /// Plugs in another adapter; every device starts its protocol over.
    pub fn set_multitap(&mut self, tap: Multitap) {
        self.tap = tap;
        self.falls = [0; 2];
        self.tp_step = 0;
        self.tp_tr = self.line(0, TR);
    }

    pub fn multitap(&self) -> Multitap {
        self.tap
    }

    /// 6-button pads (the default) or plain 3-button ones, for the few games that misread
    /// the newer pad.
    pub fn set_six_button(&mut self, on: bool) {
        self.six_button = on;
        self.falls = [0; 2];
    }

    /// Byte read of register `reg` = 0-15 (address 0xA10001 + 2 × reg): 0 version, 1-3 data
    /// of ports 1, 2 and expansion, 4-6 their control, 7-15 serial.
    pub fn read(&mut self, reg: u32, now: u64) -> u8 {
        match reg & 0xF {
            0 => self.version,
            r @ 1..=3 => {
                let port = r as usize - 1;
                if port < 2 {
                    self.expire(port, now);
                }
                let (data, ctrl) = (self.data[port], self.ctrl[port]);
                // Bit 7 is a latch; an output pin reads back what was written.
                (data & (ctrl | 0x80)) | (self.pins(port) & !ctrl & 0x7F)
            }
            r @ 4..=6 => self.ctrl[r as usize - 4],
            // Serial ports: TxData, RxData, S-Ctrl for each port. Nothing is ever received
            // and the status bits (0-2) stay clear.
            r => {
                let (port, which) = ((r as usize - 7) / 3, (r - 7) % 3);
                match which {
                    0 => self.tx[port],
                    1 => 0,
                    _ => self.sctrl[port],
                }
            }
        }
    }

    pub fn write(&mut self, reg: u32, val: u8, now: u64) {
        match reg & 0xF {
            0 => {}
            r @ 1..=3 => {
                self.data[r as usize - 1] = val;
                self.lines_changed(r as usize - 1, now);
            }
            r @ 4..=6 => {
                self.ctrl[r as usize - 4] = val;
                self.lines_changed(r as usize - 4, now);
            }
            r => {
                let (port, which) = ((r as usize - 7) / 3, (r - 7) % 3);
                match which {
                    0 => self.tx[port] = val,
                    1 => {}
                    _ => self.sctrl[port] = val & 0xF8,
                }
            }
        }
    }

    /// Level of the pin `mask` of `port` as the plugged device sees it: what the console
    /// drives, or high when the pin is an input.
    fn line(&self, port: usize, mask: u8) -> bool {
        self.ctrl[port] & mask == 0 || self.data[port] & mask != 0
    }

    /// The 6-button pad in `port` forgets its place when TH has been still for too long.
    fn expire(&mut self, port: usize, now: u64) {
        if now.saturating_sub(self.last_edge[port]) >= SIX_BUTTON_TIMEOUT {
            self.falls[port] = 0;
        }
    }

    /// The data or control register of `port` was written: the devices follow TH and TR.
    fn lines_changed(&mut self, port: usize, now: u64) {
        if port >= 2 {
            return;
        }
        let th = self.line(port, TH);
        if port == 0 && self.tap == Multitap::TeamPlayer {
            let tr = self.line(0, TR);
            if th || self.th[0] {
                // TH high, or just fallen: the start of a read.
                self.tp_step = 0;
            } else if tr != self.tp_tr {
                self.tp_step = self.tp_step.saturating_add(1);
            }
            self.tp_tr = tr;
        }
        if th != self.th[port] {
            self.expire(port, now);
            self.th[port] = th;
            self.last_edge[port] = now;
            if !th {
                self.falls[port] = (self.falls[port] + 1).min(4);
            } else if self.falls[port] == 4 {
                self.falls[port] = 0;
            }
        }
    }

    /// The seven pins of `port` as the plugged device drives them.
    fn pins(&self, port: usize) -> u8 {
        match (self.tap, port) {
            (Multitap::None, 0 | 1) => pad_pins(self.pads[port], self.th[port], self.six_falls(port)),
            (Multitap::TeamPlayer, 0) => self.team_player_pins(),
            (Multitap::EaFourWay, 0) => {
                // Port 2's TH, TR and TL pick the pad. Port 1's TH reaches all four pads, so their 6-button
                // counts move together.
                let select = (self.line(1, TH) as usize) << 2 | (self.line(1, TR) as usize) << 1 | self.line(1, TL) as usize;
                match select {
                    0..=3 => pad_pins(self.pads[select], self.th[0], self.six_falls(0)),
                    7 => EA_DETECT,
                    _ => UNCONNECTED,
                }
            }
            _ => UNCONNECTED,
        }
    }

    /// TH falls the 6-button pads behind `port` have counted, 0 when they are 3-button pads.
    fn six_falls(&self, port: usize) -> u8 {
        if self.six_button { self.falls[port] } else { 0 }
    }

    /// Team Player: TH low starts a read, every TR edge steps to the next nibble on D0-D3 and
    /// TL answers by following TR.
    fn team_player_pins(&self) -> u8 {
        let (th, tr) = (self.th[0], self.line(0, TR));
        let nibble = if th {
            0x3
        } else {
            match self.tp_step {
                0 => 0xF,
                1 | 2 => 0x0,
                3..=6 => self.team_player_type(),
                step => self.team_player_data(step as usize - 7),
            }
        };
        (th as u8) << 6 | (tr as u8) << 5 | (tr as u8) << 4 | nibble
    }

    /// Type nibble of a Team Player port: all four hold the same kind of pad.
    fn team_player_type(&self) -> u8 {
        if self.six_button { TP_SIX_BUTTON } else { TP_THREE_BUTTON }
    }

    /// Button nibble `index` of a Team Player read: for each pad in turn Right/Left/Down/Up,
    /// Start/A/C/B and, for a 6-button pad, Mode/X/Y/Z. Past the last one the pins stay high.
    fn team_player_data(&self, index: usize) -> u8 {
        let per_pad = if self.six_button { 3 } else { 2 };
        let Some(&buttons) = self.pads.get(index / per_pad) else {
            return 0xF;
        };
        let up = !buttons;
        (match index % per_pad {
            0 => up & 0xF,
            1 => (up >> 5 & 1) | (up >> 6 & 1) << 1 | (up >> 4 & 1) << 2 | (up >> 7 & 1) << 3,
            _ => extra_nibble(up),
        }) as u8
    }
}

/// Z, Y, X, Mode on bits 0-3 from the inverted button mask `up`.
fn extra_nibble(up: u16) -> u16 {
    (up >> 10 & 1) | (up >> 9 & 1) << 1 | (up >> 8 & 1) << 2 | (up >> 11 & 1) << 3
}

/// Pins of a pad holding `buttons` with TH at `th`, after `falls` TH falling edges of a
/// 6-button sequence (0 for a 3-button pad).
fn pad_pins(buttons: u16, th: bool, falls: u8) -> u8 {
    let up = !buttons;
    let pins = if th {
        // C, B and either the directions or, after the third TH low, Mode, X, Y, Z.
        let low = if falls == 3 { extra_nibble(up) } else { up & 0xF };
        TH as u16 | (up >> 6 & 1) << 5 | (up >> 5 & 1) << 4 | low
    } else {
        // Start, A and Down, Up over two grounded pins; the third TH low grounds all four,
        // the fourth leaves all four high.
        let low = match falls {
            3 => 0x0,
            4 => 0xF,
            _ => up & 0x3,
        };
        (up >> 7 & 1) << 5 | (up >> 4 & 1) << 4 | low
    };
    pins as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA1: u32 = 1;
    const DATA2: u32 = 2;
    const DATA3: u32 = 3;
    const CTRL1: u32 = 4;
    const CTRL2: u32 = 5;

    /// Writes `val` to port 1's data register at `now` and reads it back.
    fn step(io: &mut Io, val: u8, now: u64) -> u8 {
        io.write(DATA1, val, now);
        io.read(DATA1, now)
    }

    #[test]
    fn three_button_pad_sequence() {
        let mut io = Io::new(Region::Americas);
        io.set_six_button(false);
        io.set_pads([pad::UP | pad::RIGHT | pad::A | pad::C, pad::DOWN | pad::LEFT | pad::B | pad::START, 0, 0]);
        io.write(CTRL1, 0x40, 0);
        io.write(CTRL2, 0x40, 0);
        // TH high: C B Right Left Down Up.
        assert_eq!(step(&mut io, 0x40, 10), 0x40 | 0b01_0110);
        // TH low: Start A 0 0 Down Up.
        assert_eq!(step(&mut io, 0x00, 20), 0b10_0010);
        // Any number of pulses reads the same on a 3-button pad.
        for i in 0..6 {
            assert_eq!(step(&mut io, 0x40, 30 + i * 20), 0x40 | 0b01_0110);
            assert_eq!(step(&mut io, 0x00, 40 + i * 20), 0b10_0010);
        }
        io.write(DATA2, 0x40, 200);
        assert_eq!(io.read(DATA2, 200), 0x40 | 0b10_1001);
        io.write(DATA2, 0x00, 210);
        assert_eq!(io.read(DATA2, 210), 0b01_0001);
    }

    #[test]
    fn six_button_pad_sequence() {
        let mut io = Io::new(Region::Americas);
        io.set_pads([pad::UP | pad::B | pad::START | pad::X | pad::MODE, 0, 0, 0]);
        io.write(CTRL1, 0x40, 0);
        let high = 0x40 | 0b10_1110; // C up, B held, Right Left Down up, Up held
        let low = 0b01_0010; // Start held, A up, 0 0, Down up, Up held
        for round in 0..2u64 {
            let t = round * 1000;
            assert_eq!(step(&mut io, 0x40, t + 10), high);
            assert_eq!(step(&mut io, 0x00, t + 20), low);
            assert_eq!(step(&mut io, 0x40, t + 30), high);
            assert_eq!(step(&mut io, 0x00, t + 40), low);
            assert_eq!(step(&mut io, 0x40, t + 50), high);
            // Third TH low: 0000.
            assert_eq!(step(&mut io, 0x00, t + 60), 0b01_0000);
            // Mode X Y Z: Mode and X held.
            assert_eq!(step(&mut io, 0x40, t + 70), 0x40 | 0b10_0011);
            // Fourth TH low: 1111.
            assert_eq!(step(&mut io, 0x00, t + 80), 0b01_1111);
        }
        assert_eq!(step(&mut io, 0x40, 3000), high);
    }

    #[test]
    fn six_button_pad_times_out() {
        let mut io = Io::new(Region::Americas);
        io.write(CTRL1, 0x40, 0);
        let mut t = 100;
        // Two pulses, then a third in time: the pad answers 0000.
        for _ in 0..2 {
            step(&mut io, 0x00, t);
            step(&mut io, 0x40, t + 10);
            t += 20;
        }
        assert_eq!(step(&mut io, 0x00, t + SIX_BUTTON_TIMEOUT - 11), 0x30);
        // Finish the sequence.
        step(&mut io, 0x40, t + SIX_BUTTON_TIMEOUT);
        step(&mut io, 0x00, t + SIX_BUTTON_TIMEOUT + 10);
        step(&mut io, 0x40, t + SIX_BUTTON_TIMEOUT + 20);
        // Two pulses, a pause of 1.5 ms, then the third: the count started over.
        let mut t = 1_000_000;
        for _ in 0..2 {
            step(&mut io, 0x00, t);
            step(&mut io, 0x40, t + 10);
            t += 20;
        }
        let late = t - 10 + SIX_BUTTON_TIMEOUT;
        assert_eq!(step(&mut io, 0x00, late), 0x33);
        assert_eq!(step(&mut io, 0x40, late + 10), 0x7F);
        assert_eq!(step(&mut io, 0x00, late + 20), 0x33);
        assert_eq!(step(&mut io, 0x40, late + 30), 0x7F);
        assert_eq!(step(&mut io, 0x00, late + 40), 0x30);
    }

    #[test]
    fn output_pins_read_back() {
        let mut io = Io::new(Region::Americas);
        io.set_six_button(false);
        // Power-on: all inputs, TH pulled high, nothing held.
        assert_eq!(io.read(DATA1, 0), 0x7F);
        assert_eq!(io.read(CTRL1, 0), 0x00);
        io.write(CTRL1, 0x7F, 0);
        assert_eq!(io.read(CTRL1, 0), 0x7F);
        assert_eq!(step(&mut io, 0x55, 0), 0x55);
        assert_eq!(step(&mut io, 0x2A, 0), 0x2A);
        // Bit 7 is a latch whatever the control register says.
        assert_eq!(step(&mut io, 0xD5, 0), 0xD5);
        io.write(CTRL1, 0x00, 0);
        assert_eq!(io.read(DATA1, 0), 0xFF);
        // One output among inputs: Up reads the written 0, the rest comes from the pad.
        io.write(CTRL1, 0x01, 0);
        io.set_pads([pad::B, 0, 0, 0]);
        assert_eq!(step(&mut io, 0x00, 0), 0x6E);
        // The expansion port: nothing plugged in.
        assert_eq!(io.read(DATA3, 0), 0x7F);
    }

    #[test]
    fn version_register() {
        assert_eq!(Io::new(Region::Japan).read(0, 0), 0x20);
        assert_eq!(Io::new(Region::Americas).read(0, 0), 0xA0);
        assert_eq!(Io::new(Region::Europe).read(0, 0), 0xE0);
        let mut io = Io::new(Region::Europe);
        io.write(0, 0x00, 0);
        assert_eq!(io.read(0, 0), 0xE0);
    }

    #[test]
    fn serial_registers_hold_values() {
        let mut io = Io::new(Region::Japan);
        for port in 0..3u32 {
            let (tx, rx, sctrl) = (7 + port * 3, 8 + port * 3, 9 + port * 3);
            assert_eq!(io.read(tx, 0), 0xFF);
            io.write(tx, 0x12 + port as u8, 0);
            io.write(rx, 0x99, 0);
            io.write(sctrl, 0xFF, 0);
            assert_eq!(io.read(tx, 0), 0x12 + port as u8);
            assert_eq!(io.read(rx, 0), 0x00);
            assert_eq!(io.read(sctrl, 0), 0xF8);
        }
    }

    /// One Team Player read: the nibbles after the identification, TL checked against TR.
    fn team_player_read(io: &mut Io, count: usize) -> Vec<u8> {
        io.write(CTRL1, 0x60, 0);
        assert_eq!(step(io, 0x60, 0), 0x73);
        assert_eq!(step(io, 0x20, 0), 0x3F);
        assert_eq!(step(io, 0x00, 0), 0x00);
        assert_eq!(step(io, 0x20, 0), 0x30);
        let mut out = Vec::new();
        for i in 0..count {
            let tr = if i % 2 == 0 { 0x00 } else { 0x20 };
            let got = step(io, tr, 0);
            assert_eq!(got & 0x70, tr | tr >> 1, "TL follows TR at nibble {i}");
            out.push(got & 0xF);
        }
        assert_eq!(step(io, 0x60, 0), 0x73);
        out
    }

    #[test]
    fn team_player_reads_four_pads() {
        let mut io = Io::new(Region::Americas);
        io.set_multitap(Multitap::TeamPlayer);
        assert_eq!(io.multitap(), Multitap::TeamPlayer);
        io.set_pads([
            pad::UP | pad::B | pad::Z,
            pad::DOWN | pad::LEFT | pad::C | pad::START | pad::Y | pad::MODE,
            pad::RIGHT | pad::A | pad::X,
            0,
        ]);
        let expected = [
            1, 1, 1, 1, // four 6-button pads
            0b1110, 0b1110, 0b1110, // pad 1: Up; B; Z
            0b1001, 0b0101, 0b0101, // pad 2: Down Left; C Start; Y Mode
            0b0111, 0b1011, 0b1011, // pad 3: Right; A; X
            0b1111, 0b1111, 0b1111, // pad 4: nothing
        ];
        assert_eq!(team_player_read(&mut io, expected.len()), expected);
        // A second read starts over and gives the same.
        assert_eq!(team_player_read(&mut io, expected.len()), expected);
        // Port 2 is empty.
        assert_eq!(io.read(DATA2, 0), 0x7F);

        io.set_six_button(false);
        let expected = [
            0, 0, 0, 0, // four 3-button pads
            0b1110, 0b1110, // pad 1
            0b1001, 0b0101, // pad 2
            0b0111, 0b1011, // pad 3
            0b1111, 0b1111, // pad 4
        ];
        assert_eq!(team_player_read(&mut io, expected.len()), expected);
    }

    #[test]
    fn four_way_play_selects_pads() {
        let mut io = Io::new(Region::Americas);
        io.set_multitap(Multitap::EaFourWay);
        io.set_pads([pad::UP | pad::A, pad::DOWN | pad::B, pad::LEFT | pad::C | pad::X | pad::MODE, pad::RIGHT | pad::START]);
        io.write(CTRL1, 0x40, 0);
        io.write(CTRL2, 0x7F, 0);
        // No pad selected: the adapter's signature.
        io.write(DATA2, 0x7C, 0);
        assert_eq!(step(&mut io, 0x40, 0), 0x40 | EA_DETECT & 0x3F);
        assert_eq!(io.read(DATA1, 0) & 0x03, 0, "plutiedev: bits 1-0 read 00 on the ID select");
        assert_eq!(io.read(DATA2, 0), 0x7C);
        let high = [0x40 | 0b11_1110, 0x40 | 0b10_1101, 0x40 | 0b01_1011, 0x40 | 0b11_0111];
        let low = [0b10_0010, 0b11_0001, 0b11_0011, 0b01_0011];
        io.set_six_button(false);
        for player in 0..4 {
            io.write(DATA2, (player as u8) << 4 | 0x0C, 0);
            // 3-button pads: many pulses never show X, Y, Z, Mode.
            for _ in 0..5 {
                assert_eq!(step(&mut io, 0x40, 0), high[player], "pad {player} TH high");
                assert_eq!(step(&mut io, 0x00, 0), low[player], "pad {player} TH low");
            }
        }
        // 6-button pads share port 1's TH: read all four at each step, as plutiedev's loop does.
        io.set_six_button(true);
        let third_low = [0x20, 0x30, 0x30, 0x10];
        let extra_high = [0x7F, 0x6F, 0x53, 0x7F];
        for (i, th) in [0x40u8, 0x00, 0x40, 0x00, 0x40, 0x00, 0x40, 0x00].into_iter().enumerate() {
            io.write(DATA1, th, 0);
            for player in 0..4 {
                io.write(DATA2, (player as u8) << 4 | 0x0C, 0);
                let want = match i {
                    5 => third_low[player],
                    6 => extra_high[player],
                    7 => low[player] | 0x0F,
                    _ if th != 0 => high[player],
                    _ => low[player],
                };
                assert_eq!(io.read(DATA1, 0), want, "pad {player} step {i}");
            }
        }
    }

    #[test]
    fn state_is_plain_data() {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let hash = |io: &Io| {
            let mut h = DefaultHasher::new();
            io.hash(&mut h);
            h.finish()
        };
        let mut io = Io::new(Region::Japan);
        let before = io.clone();
        assert_eq!(hash(&io), hash(&before));
        io.set_pads([pad::START, 0, 0, 0]);
        assert_ne!(hash(&io), hash(&before));
    }
}
