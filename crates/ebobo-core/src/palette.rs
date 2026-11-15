//! NES colour palette and emphasis. Owner: bus slice.
//!
//! Base colours: the 2C02G palette shown on the nesdev wiki "PPU palettes" page
//! (https://www.nesdev.org/wiki/PPU_palettes, "2C02G" palette). Emphasis attenuates the
//! non-emphasised channels to ~0.816 (nesdev "Color tint bits"); columns $xE/$xF stay black.

use crate::cart::Region;
use std::sync::LazyLock;

#[rustfmt::skip]
const BASE: [u32; 64] = [
    0x626262, 0x001FB2, 0x2404C8, 0x5200B2, 0x730076, 0x800024, 0x730B00, 0x522800,
    0x244400, 0x005700, 0x005C00, 0x005324, 0x003C76, 0x000000, 0x000000, 0x000000,
    0xABABAB, 0x0D57FF, 0x4B30FF, 0x8A13FF, 0xBC08D6, 0xD21269, 0xC72E00, 0x9D5400,
    0x607B00, 0x209800, 0x00A300, 0x009942, 0x007DB4, 0x000000, 0x000000, 0x000000,
    0xFFFFFF, 0x53AEFF, 0x9085FF, 0xD365FF, 0xFF57FF, 0xFF5DCF, 0xFF7757, 0xFA9E00,
    0xBDC700, 0x7AE700, 0x43F611, 0x26EF7E, 0x2CD5F6, 0x4E4E4E, 0x000000, 0x000000,
    0xFFFFFF, 0xB6E1FF, 0xCED1FF, 0xE9C3FF, 0xFFBCFF, 0xFFBDF4, 0xFFC6C3, 0xFFD59A,
    0xE9E681, 0xCEF481, 0xB6FB9A, 0xA9FAC3, 0xA9F0F4, 0xB8B8B8, 0x000000, 0x000000,
];

static NTSC: LazyLock<Box<[[u8; 4]; 512]>> = LazyLock::new(|| build(false));
static PAL: LazyLock<Box<[[u8; 4]; 512]>> = LazyLock::new(|| build(true));

/// 512 RGBA entries per region (index | emphasis << 6), packed as `[r,g,b,a]`.
fn table(region: Region) -> &'static [[u8; 4]; 512] {
    match region {
        Region::Ntsc => &NTSC,
        Region::Pal => &PAL,
    }
}
fn build(pal: bool) -> Box<[[u8; 4]; 512]> {
    let mut t = Box::new([[0u8; 4]; 512]);
    for (i, e) in t.iter_mut().enumerate() {
        let idx = i & 0x3F;
        let emph = i >> 6;
        // Emphasis bits: bit 0 = red, bit 1 = green, bit 2 = blue (PPUMASK bits 5-7).
        // PAL 2C07 swaps red and green.
        let (r_on, g_on) = if pal { (emph & 2 != 0, emph & 1 != 0) } else { (emph & 1 != 0, emph & 2 != 0) };
        let b_on = emph & 4 != 0;
        let c = BASE[idx];
        let mut rgb = [(c >> 16) as u8, (c >> 8) as u8, c as u8];
        if emph != 0 && idx & 0x0E != 0x0E {
            let on = [r_on, g_on, b_on];
            for (ch, &lit) in rgb.iter_mut().zip(on.iter()) {
                if !lit {
                    *ch = ((*ch as u32 * 816 + 500) / 1000) as u8;
                }
            }
        }
        *e = [rgb[0], rgb[1], rgb[2], 0xFF];
    }
    t
}

/// Converts PPU output (`index | emphasis << 6`) to RGBA8888. PAL swaps the red and green
/// emphasis bits. Bits above 8 (the sprite marker, `ppu::FRAME_SPRITE`) do not affect the colour.
pub fn write_rgba(frame: &[u16], region: Region, out: &mut [u8]) {
    let t = table(region);
    for (px, o) in frame.iter().zip(out.chunks_exact_mut(4)) {
        o.copy_from_slice(&t[(*px & 0x1FF) as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_marker_does_not_change_colour() {
        let words: Vec<u16> = (0..512).collect();
        let marked: Vec<u16> = words.iter().map(|w| w | crate::ppu::FRAME_SPRITE).collect();
        for region in [Region::Ntsc, Region::Pal] {
            let (mut a, mut b) = (vec![0u8; 512 * 4], vec![0u8; 512 * 4]);
            write_rgba(&words, region, &mut a);
            write_rgba(&marked, region, &mut b);
            assert_eq!(a, b);
        }
    }
}
