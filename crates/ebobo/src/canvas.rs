//! 256x240 RGBA drawing surface and the pixel font. Owner: canvas slice.

use std::collections::HashMap;

pub const W: usize = 256;
pub const H: usize = 240;

/// RGBA8888.
pub type Color = [u8; 4];

/// The frame the window shows: game picture first, UI drawn on top.
pub struct Canvas {
    /// `W * H * 4` bytes.
    pub px: Vec<u8>,
}

impl Canvas {
    pub fn new() -> Canvas {
        Canvas { px: vec![0; W * H * 4] }
    }

    pub fn clear(&mut self, color: Color) {
        for p in self.px.chunks_exact_mut(4) {
            p.copy_from_slice(&color);
        }
    }

    /// Darkens everything by `amount` (0.0-1.0), for menus over the game.
    pub fn dim(&mut self, amount: f32) {
        let keep = ((1.0 - amount.clamp(0.0, 1.0)) * 256.0).round() as u32;
        for p in self.px.chunks_exact_mut(4) {
            for c in &mut p[..3] {
                *c = ((*c as u32 * keep + 128) >> 8) as u8;
            }
        }
    }

    /// Alpha-blended filled rectangle, clipped to the canvas.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = x.saturating_add(w).min(W as i32);
        let y1 = y.saturating_add(h).min(H as i32);
        if x0 >= x1 || y0 >= y1 || color[3] == 0 {
            return;
        }
        let (x0, x1) = (x0 as usize, x1 as usize);
        let a = color[3] as u32;
        for row in y0 as usize..y1 as usize {
            let line = &mut self.px[(row * W + x0) * 4..(row * W + x1) * 4];
            if a == 255 {
                for p in line.chunks_exact_mut(4) {
                    p.copy_from_slice(&color);
                }
            } else {
                let inv = 255 - a;
                for p in line.chunks_exact_mut(4) {
                    for i in 0..3 {
                        p[i] = ((color[i] as u32 * a + p[i] as u32 * inv + 127) / 255) as u8;
                    }
                    p[3] = (a + (p[3] as u32 * inv + 127) / 255) as u8;
                }
            }
        }
    }

    /// 1-px outline, clipped.
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.fill_rect(x, y, w, 1, color);
        if h > 1 {
            self.fill_rect(x, y + h - 1, w, 1, color);
        }
        if h > 2 {
            self.fill_rect(x, y + 1, 1, h - 2, color);
            if w > 1 {
                self.fill_rect(x + w - 1, y + 1, 1, h - 2, color);
            }
        }
    }
}

const FONT_TTF: &[u8] = include_bytes!("../assets/PressStart2P-Regular.ttf");
/// Pixel size the font is designed for: one em = one 8x8 cell.
const CELL: i32 = 8;

/// One glyph: 8 rows, bit 7 = leftmost pixel.
type Mask = [u8; 8];

/// Press Start 2P (embedded TTF, Latin + Cyrillic), rasterised at 8 px per glyph cell.
/// Monospaced: every character advances 8 px at scale 1.
pub struct Font {
    font: fontdue::Font,
    /// Row of the cell (0 = top) the baseline sits under: glyph rows above it are `baseline - 1 ..`.
    baseline: i32,
    masks: HashMap<char, Mask>,
}

impl Font {
    pub fn new() -> Font {
        let font = fontdue::Font::from_bytes(FONT_TTF, fontdue::FontSettings::default())
            .expect("embedded Press Start 2P parses");
        // Capitals fill the cell from its top row: the baseline is just under the top of 'A'.
        // fontdue's bitmap may carry an empty antialiasing row, so measure the lit pixels.
        let (m, _) = font.rasterize('A', CELL as f32);
        let mut f = Font { font, baseline: m.ymin + m.height as i32, masks: HashMap::with_capacity(256) };
        if let Some(top) = f.rasterize('A').and_then(|mask| mask.iter().position(|&r| r != 0)) {
            f.baseline -= top as i32;
        }
        f
    }

    fn rasterize(&self, ch: char) -> Option<Mask> {
        let index = self.font.lookup_glyph_index(ch);
        if index == 0 && ch != '\0' {
            return None;
        }
        let (m, cov) = self.font.rasterize_indexed(index, CELL as f32);
        let mut mask = [0u8; 8];
        // fontdue rows run top-down; the bitmap's bottom row is `ymin` pixels above the baseline.
        let top = self.baseline - (m.ymin + m.height as i32);
        for gy in 0..m.height {
            let cy = top + gy as i32;
            if !(0..CELL).contains(&cy) {
                continue;
            }
            for gx in 0..m.width {
                let cx = m.xmin + gx as i32;
                if (0..CELL).contains(&cx) && cov[gy * m.width + gx] >= 128 {
                    mask[cy as usize] |= 0x80 >> cx;
                }
            }
        }
        Some(mask)
    }

    fn mask(&mut self, ch: char) -> Mask {
        if let Some(mask) = self.masks.get(&ch) {
            return *mask;
        }
        let mask = match self.rasterize(ch) {
            Some(mask) => mask,
            None => self.rasterize('?').unwrap_or([0; 8]),
        };
        self.masks.insert(ch, mask);
        mask
    }

