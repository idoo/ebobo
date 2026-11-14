//! Sprites of one screen row. Owner: sprites slice.
//!
//! A pure function of the chip's memory: no state of its own, so the caller can skip the
//! pixels (fast-forward) and still get the same flags.

use crate::vdp_mem::{Mem, SpriteFlags, SpriteRow};

/// Fills `row_out` with the sprite pixels of screen row `row` and returns the flags the row
/// raises. `row` counts from the first displayed line; in interlace mode 2 rows are half
/// lines (0..448 or 0..480) and patterns are 16 rows tall.
///
/// As the chip does: the sprite list is walked by its link fields from sprite 0 (at most 80
/// entries in 320-pixel mode, 64 in 256-pixel mode); the first 20 (16) sprites on the row are
/// drawn, up to 320 (256) sprite pixels; a sprite at x = 0 masks the lower-priority ones on
/// the row under the chip's rules; earlier sprites in the list are in front.
pub fn sprite_row(_m: &Mem, _row: u32, _row_out: &mut SpriteRow) -> SpriteFlags {
    todo!("owner: sprites slice")
}
