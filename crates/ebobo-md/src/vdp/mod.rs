//! Video chip (315-5313): ports, DMA, interrupts, HV counter, line renderer. Owner: vdp slice.
//!
//! Contract with `system.rs`:
//! - Time is master clocks since power-on (`u64`), never going backwards. The VDP keeps its
//!   own position (line, clock within the line) and derives everything from it.
//! - The caller brings the chip up to date with [`Vdp::run_to`] before every [`Vdp::read`]
//!   and [`Vdp::write`], and before it looks at [`Vdp::irq_level`], [`Vdp::z80_irq`] or
//!   [`Vdp::take_frame_done`].
//! - A line is drawn from the state the chip has at one fixed point of that line, so a
//!   register or memory write lands between two lines.
//! - Pixels go to a [`Screen`], which is not emulated state. With `None` nothing is drawn,
//!   and every bit of emulated state (the sprite overflow and collision flags included)
//!   comes out the same.

use serde::{Deserialize, Serialize};

/// The picture: `w`×`h` pixels, `0x00RRGGBB`, row by row. Not part of the emulated state.
pub struct Screen {
    /// 256 or 320.
    pub w: usize,
    /// 224 or 240, doubled in interlace mode 2.
    pub h: usize,
    /// `MAX_W * MAX_H` entries; the picture is the top-left `w`×`h`, rows `MAX_W` apart.
    pub px: Vec<u32>,
}

impl Screen {
    pub fn new() -> Screen {
        Screen { w: 320, h: 224, px: vec![0; crate::MAX_W * crate::MAX_H] }
    }
}

impl Default for Screen {
    fn default() -> Screen {
        Screen::new()
    }
}

/// 68000 memory as a 68k→VDP DMA reads it.
pub trait DmaSource {
    /// `addr` is an even 24-bit 68000 address.
    fn dma_read16(&mut self, addr: u32) -> u16;
}

/// Complete emulated state of the chip.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct Vdp {
    _todo: (),
}

impl Vdp {
    /// Power-on state. `pal`: 313 lines at 50 Hz instead of 262 at 60 Hz.
    pub fn new(_pal: bool) -> Vdp {
        todo!("owner: vdp slice")
    }

    /// Advances to master clock `now`: finishes lines (drawing each into `screen`), raises
    /// interrupt requests, moves the HV counter and the status flags.
    pub fn run_to(&mut self, _now: u64, _screen: Option<&mut Screen>) {
        todo!("owner: vdp slice")
    }

    /// The earliest master clock after the chip's position at which [`Vdp::irq_level`],
    /// [`Vdp::z80_irq`] or the frame-done flag changes if nothing is written meanwhile.
    pub fn next_event(&self) -> u64 {
        todo!("owner: vdp slice")
    }

    /// True once per frame, when the last displayed line is finished (vertical blanking
    /// starts): the picture in the screen is complete. Reading clears it.
    pub fn take_frame_done(&mut self) -> bool {
        todo!("owner: vdp slice")
    }

    /// 68000 interrupt level requested now: 6 (vertical), 4 (horizontal) or 0. Follows the
    /// enable bits, so a write that enables a pending interrupt raises it at once.
    pub fn irq_level(&self) -> u8 {
        todo!("owner: vdp slice")
    }

    /// The 68000 acknowledged `level`: that request is cleared.
    pub fn irq_ack(&mut self, _level: u8) {
        todo!("owner: vdp slice")
    }

    /// The Z80's /INT line is low (one line long, starting with the vertical interrupt;
    /// not affected by the enable bits).
    pub fn z80_irq(&self) -> bool {
        todo!("owner: vdp slice")
    }

    /// A 68k→VDP DMA holds the 68000 off the bus until this master clock (at or before the
    /// chip's position when it does not).
    pub fn m68k_frozen_until(&self) -> u64 {
        todo!("owner: vdp slice")
    }

    /// Word read of a port, `addr` = 0x00-0x1E even (the low bits of 0xC000xx): 0x00/0x02
    /// data, 0x04/0x06 status, 0x08-0x0E HV counter. Other addresses give 0xFFFF.
    pub fn read(&mut self, _addr: u32, _now: u64) -> u16 {
        todo!("owner: vdp slice")
    }

    /// Word write of a port (data or control). A control write that starts a 68k→VDP DMA
    /// reads its source through `mem`; the transfer happens here and
    /// [`Vdp::m68k_frozen_until`] reports how long it takes.
    pub fn write(&mut self, _addr: u32, _val: u16, _now: u64, _mem: &mut dyn DmaSource) {
        todo!("owner: vdp slice")
    }

    /// Lines per frame: 262 or 313.
    pub fn lines(&self) -> u32 {
        todo!("owner: vdp slice")
    }

    /// Debugging and tests.
    pub fn vram(&self) -> &[u8] {
        todo!("owner: vdp slice")
    }

    /// 64 colour entries, 9 bits each (`0000BBB0GGG0RRR0`).
    pub fn cram(&self) -> &[u16] {
        todo!("owner: vdp slice")
    }

    /// Register 0-23.
    pub fn reg(&self, _index: usize) -> u8 {
        todo!("owner: vdp slice")
    }
}
