//! PNG in and out, and shrinking. Owner: image slice.

use crate::Picture;

/// Largest side `decode_png` accepts, in pixels.
const MAX_DECODE_SIDE: u32 = 4096;

/// Decodes a PNG of any colour type and bit depth into RGBA8888. Refuses files whose header
/// claims more than 4096 pixels a side, before allocating for them.
///
/// 16-bit samples keep their high byte; palette, grey and tRNS files are expanded. The error is
/// a short text for the log.
pub fn decode_png(bytes: &[u8]) -> Result<Picture, String> {
    // The IHDR chunk is always first: signature (8), length (4), "IHDR" (4), width (4), height (4).
    if bytes.len() < 24 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] || &bytes[12..16] != b"IHDR" {
        return Err("not a png".into());
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if w == 0 || h == 0 {
        return Err("png: empty picture".into());
    }
    if w > MAX_DECODE_SIDE || h > MAX_DECODE_SIDE {
        return Err(format!("png too large: {w}x{h}"));
    }

    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| format!("png: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(|e| format!("png: {e}"))?;
    if frame.bit_depth != png::BitDepth::Eight {
        return Err("png: unexpected bit depth".into());
    }
    buf.truncate(frame.buffer_size());

    let pixels = frame.width as usize * frame.height as usize;
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => return Err("png: palette not expanded".into()),
    };
    if buf.len() != pixels * channels {
        return Err("png: unexpected data size".into());
    }
    let rgba = match channels {
        4 => buf,
        _ => {
            let mut out = Vec::with_capacity(pixels * 4);
            for p in buf.chunks_exact(channels) {
                match channels {
                    3 => out.extend_from_slice(&[p[0], p[1], p[2], 255]),
                    2 => out.extend_from_slice(&[p[0], p[0], p[0], p[1]]),
                    _ => out.extend_from_slice(&[p[0], p[0], p[0], 255]),
                }
            }
            out
        }
    };
    Ok(Picture { w: frame.width, h: frame.height, rgba })
}

/// Encodes as a PNG (for the disk cache): RGBA, 8 bits a channel, default compression.
///
/// A picture that cannot be a PNG (a zero side, or `rgba` not `w`×`h`×4 bytes) gives an empty
/// vector, which `decode_png` refuses.
pub fn encode_png(picture: &Picture) -> Vec<u8> {
    let mut out = Vec::new();
    if encode_into(picture, &mut out).is_err() {
        out.clear();
    }
    out
}

fn encode_into(picture: &Picture, out: &mut Vec<u8>) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(out, picture.w, picture.h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&picture.rgba)?;
    writer.finish()
}

/// The size `w`×`h` shrinks to inside `max_w`×`max_h`: the side that limits takes the whole
/// maximum, the other is rounded to the nearest pixel (halves up) and is at least 1.
/// 512×720 into 320×320 is 228×320.
fn fitted_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (w64, h64, mw, mh) = (w as u64, h as u64, max_w as u64, max_h as u64);
    if w64 * mh >= h64 * mw {
        (max_w, ((h64 * mw + w64 / 2) / w64).max(1) as u32)
    } else {
        (((w64 * mh + h64 / 2) / h64).max(1) as u32, max_h)
    }
}

