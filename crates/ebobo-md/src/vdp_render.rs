//! Draws one screen row: scroll planes A and B, the window plane, sprites, priorities,
//! shadow and highlight, colour output. Owner: planes slice.
//!
//! A pure function of the chip's memory. Mode 5 only (the Master System compatibility mode
//! is not emulated).

use crate::vdp_mem::{Mem, SpriteFlags};

/// Draws screen row `row` into `out` (the first 256 or 320 entries, `0x00RRGGBB`) and returns
/// the flags the row's sprites raise. With `out` = `None` only the flags are worked out, and
/// they are the same. `row` counts from the first displayed line; in interlace mode 2 rows
/// are half lines (0..448 or 0..480).
///
/// The caller handles a blanked display (register 1 bit 6 off: the whole row is the
/// background colour, no sprite flags); this function draws an enabled display.
pub fn render_row(_m: &Mem, _row: u32, _out: Option<&mut [u32]>) -> SpriteFlags {
    todo!("owner: planes slice")
}

/// A 9-bit colour entry (`0000BBB0GGG0RRR0`) as `0x00RRGGBB`, through the console's
/// non-linear output levels. `shade`: 0 normal, 1 shadow, 2 highlight.
pub fn rgb(_entry: u16, _shade: u8) -> u32 {
    todo!("owner: planes slice")
}
