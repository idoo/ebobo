//! PNG in and out, and shrinking. Owner: image slice.

use crate::Picture;

/// Decodes a PNG of any colour type and bit depth into RGBA8888. Refuses files whose header
/// claims more than 4096 pixels a side, before allocating for them.
pub fn decode_png(_bytes: &[u8]) -> Result<Picture, String> {
    todo!("owner: image slice")
}

/// Encodes as a PNG (for the disk cache).
pub fn encode_png(_picture: &Picture) -> Vec<u8> {
    todo!("owner: image slice")
}

/// Shrinks `picture` to fit `max_w`×`max_h`, keeping its proportions, averaging the source
/// pixels each result pixel covers (alpha-weighted). A picture that already fits is returned
/// as it is; nothing is ever enlarged.
pub fn fit(_picture: Picture, _max_w: u32, _max_h: u32) -> Picture {
    todo!("owner: image slice")
}
