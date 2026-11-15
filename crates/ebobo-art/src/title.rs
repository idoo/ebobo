//! Capturing a game's title screen by running it. Owner: title slice.

use crate::Picture;

/// Runs the ROM without input for up to about 20 emulated seconds and returns the frame that
/// looks most like a title screen, at the console's own resolution: a picture that stands
/// still, is not blank, and has more going on than a line of text on black. `None` for a
/// file that is not a ROM ebobo can run, or a game that shows nothing in that time.
///
/// Deterministic: the same ROM always gives the same picture.
pub fn capture(_rom: &[u8]) -> Option<Picture> {
    todo!("owner: title slice")
}
