//! Types shared by the video chip (`vdp.rs`) and the row renderer (`vdp_render.rs`,
//! `vdp_sprites.rs`). Owner: the lead; ask before changing.

/// What a screen row is drawn from: the chip's registers and memories at that moment.
#[derive(Clone, Copy)]
pub struct Mem<'a> {
    /// Registers 0-23 as written.
    pub regs: &'a [u8; 24],
    /// 65536 bytes of video RAM.
    pub vram: &'a [u8],
    /// 64 colours (4 palette lines of 16), 9 bits each as written: `0000BBB0GGG0RRR0`.
    pub cram: &'a [u16; 64],
    /// 40 vertical scroll words: even entries plane A, odd entries plane B, one pair per
    /// two-cell column (entries 0 and 1 for the whole plane in full-screen scroll mode).
    pub vsram: &'a [u16; 40],
}

/// What a row's sprites raise in the status register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpriteFlags {
    /// More sprites or sprite pixels on the row than the chip draws.
    pub overflow: bool,
    /// Two non-transparent sprite pixels met.
    pub collision: bool,
}

/// One row of sprite pixels, one byte per screen column (the first 256 or 320 are used):
/// 0 = no sprite pixel; otherwise bits 0-3 the colour (1-15), bits 4-5 the palette line,
/// bit 7 the priority flag.
pub type SpriteRow = [u8; 320];
