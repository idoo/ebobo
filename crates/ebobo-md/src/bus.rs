//! Everything on the two buses: both CPUs' memory maps, the devices behind them, and the
//! stepping of each CPU against the master clock. Owner: bus slice.
//!
//! Contract with `system.rs` (which owns the frame loop, snapshots and the public `Md`):
//! - [`State`] is the whole emulated state, plain data; `system.rs` clones, hashes and
//!   serializes it. Nothing else in [`Machine`] feeds back into it.
//! - Time is master clocks since power-on. `State::now` is where the 68000 is; an
//!   instruction's bus accesses all happen at the `now` it started at.
//! - 68000 map: 0x000000-0x3FFFFF cartridge; 0xA00000-0xA0FFFF the Z80's address space
//!   (its RAM, the YM2612, the bank register), reachable while the 68000 holds the Z80's
//!   bus; 0xA10000-0xA1001F controller ports; 0xA11100 Z80 bus request (bit 8; reads give
//!   bit 8 = 0 once the bus is granted); 0xA11200 Z80 reset (bit 8 = 0 holds the Z80 and the
//!   YM2612 in reset); 0xA13000-0xA130FF cartridge registers; 0xC00000-0xC0001F VDP ports
//!   (PSG writes at 0xC00011); 0xE00000-0xFFFFFF 64 KB RAM, mirrored. Unmapped reads give
//!   0xFF bytes.
//! - Z80 map: 0x0000-0x1FFF 8 KB RAM (mirrored to 0x3FFF); 0x4000-0x5FFF YM2612 (ports 0-3
//!   mirrored); 0x6000-0x60FF bank register (a write shifts bit 0 of the value in from the
//!   top of 9 bits); 0x7F00-0x7F1F VDP ports (PSG writes at 0x7F11); 0x8000-0xFFFF a 32 KB
//!   window of 68000 memory picked by the bank register.
//! - Before any device access the device is brought to the current time: `Vdp::run_to`,
//!   the sound chips through [`Machine::sync_sound`].

use serde::{Deserialize, Serialize};

use crate::audio::AudioOut;
use crate::cart::{Cart, CartState};
use crate::io::Io;
use crate::psg::Psg;
use crate::vdp::{Screen, Vdp};
use crate::ym2612::Ym2612;

/// The whole emulated state of the console.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub(crate) struct State {
    pub m68k: ebobo_m68k::Cpu,
    pub z80: ebobo_z80::Cpu,
    /// 64 KB work RAM.
    pub ram: Vec<u8>,
    /// 8 KB Z80 RAM.
    pub zram: Vec<u8>,
    pub vdp: Vdp,
    pub ym: Ym2612,
    pub psg: Psg,
    pub io: Io,
    pub cart: CartState,
    /// Master clock the 68000 has run to.
    pub now: u64,
    /// Master clock the Z80 has run to.
    pub z80_now: u64,
    /// Master clocks at which the next FM and PSG samples are due.
    pub fm_next: u64,
    pub psg_next: u64,
    /// The 68000 asked for the Z80's bus (0xA11100): the Z80 stands still.
    pub z80_busreq: bool,
    /// The Z80's reset line is released (0xA11200). While it is held the Z80 does not run
    /// and the YM2612 is in reset.
    pub z80_run: bool,
    /// The Z80's 9-bit bank register: bits 15-23 of the 68000 address its window shows.
    pub z80_bank: u32,
}

/// The console: emulated state plus what is not state (the cartridge as built, the picture,
/// the audio output).
pub(crate) struct Machine {
    pub s: State,
    pub cart: Cart,
    pub screen: Screen,
    pub audio: AudioOut,
    /// False in fast-forward: no pixels are drawn and the audio output is muted. Emulated
    /// state is the same either way.
    pub output: bool,
}

impl Machine {
    /// Power-on with `cart` in the slot: memories cleared, both CPUs reset (the 68000 loads
    /// its stack pointer and PC from the cartridge; `tas_no_write` on), the Z80 held in
    /// reset, the console's region and video standard from `cart.info().region`.
    pub(crate) fn new(_cart: Cart) -> Machine {
        todo!("owner: bus slice")
    }

    /// Reset button: both CPUs and the YM2612 reset, RAM and the VDP keep their contents.
    pub(crate) fn reset(&mut self) {
        todo!("owner: bus slice")
    }

    /// One 68000 instruction or interrupt at `s.now`, with the VDP's interrupt level on its
    /// pins (the acknowledge clears the VDP's request); advances `s.now` by its length.
    /// While a DMA holds the bus, moves `s.now` to the end of the hold instead.
    pub(crate) fn step_m68k(&mut self) {
        todo!("owner: bus slice")
    }

    /// Runs the Z80 from `s.z80_now` up to `s.now`, its /INT line following the VDP; while
    /// it is in reset or its bus is taken, time just passes.
    pub(crate) fn run_z80(&mut self) {
        todo!("owner: bus slice")
    }

    /// Brings the VDP to `s.now`, drawing into the screen when `output` is on.
    pub(crate) fn sync_vdp(&mut self) {
        todo!("owner: bus slice")
    }

    /// Runs the YM2612 and the PSG up to `s.now`, one sample period at a time in time order,
    /// feeding the audio output.
    pub(crate) fn sync_sound(&mut self) {
        todo!("owner: bus slice")
    }
}