    /// Draws `text` with its top-left at (x, y); `scale` multiplies the 8 px cell (1 or 2).
    /// Characters missing from the font draw as '?'.
    pub fn draw(&mut self, canvas: &mut Canvas, x: i32, y: i32, text: &str, color: Color, scale: u32) {
        let s = scale.max(1) as i32;
        let mut cx = x;
        for ch in text.chars() {
            if cx >= W as i32 {
                break;
            }
            if ch != ' ' && cx + CELL * s > 0 {
                let mask = self.mask(ch);
                for (row, bits) in mask.iter().enumerate() {
                    if *bits == 0 {
                        continue;
                    }
                    for col in 0..CELL {
                        if bits & (0x80 >> col) != 0 {
                            canvas.fill_rect(cx + col * s, y + row as i32 * s, s, s, color);
                        }
                    }
                }
            }
            cx += CELL * s;
        }
    }

    /// Width in pixels of `text` at `scale`.
    pub fn width(&self, text: &str, scale: u32) -> i32 {
        CELL * text.chars().count() as i32 * scale.max(1) as i32
    }

    /// Draws horizontally centred on the canvas.
    pub fn draw_centered(&mut self, canvas: &mut Canvas, y: i32, text: &str, color: Color, scale: u32) {
        let x = (W as i32 - self.width(text, scale)) / 2;
        self.draw(canvas, x, y, text, color, scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&ebobo_core::cart::crc32(&[kind, data]).to_be_bytes());
    }

    /// Minimal RGBA PNG: zlib with stored (uncompressed) deflate blocks.
    fn write_png(path: &str, w: usize, h: usize, rgba: &[u8]) {
        let mut raw = Vec::with_capacity(h * (w * 4 + 1));
        for row in rgba.chunks_exact(w * 4) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, block) in blocks.iter().enumerate() {
            z.push((i + 1 == blocks.len()) as u8);
            let len = block.len() as u16;
            z.extend_from_slice(&len.to_le_bytes());
            z.extend_from_slice(&(!len).to_le_bytes());
            z.extend_from_slice(block);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());

        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(w as u32).to_be_bytes());
        ihdr.extend_from_slice(&(h as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png_chunk(&mut png, b"IHDR", &ihdr);
        png_chunk(&mut png, b"IDAT", &z);
        png_chunk(&mut png, b"IEND", &[]);
        std::fs::write(path, png).unwrap();
    }

    fn lit(c: &Canvas, x0: i32, y0: i32, size: i32) -> usize {
        let mut n = 0;
        for y in y0..y0 + size {
            for x in x0..x0 + size {
                if c.px[(y as usize * W + x as usize) * 4] > 128 {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn canvas_font_renders_latin_and_cyrillic() {
        let mut c = Canvas::new();
        c.clear([0x14, 0x13, 0x1A, 255]);
        let mut font = Font::new();
        let text = "ПРИВЕТ ebobo 123";
        let white = [255, 255, 255, 255];
        font.draw(&mut c, 8, 8, text, white, 1);
        font.draw(&mut c, 0, 24, text, white, 2);
        font.draw(&mut c, 8, 48, "AЙgдё Ъ{}|~\u{2603}", white, 1);
        font.draw_centered(&mut c, 64, "EBOBO", [0xE4, 0x3B, 0x2C, 255], 2);
        write_png("/tmp/ebobo-font-test.png", W, H, &c.px);

        assert_eq!(font.width(text, 1), 8 * 16);
        assert_eq!(font.width(text, 2), 16 * 16);
        for (i, ch) in text.chars().enumerate() {
            let at1 = lit(&c, 8 + 8 * i as i32, 8, 8);
            let at2 = lit(&c, 16 * i as i32, 24, 16);
            if ch == ' ' {
                assert_eq!((at1, at2), (0, 0), "space cell {i} is empty");
            } else {
                assert!(at1 >= 5, "glyph {ch:?} at scale 1 has {at1} lit pixels");
                assert_eq!(at2, at1 * 4, "scale 2 doubles every pixel of {ch:?}");
            }
        }
        // Missing glyph falls back to '?'.
        assert_eq!(font.mask('\u{2603}'), font.mask('?'));
        // Cell contents never spill into the neighbouring rows.
        assert_eq!(lit(&c, 0, 0, 8) + lit(&c, 8, 16, 8), 0);
    }

    #[test]
    fn canvas_fill_rect_clips_and_blends() {
        let mut c = Canvas::new();
        c.clear([0, 0, 0, 255]);
        c.fill_rect(-10, -10, 20, 20, [255, 0, 0, 255]);
        assert_eq!(&c.px[0..4], &[255, 0, 0, 255]);
        assert_eq!(&c.px[10 * 4..10 * 4 + 4], &[0, 0, 0, 255]);
        c.fill_rect(250, 230, 100, 100, [0, 0, 255, 128]);
        let last = (H * W - 1) * 4;
        assert_eq!(&c.px[last..last + 4], &[0, 0, 128, 255]);
        c.rect(20, 20, 4, 3, [9, 9, 9, 255]);
        let at = |x: usize, y: usize| c.px[(y * W + x) * 4];
        assert_eq!((at(20, 20), at(23, 22), at(21, 21), at(22, 21)), (9, 9, 0, 0));
        c.dim(0.5);
        assert_eq!(c.px[0], 128);
    }
}