/// Shrinks `picture` to fit `max_w`×`max_h`, keeping its proportions, averaging the source
/// pixels each result pixel covers (alpha-weighted). A picture that already fits is returned
/// as it is; nothing is ever enlarged.
///
/// The result size is the limiting side at its maximum and the other rounded to the nearest
/// pixel (512×720 into 320×320 gives 228×320). Each result pixel is the exact area average of
/// the source rectangle under it, partly covered source pixels counted by their covered share.
/// Colour is weighted by alpha, so transparent pixels do not darken their neighbours; a fully
/// transparent result pixel is 0,0,0,0. Integer arithmetic only, rounding halves up.
/// A picture with a zero side or a buffer of the wrong length, or a zero box, is returned as it is.
pub fn fit(picture: Picture, max_w: u32, max_h: u32) -> Picture {
    let (w, h) = (picture.w, picture.h);
    if (w <= max_w && h <= max_h) || w == 0 || h == 0 || max_w == 0 || max_h == 0 {
        return picture;
    }
    if picture.rgba.len() as u64 != w as u64 * h as u64 * 4 {
        return picture;
    }
    let (ow, oh) = fitted_size(w, h, max_w, max_h);
    let (sw, sh, dw, dh) = (w as u64, h as u64, ow as u64, oh as u64);

    // Horizontally, in units of 1/ow of a source pixel: source pixel i covers [i*ow, (i+1)*ow),
    // result pixel x covers [x*w, (x+1)*w). The weight is the overlap; a result pixel's weights
    // sum to w. Vertically the same with oh and h.
    let mut x_first = Vec::with_capacity(ow as usize);
    let mut x_weights: Vec<u64> = Vec::with_capacity(w as usize + ow as usize);
    let mut x_start = Vec::with_capacity(ow as usize + 1);
    for x in 0..dw {
        let (lo, hi) = (x * sw, (x + 1) * sw);
        let first = lo / dw;
        let end = hi.div_ceil(dw);
        x_first.push(first as usize);
        x_start.push(x_weights.len());
        for i in first..end {
            x_weights.push(((i + 1) * dw).min(hi) - (i * dw).max(lo));
        }
    }
    x_start.push(x_weights.len());

    let total = sw * sh;
    let src = &picture.rgba;
    let stride = w as usize * 4;
    let mut rgba = Vec::with_capacity(ow as usize * oh as usize * 4);
    // Per result pixel of the current row: weighted sums of r·a, g·a, b·a and a.
    let mut acc = vec![0u64; ow as usize * 4];
    for y in 0..dh {
        acc.fill(0);
        let (lo, hi) = (y * sh, (y + 1) * sh);
        for j in lo / dh..hi.div_ceil(dh) {
            let wy = ((j + 1) * dh).min(hi) - (j * dh).max(lo);
            let row = &src[j as usize * stride..][..stride];
            for (x, cell) in acc.chunks_exact_mut(4).enumerate() {
                let weights = &x_weights[x_start[x]..x_start[x + 1]];
                let pixels = &row[x_first[x] * 4..][..weights.len() * 4];
                for (p, &wx) in pixels.chunks_exact(4).zip(weights) {
                    let wa = wx * wy * p[3] as u64;
                    cell[0] += wa * p[0] as u64;
                    cell[1] += wa * p[1] as u64;
                    cell[2] += wa * p[2] as u64;
                    cell[3] += wa;
                }
            }
        }
        for cell in acc.chunks_exact(4) {
            let asum = cell[3];
            if asum == 0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let half = asum / 2;
            rgba.extend_from_slice(&[
                ((cell[0] + half) / asum) as u8,
                ((cell[1] + half) / asum) as u8,
                ((cell[2] + half) / asum) as u8,
                ((asum + total / 2) / total) as u8,
            ]);
        }
    }
    Picture { w: ow, h: oh, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Picture {
        Picture { w, h, rgba: px.repeat(w as usize * h as usize) }
    }

    fn encode_raw(w: u32, h: u32, color: png::ColorType, depth: png::BitDepth, palette: Option<(&[u8], &[u8])>, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, w, h);
            encoder.set_color(color);
            encoder.set_depth(depth);
            if let Some((rgb, trns)) = palette {
                encoder.set_palette(rgb.to_vec());
                encoder.set_trns(trns.to_vec());
            }
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
            writer.finish().unwrap();
        }
        out
    }

    #[test]
    fn round_trip() {
        let picture = Picture { w: 3, h: 2, rgba: (0..24).map(|i| (i * 11) as u8).collect() };
        let bytes = encode_png(&picture);
        assert_eq!(decode_png(&bytes).unwrap(), picture);
    }

    #[test]
    fn encode_of_a_broken_picture_is_empty() {
        assert!(encode_png(&Picture { w: 2, h: 2, rgba: vec![0; 3] }).is_empty());
        assert!(decode_png(&[]).is_err());
    }

    #[test]
    fn decodes_palette_with_transparency() {
        let bytes = encode_raw(2, 1, png::ColorType::Indexed, png::BitDepth::Eight, Some((&[255, 0, 0, 0, 255, 0], &[255, 0])), &[0, 1]);
        let picture = decode_png(&bytes).unwrap();
        assert_eq!((picture.w, picture.h), (2, 1));
        assert_eq!(picture.rgba, [255, 0, 0, 255, 0, 255, 0, 0]);
    }

    #[test]
    fn decodes_16_bit_grey() {
        let bytes = encode_raw(2, 1, png::ColorType::Grayscale, png::BitDepth::Sixteen, None, &[0x12, 0x34, 0xFE, 0xDC]);
        let picture = decode_png(&bytes).unwrap();
        assert_eq!(picture.rgba, [0x12, 0x12, 0x12, 255, 0xFE, 0xFE, 0xFE, 255]);
    }

    #[test]
    fn refuses_a_huge_header() {
        let mut bytes = encode_png(&solid(1, 1, [1, 2, 3, 4]));
        bytes[16..20].copy_from_slice(&5000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&5000u32.to_be_bytes());
        let error = decode_png(&bytes).unwrap_err();
        assert!(error.contains("too large"), "{error}");
    }

    #[test]
    fn checker_averages() {
        let mut rgba = Vec::new();
        for y in 0..4 {
            for x in 0..4 {
                let v = if (x + y) % 2 == 0 { 0 } else { 255 };
                rgba.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let out = fit(Picture { w: 4, h: 4, rgba }, 2, 2);
        assert_eq!((out.w, out.h), (2, 2));
        // 127.5 rounds up.
        assert_eq!(out.rgba, [128, 128, 128, 255].repeat(4));
    }

    #[test]
    fn fractional_edges_are_exact() {
        // 3 pixels into 2: each result pixel is one whole pixel and half of the middle one.
        let rgba = vec![0, 0, 0, 255, 90, 90, 90, 255, 30, 30, 30, 255];
        let out = fit(Picture { w: 3, h: 1, rgba }, 2, 1);
        assert_eq!((out.w, out.h), (2, 1));
        assert_eq!(out.rgba, [30, 30, 30, 255, 50, 50, 50, 255]);
    }

    #[test]
    fn a_picture_that_fits_is_unchanged() {
        let picture = Picture { w: 3, h: 2, rgba: (0..24).collect() };
        assert_eq!(fit(picture.clone(), 320, 320), picture);
        assert_eq!(fit(picture.clone(), 3, 2), picture);
    }

    #[test]
    fn keeps_proportions() {
        let out = fit(solid(512, 720, [10, 20, 30, 255]), 320, 320);
        assert_eq!((out.w, out.h), (228, 320));
        assert_eq!(out.rgba, [10, 20, 30, 255].repeat(228 * 320));
        let out = fit(solid(720, 512, [10, 20, 30, 255]), 320, 320);
        assert_eq!((out.w, out.h), (320, 228));
        let out = fit(solid(4000, 2, [9, 9, 9, 255]), 320, 320);
        assert_eq!((out.w, out.h), (320, 1));
    }

    #[test]
    fn transparent_neighbours_do_not_darken() {
        let out = fit(Picture { w: 2, h: 1, rgba: vec![255, 0, 0, 255, 0, 0, 0, 0] }, 1, 1);
        assert_eq!(out.rgba, [255, 0, 0, 128]);
        let out = fit(solid(2, 2, [200, 100, 50, 0]), 1, 1);
        assert_eq!(out.rgba, [0, 0, 0, 0]);
    }

    /// Needs the network: `cargo test --release -p ebobo-art image:: -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_box_art() {
        let url = "https://thumbnails.libretro.com/Nintendo%20-%20Nintendo%20Entertainment%20System/Named_Boxarts/Chip%20'n%20Dale%20-%20Rescue%20Rangers%20(Europe).png";
        let dir = std::env::temp_dir().join(format!("ebobo-art-{}-boxart", std::process::id()));
        let file = dir.join("box.png");
        let started = std::time::Instant::now();
        crate::net::fetch(url, &file, 30).unwrap();
        let fetched = started.elapsed();
        let bytes = std::fs::read(&file).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let started = std::time::Instant::now();
        let picture = decode_png(&bytes).unwrap();
        let decoded = started.elapsed();
        assert_eq!((picture.w, picture.h), (512, 727));

        let started = std::time::Instant::now();
        let small = fit(picture, 320, 320);
        let fitted = started.elapsed();
        assert_eq!((small.w, small.h), (225, 320));

        let started = std::time::Instant::now();
        let again = encode_png(&small);
        let encoded = started.elapsed();
        assert_eq!(decode_png(&again).unwrap(), small);
        println!("{} bytes: fetch {fetched:?}, decode {decoded:?}, fit {fitted:?}, encode {encoded:?} ({} bytes)", bytes.len(), again.len());
    }
}
