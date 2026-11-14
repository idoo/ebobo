//! Controller ports: 3- and 6-button pads, four-player adapters. Owner: periph slice.
//!
//! Contract with `system.rs`: the system hands over the pads of the frame with
//! [`Io::set_pads`] and forwards the 68000's byte accesses to 0xA10001-0xA1001F. Time is
//! master clocks since power-on (the 6-button pad forgets its place after about 1.5 ms
//! without a TH edge).

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

/// Complete emulated state of the I/O chip and what is plugged in.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Io {
    _todo: (),
}

impl Io {
    /// Power-on state for a console of `region` (the version register tells games), 6-button
    /// pads, no adapter.
    pub fn new(_region: Region) -> Io {
        todo!("owner: periph slice")
    }

    /// Buttons held during the coming frame; `pads[i]` is player i+1 (see [`pad`]).
    pub fn set_pads(&mut self, _pads: [u16; 4]) {
        todo!("owner: periph slice")
    }

    pub fn set_multitap(&mut self, _tap: Multitap) {
        todo!("owner: periph slice")
    }

    pub fn multitap(&self) -> Multitap {
        todo!("owner: periph slice")
    }

    /// 6-button pads (the default) or plain 3-button ones, for the few games that misread
    /// the newer pad.
    pub fn set_six_button(&mut self, _on: bool) {
        todo!("owner: periph slice")
    }

    /// Byte read of register `reg` = 0-15 (address 0xA10001 + 2 × reg): 0 version, 1-3 data
    /// of ports 1, 2 and expansion, 4-6 their control, 7-15 serial.
    pub fn read(&mut self, _reg: u32, _now: u64) -> u8 {
        todo!("owner: periph slice")
    }

    pub fn write(&mut self, _reg: u32, _val: u8, _now: u64) {
        todo!("owner: periph slice")
    }
}
