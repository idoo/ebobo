//! ebobo-md: deterministic Sega Mega Drive / Genesis emulator (68000 + Z80, VDP, YM2612 + PSG).
//!
//! Determinism contract (rollback netplay depends on it): the same ROM bytes, region,
//! multitap setting and sequence of `run_frame` inputs produce bit-identical state on every
//! machine. No clocks, no randomness, no floats in emulated state, lookup tables included
//! (literal constants or integer arithmetic, never `sin`/`pow` at run time: libm differs
//! between platforms). Floats live only in the audio output path ([`audio::AudioOut`]), which
//! is outside the snapshot, as is the picture ([`vdp::Screen`]). Emulated state never depends
//! on whether pixels or sound are produced (fast-forward re-simulation skips both).
//!
//! Time is counted in master clocks (`u64`, since power-on). One line is 3420 of them; the
//! 68000 runs at 1/7, the Z80 at 1/15.
//!
//! Module ownership (one owner per file):
//! - `vdp.rs`, `vdp_timing.rs`: video chip state, ports, DMA, interrupts, HV counter.
//! - `vdp_render.rs`, `vdp_sprites.rs`: drawing one screen row from the chip's memory.
//! - `vdp_mem.rs`: the types those four share.
//! - `ym2612.rs`: FM chip. `psg.rs`, `audio.rs`: PSG chip, mixing and resampling.
//! - `cart.rs`: ROM image, header, battery RAM, bank switching. `io.rs`: controller ports.
//! - `system.rs`: memory maps of both CPUs, scheduler, the public [`Md`] machine.
//!
//! Written from published documentation and hardware research, not from another emulator's
//! code.

use serde::{Deserialize, Serialize};

#[cfg(feature = "psg")]
pub mod audio;
#[cfg(feature = "cart")]
pub mod cart;
#[cfg(feature = "io")]
pub mod io;
#[cfg(feature = "psg")]
pub mod psg;
#[cfg(feature = "system")]
mod system;
#[cfg(feature = "vdp")]
pub mod vdp;
pub mod vdp_mem;
#[cfg(feature = "vdp-render")]
pub mod vdp_render;
#[cfg(feature = "vdp-sprites")]
pub mod vdp_sprites;
#[cfg(feature = "vdp")]
mod vdp_timing;
#[cfg(feature = "fm")]
pub mod ym2612;

#[cfg(feature = "cart")]
pub use cart::{RomError, RomInfo};
#[cfg(feature = "io")]
pub use io::pad;
#[cfg(feature = "system")]
pub use system::{Md, Snapshot, StateError};

/// Console region. `Europe` is the 50 Hz PAL machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Region {
    Japan,
    Americas,
    Europe,
}

/// Largest picture: 320 pixels wide, 240 lines, doubled in interlace mode 2.
pub const MAX_W: usize = 320;
pub const MAX_H: usize = 480;

/// The library lists Mega Drive games only when this is true: set once games boot and play.
pub const PLAYABLE: bool = false;

/// Master clock in Hz.
pub const MCLK_NTSC: u32 = 53_693_175;
pub const MCLK_PAL: u32 = 53_203_424;
/// Master clocks per line (both regions, both widths).
pub const MCLK_PER_LINE: u64 = 3420;
/// Master clocks per 68000 cycle and per Z80 T-state.
pub const M68K_DIV: u64 = 7;
pub const Z80_DIV: u64 = 15;
/// Master clocks per YM2612 output sample (the chip runs at the 68000 clock; one sample is
/// 144 of its cycles) and per PSG sample (Z80 clock / 16).
pub const FM_SAMPLE_MCLK: u64 = 7 * 144;
pub const PSG_SAMPLE_MCLK: u64 = 15 * 16;
