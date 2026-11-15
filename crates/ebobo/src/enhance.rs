//! Picture enhancement ("Enhance"): HD smoothing of pixel art that knows which pixels are
//! sprites, and soft "depth" shadows between the layers. Display only: it reads the console's
//! finished picture and layer plane and never touches emulated state.
//!
//! The magnification filter is MMPX:
//!
//!   Morgan McGuire and Mara Gagiu, "MMPX Style-Preserving Pixel Art Magnification",
//!   Journal of Computer Graphics Techniques (JCGT), vol. 10, no. 2, 2021.
//!   Reference implementation Copyright 2020 Morgan McGuire & Mara Gagiu, provided under the
//!   Open Source MIT license (https://opensource.org/licenses/MIT), from
//!   https://casual-effects.com/research/McGuire2021PixelArt/.
//!
//! `mmpx_rules` is a port of that reference (`MMPXAlgorithm::run` in its `cppPerf.cpp`), rule for
//! rule and in the same order. Two things differ, both on purpose:
//! - A pixel is the word `layer << 24 | 0x00RRGGBB`, so "equal" in the rules means the same
//!   colour AND the same layer: a sprite pixel never merges with a background pixel of its
//!   colour, and every output pixel carries the layer of the source pixel its colour came from.
//! - The reference weighs brightness by transparency (the top byte was alpha). Console pictures
//!   are opaque, so brightness here is just R + G + B + 1 and the layer does not enter it.

/// Switchable picture enhancement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The console's own pixels.
    #[default]
    Off,
    /// Magnified 4 times by MMPX applied twice (2 times for a picture taller than 240 lines).
    Hd,
    /// `Hd` plus soft shadows that every layer casts onto the layers behind it.
    Depth,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Hd, Mode::Depth];

    pub fn next(self) -> Mode {
        let i = Self::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Mode {
        let i = Self::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// CLI / config name.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Hd => "hd",
            Mode::Depth => "depth",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Self::ALL.into_iter().find(|e| e.name().eq_ignore_ascii_case(s.trim()))
    }
}

/// An enhanced picture: `w`×`h` RGBA bytes, `scale` times the console's own size.
pub struct Enhanced<'a> {
    pub rgba: &'a [u8],
    pub w: usize,
    pub h: usize,
    pub scale: u32,
}

/// Tallest picture that is magnified 4 times; taller ones (interlace) are magnified 2 times.
const MAX_ROWS_4X: usize = 240;
/// Shadow offset, down and to the right, in halves of a source pixel per layer of height.
const SHADOW_OFFSET_HALVES: usize = 3;
/// Darkening at full shadow strength, of 255 (40 %).
const SHADOW_DARK: u32 = 102;
/// Highest layer value the cores report.
const TOP_LAYER: u8 = 3;
/// Row bands a pass is split into: the calling thread takes the last, scoped threads the rest.
/// The bands only change who computes a row, never its bytes.
const BANDS: usize = 4;

/// The enhancer and its buffers, which are reused from frame to frame: no picture buffer is
/// allocated once a picture of the same size has been through.
#[derive(Default)]
pub struct Enhancer {
    /// The source picture as `layer << 24 | 0x00RRGGBB` words.
    src: Vec<u32>,
    /// After one MMPX pass, and after two.
    x2: Vec<u32>,
    x4: Vec<u32>,
    /// Per band: in MMPX one flag per source pixel of the row in work (its neighbourhood is
    /// not flat), in the shadow one row of strengths and one blended mask row.
    scratch: [Scratch; BANDS],
    /// The result, RGBA bytes.
    out: Vec<u8>,
    /// Shadow scratch: the horizontal box sums of the occluders and the blurred mask, both
    /// 2 times the source size.
    hsum: Vec<u32>,
    soft: Vec<u32>,
    /// Bands to use, 1..=`BANDS`; 0 means `BANDS`.
    bands: usize,
}

impl Enhancer {
    pub fn new() -> Enhancer {
        Enhancer::default()
    }

    /// Enhances a `w`×`h` picture (`rgba`: 4 bytes per pixel; `layers`: one byte per pixel,
    /// 0-3, higher = nearer the viewer; a plane of the wrong length counts as all zero).
    /// `Mode::Off` hands the input back with `scale` 1.
    pub fn apply<'a>(&'a mut self, mode: Mode, rgba: &'a [u8], layers: &[u8], w: usize, h: usize) -> Enhanced<'a> {
        let n = w * h;
        if mode == Mode::Off || n == 0 || rgba.len() < n * 4 {
            return Enhanced { rgba, w, h, scale: 1 };
        }
        self.src.resize(n, 0);
        let mut top = 0;
        if layers.len() >= n {
            for ((d, c), &l) in self.src.iter_mut().zip(rgba.chunks_exact(4)).zip(layers) {
                let l = l.min(TOP_LAYER);
                top = top.max(l);
                *d = u32::from(l) << 24 | u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2]);
            }
        } else {
            for (d, c) in self.src.iter_mut().zip(rgba.chunks_exact(4)) {
                *d = u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2]);
            }
        }

        let bands = if self.bands == 0 { BANDS } else { self.bands.min(BANDS) };
        self.x2.resize(n * 4, 0);
        let src = &self.src;
        banded(&mut self.x2, 4 * w, h, bands, &mut self.scratch, |y0, part, sc| mmpx_band(src, w, h, y0, part, &mut sc.bytes));
        let (big, scale) = if h > MAX_ROWS_4X {
            (&self.x2, 2)
        } else {
            self.x4.resize(n * 16, 0);
            let x2 = &self.x2;
            banded(&mut self.x4, 8 * w, 2 * h, bands, &mut self.scratch, |y0, part, sc| {
                mmpx_band(x2, 2 * w, 2 * h, y0, part, &mut sc.bytes)
            });
            (&self.x4, 4)
        };
        let (bw, bh) = (w * scale, h * scale);
        self.out.resize(bw * bh * 4, 0);

        if mode == Mode::Depth && top > 0 {
            let mw = 2 * w;
            soften(&self.x2, mw, 2 * h, bands, &mut self.scratch, &mut self.hsum, &mut self.soft);
            let soft = &self.soft;
            let area = STRENGTH_FULL;
            banded(&mut self.out, bw * 4, bh, bands, &mut self.scratch, |y0, part, sc| {
                sc.bytes.resize(bw, 0);
                let strength = &mut sc.bytes[..bw];
                for (i, out) in part.chunks_exact_mut(bw * 4).enumerate() {
                    let y = y0 + i;
                    let row = &big[y * bw..][..bw];
                    if shadow_row(soft, mw, row, y, scale, top, strength, &mut sc.words) {
                        darken_row(row, strength, area, out);
                    } else {
                        unpack_row(row, out);
                    }
                }
            });
        } else {
            banded(&mut self.out, bw * 4, bh, bands, &mut self.scratch, |y0, part, _| unpack_row(&big[y0 * bw..], part));
        }
        Enhanced { rgba: &self.out, w: bw, h: bh, scale: scale as u32 }
    }
}

/// Splits the first `rows` rows of `dst` (`row_len` items each) into up to `bands` runs of
/// whole rows and calls `f(first_row, run, scratch)` for each: all runs but the last on scoped
/// threads, the last on the calling thread. One band runs without any thread.
fn banded<T: Send>(
    dst: &mut [T],
    row_len: usize,
    rows: usize,
    bands: usize,
    scratch: &mut [Scratch; BANDS],
    f: impl Fn(usize, &mut [T], &mut Scratch) + Sync,
) {
    let per = rows.div_ceil(bands.max(1)).max(1);
    let dst = &mut dst[..rows * row_len];
    if per >= rows {
        f(0, dst, &mut scratch[0]);
        return;
    }
    std::thread::scope(|s| {
        let mut parts = dst.chunks_mut(per * row_len).zip(scratch.iter_mut()).enumerate();
        let last = parts.next_back();
        for (i, (part, sc)) in parts {
            let f = &f;
            s.spawn(move || f(i * per, part, sc));
        }
        if let Some((i, (part, sc))) = last {
            f(i * per, part, sc);
        }
    });
}

/// Packed words to RGBA bytes (the layer is dropped, alpha is 255).
fn unpack_row(row: &[u32], out: &mut [u8]) {
    for (o, &p) in out.chunks_exact_mut(4).zip(row) {
        o.copy_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8, 255]);
    }
}

/// Packed words to RGBA bytes, each darkened toward black by up to 40 % by its shadow
/// strength (`strength` counts occluders in the blur window, 0..=`area`).
fn darken_row(row: &[u32], strength: &[u8], area: u32, out: &mut [u8]) {
    // dark = SHADOW_DARK * strength / area, by a multiplication so the loop vectorises.
    let mul = (SHADOW_DARK << 16).div_ceil(area);
    // Exact x / 255 for x < 65535.
    let div255 = |x: u32| (x + 1 + (x >> 8)) >> 8;
    for ((o, &p), &s) in out.chunks_exact_mut(4).zip(row).zip(strength) {
        let f = 255 - ((u32::from(s) * mul) >> 16);
        let (r, g, b) = (p >> 16 & 0xFF, p >> 8 & 0xFF, p & 0xFF);
        o.copy_from_slice(&[div255(r * f) as u8, div255(g * f) as u8, div255(b * f) as u8, 255]);
    }
}

/// What one band keeps between rows.
#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    words: Vec<u32>,
}

/// One 10-bit counter per layer 1-3 in a word: what a pixel of that layer adds to the blur.
/// Ten bits hold the 25 of a full box times the 16 of the sampling weights.
const OCCLUDER: [u32; 4] = [0, 1, 1 << 10, 1 << 20];
/// Blur radius in mask pixels (halves of a source pixel): a 5×5 box, about the 9×9 output
/// pixels the blur covered when it ran at 4 times the source size.
const MASK_RADIUS: usize = 2;
/// Shadow strength under a full box: 25 occluders × 16 sampling weight, halved to fit a byte.
const STRENGTH_FULL: u32 = 200;

/// Blurs the occluders at 2 times the source size (`x2`, `w`×`h`: the picture after one MMPX
/// pass, whatever the output scale): `soft[i]` holds, for each layer 1-3 (bits 0-9, 10-19,
/// 20-29), how many pixels of exactly that layer lie in the 5×5 box around mask pixel i.
/// Outside the picture nothing occludes. Both passes run in row bands: the horizontal sums
/// (`hsum`) are finished before the vertical pass reads them, so the bands never change a value.
///
/// Blurring the occluder rather than the finished shadow is what keeps the soft edge off the
/// occluder itself: a pixel only ever looks up the counters of layers above its own.
fn soften(x2: &[u32], w: usize, h: usize, bands: usize, scratch: &mut [Scratch; BANDS], hsum: &mut Vec<u32>, soft: &mut Vec<u32>) {
    let r = MASK_RADIUS;
    let n = 2 * r + 1;
    hsum.resize(w * h, 0);
    soft.resize(w * h, 0);
    banded(hsum, w, h, bands, scratch, |y0, part, sc| {
        // Pixel x sits at pad[x + r + 1]; its window is pad[x + 1..=x + n].
        let pad = &mut sc.words;
        pad.clear();
        pad.resize(w + n, 0);
        for (i, out) in part.chunks_exact_mut(w).enumerate() {
            for (m, &p) in pad[r + 1..r + 1 + w].iter_mut().zip(&x2[(y0 + i) * w..][..w]) {
                *m = OCCLUDER[(p >> 24) as usize & 3];
            }
            let mut sum: u32 = pad[..n].iter().sum();
            for ((o, &add), &sub) in out.iter_mut().zip(&pad[n..]).zip(&pad[..w]) {
                sum = sum + add - sub;
                *o = sum;
            }
        }
    });
    let hsum = &*hsum;
    banded(soft, w, h, bands, scratch, |y0, part, _| {
        for (i, out) in part.chunks_exact_mut(w).enumerate() {
            let y = y0 + i;
            let (from, to) = (y.saturating_sub(r), (y + r + 1).min(h));
            out.copy_from_slice(&hsum[from * w..][..w]);
            for yy in from + 1..to {
                for (o, &s) in out.iter_mut().zip(&hsum[yy * w..][..w]) {
                    *o += s;
                }
            }
        }
    });
}

/// Shadow strength of one output row (0..=`STRENGTH_FULL`): a pixel of layer p is shadowed by
/// layer p + k as far as that layer covers the blur box `k * 1.5` source pixels up and to the
/// left of it; the strongest occluder counts. One pass per height difference `k`. Returns
/// whether any pixel of the row is in shadow.
///
/// The mask (`soft`, `mw` wide) is 2 times the source size. A 4x output pixel lies a quarter
/// of a mask pixel off a mask centre, so it takes the four nearest mask values with weights
/// 3:1 each way (16 in all); a 2x output pixel is a mask pixel and takes it 16 times. `t` is
/// scratch: the two mask rows blended, with two zero columns in front.
#[allow(clippy::too_many_arguments)]
fn shadow_row(soft: &[u32], mw: usize, row: &[u32], y: usize, scale: usize, top: u8, strength: &mut [u8], t: &mut Vec<u32>) -> bool {
    strength.fill(0);
    // Layer p + k is counted in bits 10(p+k-1)..; past layer 3 the shift runs off the counters.
    let pick = |v: u32, p: u32, k: usize| (((u64::from(v) >> (((p >> 24) + k as u32 - 1) * 10)) as u32 & 0x3FF) >> 1) as u8;
    for k in 1..=usize::from(top) {
        let d = k * SHADOW_OFFSET_HALVES;
        // Mask rows r0 and r0 + 1 with weights w0 and w1 (of 4).
        let (r0, w0, w1) = if scale == 4 {
            let n = (y / 2) as isize - d as isize;
            if y % 2 == 0 { (n - 1, 1, 3) } else { (n, 3, 1) }
        } else {
            (y as isize - d as isize, 4, 0)
        };
        if r0 + 1 < 0 || d >= mw {
            break;
        }
        t.clear();
        t.resize(mw + 2, 0);
        if r0 >= 0 {
            for (o, &v) in t[2..].iter_mut().zip(&soft[r0 as usize * mw..][..mw]) {
                *o = v * w0;
            }
        }
        if w1 > 0 {
            for (o, &v) in t[2..].iter_mut().zip(&soft[(r0 + 1) as usize * mw..][..mw]) {
                *o += v * w1;
            }
        }
        if scale == 4 {
            // Output pixels 2m and 2m + 1 read mask columns m - d - 1 ..= m - d + 1, from m = d - 1.
            let x0 = 2 * (d - 1);
            for ((s, p), win) in strength[x0..].chunks_exact_mut(2).zip(row[x0..].chunks_exact(2)).zip(t.windows(3)) {
                s[0] = s[0].max(pick(win[0] + 3 * win[1], p[0], k));
                s[1] = s[1].max(pick(3 * win[1] + win[2], p[1], k));
            }
        } else {
            for ((s, &p), &v) in strength[d..].iter_mut().zip(&row[d..]).zip(&t[2..]) {
                *s = (*s).max(pick(4 * v, p, k));
            }
        }
    }
    strength.iter().fold(0, |a, &s| a | s) != 0
}

/// One MMPX pass: `src` (`w`×`h` words) magnified 2 times into `dst` (`2w`×`2h`). Outside the
/// picture the nearest edge pixel repeats. `busy` is scratch (one byte per source column).
#[cfg(test)]
fn mmpx_2x(src: &[u32], w: usize, h: usize, dst: &mut [u32], busy: &mut Vec<u8>) {
    mmpx_band(src, w, h, 0, &mut dst[..w * h * 4], busy);
}

/// Source rows `y0..` of one MMPX pass, as many as `dst` holds (4·`w` words per source row):
/// the same words `mmpx_2x` writes there, whatever the band.
fn mmpx_band(src: &[u32], w: usize, h: usize, y0: usize, dst: &mut [u32], busy: &mut Vec<u8>) {
    busy.clear();
    busy.resize(w, 0);
    let row = |y: isize| &src[y.clamp(0, h as isize - 1) as usize * w..][..w];
    for (band_y, out) in dst.chunks_exact_mut(4 * w).enumerate() {
        let yi = (y0 + band_y) as isize;
        let rows = [row(yi - 3), row(yi - 2), row(yi - 1), row(yi), row(yi + 1), row(yi + 2), row(yi + 3)];
        let (up, mid, down) = (rows[2], rows[3], rows[4]);
        let (out0, out1) = out.split_at_mut(2 * w);

        // The common case first, for the whole row: a pixel whose 3×3 neighbourhood is one
        // colour becomes four of itself. Both loops are branch-free so they vectorise.
        for ((o0, o1), &e) in out0.chunks_exact_mut(2).zip(out1.chunks_exact_mut(2)).zip(mid) {
            o0[0] = e;
            o0[1] = e;
            o1[0] = e;
            o1[1] = e;
        }
        let differs = |x: usize, l: usize, r: usize| {
            let e = mid[x];
            ((up[l] ^ e) | (up[x] ^ e) | (up[r] ^ e) | (mid[l] ^ e) | (mid[r] ^ e) | (down[l] ^ e) | (down[x] ^ e) | (down[r] ^ e)) != 0
        };
        if w >= 3 {
            let n = w - 2;
            let (u0, u1, u2) = (&up[..n], &up[1..n + 1], &up[2..n + 2]);
            let (m0, m1, m2) = (&mid[..n], &mid[1..n + 1], &mid[2..n + 2]);
            let (d0, d1, d2) = (&down[..n], &down[1..n + 1], &down[2..n + 2]);
            let flags = &mut busy[1..n + 1];
            for x in 0..n {
                let e = m1[x];
                let diff = (u0[x] ^ e) | (u1[x] ^ e) | (u2[x] ^ e) | (m0[x] ^ e) | (m2[x] ^ e) | (d0[x] ^ e) | (d1[x] ^ e) | (d2[x] ^ e);
                flags[x] = u8::from(diff != 0);
            }
        }
        busy[0] = u8::from(differs(0, 0, 1.min(w - 1)));
        busy[w - 1] = u8::from(differs(w - 1, w.saturating_sub(2), w - 1));

        // Then the rules, only where something is going on.
        for (x, _) in busy.iter().enumerate().filter(|(_, b)| **b != 0) {
            let [j, k, l, m] = mmpx_rules(&rows, x, w);
            out0[2 * x] = j;
            out0[2 * x + 1] = k;
            out1[2 * x] = l;
            out1[2 * x + 1] = m;
        }
    }
}

/// Brightness as the MMPX rules compare it (opaque pixels; the layer does not count).
#[inline]
fn luma(c: u32) -> u32 {
    (c >> 16 & 0xFF) + (c >> 8 & 0xFF) + (c & 0xFF) + 1
}

#[inline]
fn all_eq2(b: u32, a0: u32, a1: u32) -> bool {
    ((b ^ a0) | (b ^ a1)) == 0
}

#[inline]
fn all_eq3(b: u32, a0: u32, a1: u32, a2: u32) -> bool {
    ((b ^ a0) | (b ^ a1) | (b ^ a2)) == 0
}

#[inline]
fn all_eq4(b: u32, a0: u32, a1: u32, a2: u32, a3: u32) -> bool {
    ((b ^ a0) | (b ^ a1) | (b ^ a2) | (b ^ a3)) == 0
}

#[inline]
fn any_eq3(b: u32, a0: u32, a1: u32, a2: u32) -> bool {
    b == a0 || b == a1 || b == a2
}

#[inline]
fn none_eq2(b: u32, a0: u32, a1: u32) -> bool {
    b != a0 && b != a1
}

#[inline]
fn none_eq4(b: u32, a0: u32, a1: u32, a2: u32, a3: u32) -> bool {
    b != a0 && b != a1 && b != a2 && b != a3
}

/// The MMPX rules for the pixel in column `x` of the middle row of `rows` (rows y-3..=y+3,
/// each `w` wide): its four output pixels, top left, top right, bottom left, bottom right.
/// The names are the paper's: the 3×3 neighbourhood A B C / D E F / G H I, P and S two rows
/// up and down, Q and R two columns left and right, outputs J K / L M.
#[inline]
fn mmpx_rules(rows: &[&[u32]; 7], x: usize, w: usize) -> [u32; 4] {
    let s = |dx: isize, dy: isize| rows[(dy + 3) as usize][(x as isize + dx).clamp(0, w as isize - 1) as usize];
    let (a, b, c) = (s(-1, -1), s(0, -1), s(1, -1));
    let (d, e, f) = (s(-1, 0), s(0, 0), s(1, 0));
    let (g, h, i) = (s(-1, 1), s(0, 1), s(1, 1));
    let (mut j, mut k, mut l, mut m) = (e, e, e, e);

    let (p, ss) = (s(0, -2), s(0, 2));
    let (q, r) = (s(-2, 0), s(2, 0));
    let (bl, dl, el, fl, hl) = (luma(b), luma(d), luma(e), luma(f), luma(h));

    // 1:1 slope rules
    if (d == b && d != h && d != f) && (el >= dl || e == a) && any_eq3(e, a, c, g) && ((el < dl) || a != d || e != p || e != q) {
        j = d;
    }
    if (b == f && b != d && b != h) && (el >= bl || e == c) && any_eq3(e, a, c, i) && ((el < bl) || c != b || e != p || e != r) {
        k = b;
    }
    if (h == d && h != f && h != b) && (el >= hl || e == g) && any_eq3(e, a, g, i) && ((el < hl) || g != h || e != ss || e != q) {
        l = h;
    }
    if (f == h && f != b && f != d) && (el >= fl || e == i) && any_eq3(e, c, g, i) && ((el < fl) || i != h || e != r || e != ss) {
        m = f;
    }

    // Intersection rules
    if (e != f && all_eq4(e, c, i, d, q) && all_eq2(f, b, h)) && (f != s(3, 0)) {
        k = f;
        m = f;
    }
    if (e != d && all_eq4(e, a, g, f, r) && all_eq2(d, b, h)) && (d != s(-3, 0)) {
        j = d;
        l = d;
    }
    if (e != h && all_eq4(e, g, i, b, p) && all_eq2(h, d, f)) && (h != s(0, 3)) {
        l = h;
        m = h;
    }
    if (e != b && all_eq4(e, a, c, h, ss) && all_eq2(b, d, f)) && (b != s(0, -3)) {
        j = b;
        k = b;
    }
    if bl < el && all_eq4(e, g, h, i, ss) && none_eq4(e, a, d, c, f) {
        j = b;
        k = b;
    }
    if hl < el && all_eq4(e, a, b, c, p) && none_eq4(e, d, g, i, f) {
        l = h;
        m = h;
    }
    if fl < el && all_eq4(e, a, d, g, q) && none_eq4(e, b, c, i, h) {
        k = f;
        m = f;
    }
    if dl < el && all_eq4(e, c, f, i, r) && none_eq4(e, b, a, g, h) {
        j = d;
        l = d;
    }

    // 2:1 slope rules
    if h != b {
        if h != a && h != e && h != c {
            if all_eq3(h, g, f, r) && none_eq2(h, d, s(2, -1)) {
                l = m;
            }
            if all_eq3(h, i, d, q) && none_eq2(h, f, s(-2, -1)) {
                m = l;
            }
        }
        if b != i && b != g && b != e {
            if all_eq3(b, a, f, r) && none_eq2(b, d, s(2, 1)) {
                j = k;
            }
            if all_eq3(b, c, d, q) && none_eq2(b, f, s(-2, 1)) {
                k = j;
            }
        }
    }
    if f != d {
        if d != i && d != e && d != c {
            if all_eq3(d, a, h, ss) && none_eq2(d, b, s(1, 2)) {
                j = l;
            }
            if all_eq3(d, g, b, p) && none_eq2(d, h, s(1, -2)) {
                l = j;
            }
        }
        if f != e && f != a && f != g {
            if all_eq3(f, c, h, ss) && none_eq2(f, b, s(-1, 2)) {
                k = m;
            }
            if all_eq3(f, i, b, p) && none_eq2(f, h, s(-1, -2)) {
                m = k;
            }
        }
    }
    [j, k, l, m]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const NES_ROM: &str = "/Users/ivan/code/ebobo-chipndale-4p/original/Chip 'n Dale - Rescue Rangers (Europe).nes";
    const MD_ROMS: &str = "/Users/ivan/code/ebobo/crates/ebobo-md/tests/roms";

    const K: [u8; 4] = [0, 0, 0, 255];
    const W: [u8; 4] = [255, 255, 255, 255];

    /// A picture from rows of text: `#` is white, anything else black.
    fn art(rows: &[&str]) -> (Vec<u8>, usize, usize) {
        let rgba = rows.iter().flat_map(|r| r.bytes().flat_map(|c| if c == b'#' { W } else { K })).collect();
        (rgba, rows[0].len(), rows.len())
    }

    /// The reverse of `art`, one string per row.
    fn text(rgba: &[u8], w: usize) -> Vec<String> {
        let cells: Vec<char> = rgba.chunks_exact(4).map(|c| if c == W { '#' } else { '.' }).collect();
        cells.chunks(w).map(|r| r.iter().collect()).collect()
    }

    fn nearest(rgba: &[u8], w: usize, h: usize, f: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * f * f * 4);
        for y in 0..h * f {
            for x in 0..w * f {
                let i = (y / f * w + x / f) * 4;
                out.extend_from_slice(&rgba[i..i + 4]);
            }
        }
        out
    }

    fn owned(e: Enhanced) -> (Vec<u8>, usize, usize, u32) {
        (e.rgba.to_vec(), e.w, e.h, e.scale)
    }

    #[test]
    fn enhance_mode_names_round_trip() {
        for mode in Mode::ALL {
            assert_eq!(Mode::parse(mode.name()), Some(mode));
            assert_eq!(mode.next().prev(), mode);
            #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
            struct Cfg {
                enhance: Mode,
            }
            let text = toml::to_string(&Cfg { enhance: mode }).unwrap();
            assert_eq!(text.trim(), format!("enhance = \"{}\"", mode.name()));
            assert_eq!(toml::from_str::<Cfg>(&text).unwrap(), Cfg { enhance: mode });
        }
        assert_eq!(Mode::default(), Mode::Off);
        assert_eq!(Mode::parse(" HD "), Some(Mode::Hd));
        assert_eq!(Mode::parse("x"), None);
        assert_eq!(Mode::Off.next(), Mode::Hd);
        assert_eq!(Mode::Off.prev(), Mode::Depth);
    }

    #[test]
    fn enhance_off_passes_through() {
        let (rgba, w, h) = art(&["#..", ".#.", "..#"]);
        let mut e = Enhancer::new();
        let out = e.apply(Mode::Off, &rgba, &[1; 9], w, h);
        assert_eq!((out.w, out.h, out.scale), (3, 3, 1));
        assert!(std::ptr::eq(out.rgba, &rgba[..]));
    }

    #[test]
    fn enhance_flat_picture_stays_flat() {
        let rgba: Vec<u8> = [40, 90, 200, 255].repeat(16 * 12);
        let mut e = Enhancer::new();
        for mode in [Mode::Hd, Mode::Depth] {
            let (out, w, h, scale) = owned(e.apply(mode, &rgba, &[2; 16 * 12], 16, 12));
            assert_eq!((w, h, scale), (64, 48, 4));
            assert!(out.chunks_exact(4).all(|c| c == [40, 90, 200, 255]));
        }
    }

    /// One MMPX pass over a one-pixel diagonal line, worked through the rules by hand: every
    /// pixel of the line stays a 2×2 block (no rule fires for it: its four edge neighbours are
    /// all the background), and each background pixel that touches two line pixels by its
    /// edges (D = H or B = F, the 1:1 slope rule) gives the corner between them to the line.
    #[test]
    fn enhance_diagonal_line_follows_the_slope_rule() {
        let (rgba, w, h) = art(&["......", ".#....", "..#...", "...#..", "....#.", "......"]);
        let src: Vec<u32> = rgba.chunks_exact(4).map(|c| u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2])).collect();
        let mut dst = vec![0; w * h * 4];
        mmpx_2x(&src, w, h, &mut dst, &mut Vec::new());
        let mut out = vec![0; dst.len() * 4];
        unpack_row(&dst, &mut out);
        let want = [
            "............",
            "............",
            "..##........",
            "..###.......",
            "...###......",
            "....###.....",
            ".....###....",
            "......###...",
            ".......###..",
            "........##..",
            "............",
            "............",
        ];
        assert_eq!(text(&out, 12), want);
    }

    #[test]
    fn enhance_layers_keep_same_colours_apart() {
        let (rgba, w, h) = art(&["......", "......", "..#...", "...#..", "......", "......"]);
        let mut same = vec![0u8; w * h];
        let mut e = Enhancer::new();
        let (merged, ..) = owned(e.apply(Mode::Hd, &rgba, &same, w, h));
        // One of the two white pixels is a sprite.
        same[2 * w + 2] = 1;
        let (apart, bw, bh, scale) = owned(e.apply(Mode::Hd, &rgba, &same, w, h));
        assert_eq!((bw, bh, scale), (24, 24, 4));
        // Different layers: nothing joins them, each stays a hard-edged 4×4 block.
        assert_eq!(text(&apart, bw), text(&nearest(&rgba, w, h, 4), bw));
        // Equal layers: the line is joined across the corner.
        assert_ne!(merged, apart);
        let white = |px: &[u8]| px.chunks_exact(4).filter(|c| *c == W).count();
        assert!(white(&merged) > white(&apart));
        let at = |x: usize, y: usize| &merged[(y * bw + x) * 4..][..4];
        assert_eq!(at(12, 11), W, "the corner right of the upper pixel");
        assert_eq!(at(11, 12), W, "the corner below the upper pixel");
    }

    #[test]
    fn enhance_output_size_follows_the_row_count() {
        let mut e = Enhancer::new();
        let rgba = vec![7u8; 320 * 448 * 4];
        let layers = vec![0u8; 320 * 448];
        let out = e.apply(Mode::Hd, &rgba[..256 * 240 * 4], &layers[..256 * 240], 256, 240);
        assert_eq!((out.w, out.h, out.scale, out.rgba.len()), (1024, 960, 4, 1024 * 960 * 4));
        let out = e.apply(Mode::Depth, &rgba[..320 * 224 * 4], &layers[..320 * 224], 320, 224);
        assert_eq!((out.w, out.h, out.scale, out.rgba.len()), (1280, 896, 4, 1280 * 896 * 4));
        let out = e.apply(Mode::Hd, &rgba, &layers, 320, 448);
        assert_eq!((out.w, out.h, out.scale, out.rgba.len()), (640, 896, 2, 640 * 896 * 4));
        // A missing layer plane is a flat one.
        let out = e.apply(Mode::Depth, &rgba, &[], 320, 448);
        assert_eq!((out.w, out.h, out.scale), (640, 896, 2));
    }

    #[test]
    fn enhance_depth_darkens_only_below_and_right_of_a_higher_layer() {
        // A flat background with a 6×6 sprite at (8, 8).
        let (w, h) = (24, 24);
        let sprite = |x: usize, y: usize| (8..14).contains(&x) && (8..14).contains(&y);
        let mut rgba = Vec::new();
        let mut layers = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(if sprite(x, y) { &[250, 250, 0, 255] } else { &[100, 150, 200, 255] });
                layers.push(u8::from(sprite(x, y)));
            }
        }
        let mut e = Enhancer::new();
        let (hd, bw, bh, _) = owned(e.apply(Mode::Hd, &rgba, &layers, w, h));
        let (depth, ..) = owned(e.apply(Mode::Depth, &rgba, &layers, w, h));
        assert_eq!(hd.len(), depth.len());
        let mut changed = 0;
        for y in 0..bh {
            for x in 0..bw {
                let (a, b) = (&hd[(y * bw + x) * 4..][..4], &depth[(y * bw + x) * 4..][..4]);
                if a == b {
                    continue;
                }
                changed += 1;
                assert!(!sprite(x / 4, y / 4), "the sprite shadows itself at {x},{y}");
                assert!(b[0] < a[0] && b[1] < a[1] && b[2] < a[2] && b[3] == 255, "not darker at {x},{y}");
                // The offset is 6 output pixels, the blur 4 and the sampling of the half-size mask
                // up to 2 more: the shadow reaches from 1 to 11 pixels past the sprite's edges,
                // down and to the right only.
                assert!((33..67).contains(&x) && (33..67).contains(&y), "a shadow up or left of the sprite at {x},{y}");
            }
        }
        assert!(changed > 100, "{changed} pixels in shadow");
        // Deep in the shadow the colour is down to 60 %.
        assert_eq!(&depth[(56 * bw + 56) * 4..][..4], [60, 90, 120, 255]);
        // The edge is soft: further out it is lighter but still shadowed.
        let edge = &depth[(56 * bw + 63) * 4..][..4];
        assert!(edge[0] > 60 && edge[0] < 100, "{edge:?}");
    }

    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01B3))
    }

    /// Four colours and four layers from a fixed generator, in 1- to 4-pixel runs: every rule gets its chance.
    fn noise_shot(w: usize, h: usize) -> Shot {
        let mut seed = 0x9E37_79B9u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 20
        };
        let palette = [[0, 0, 0, 255], [255, 255, 255, 255], [200, 40, 40, 255], [40, 60, 220, 255]];
        let (mut rgba, mut layers) = (Vec::new(), Vec::new());
        let (mut colour, mut layer, mut run) = (0, 0, 0);
        for _ in 0..w * h {
            if run == 0 {
                let n = next();
                (colour, layer, run) = (n as usize & 3, (n >> 2 & 3) as u8, n >> 4 & 3);
            } else {
                run -= 1;
            }
            rgba.extend_from_slice(&palette[colour]);
            layers.push(layer);
        }
        (rgba, layers, w, h)
    }

    /// The pictures whose output is pinned: both console sizes, noise, and a tall one (magnified 2 times).
    fn pin_shots() -> [Shot; 4] {
        [drawn_shot(256, 240), drawn_shot(320, 224), noise_shot(64, 48), noise_shot(48, 250)]
    }

    fn pins(mode: Mode) -> Vec<u64> {
        let mut e = Enhancer::new();
        pin_shots().iter().map(|(rgba, layers, w, h)| fnv(e.apply(mode, rgba, layers, *w, *h).rgba)).collect()
    }

    /// The Hd picture, byte for byte, as it was before the work was split into bands.
    #[test]
    fn enhance_hd_output_is_pinned() {
        let sums = pins(Mode::Hd);
        assert_eq!(sums, [0x484D_FC96_9008_311D, 0x8726_578A_88B0_4A4D, 0x2402_5786_5CFB_9655, 0xC577_9599_62E6_81BC], "{sums:#X?}");
    }

    /// The Depth picture is the same bytes on every machine.
    #[test]
    fn enhance_depth_output_is_pinned() {
        let sums = pins(Mode::Depth);
        assert_eq!(sums, [0x5E01_2F6D_15A4_B03D, 0x0F95_9E16_C5FD_1E69, 0x3836_1D88_2D09_F5CE, 0xB0AC_8123_2E58_417C], "{sums:#X?}");
    }

    /// 1, 2, 3 and 4 bands give the same bytes, on a game picture when one is on this disk.
    #[test]
    fn enhance_bands_give_equal_output() {
        let mut shots = pin_shots().to_vec();
        shots.extend(md_shot("cave-story-md.gen", 600));
        for (rgba, layers, w, h) in &shots {
            for mode in [Mode::Hd, Mode::Depth] {
                let mut e = Enhancer::new();
                e.bands = 1;
                let one = owned(e.apply(mode, rgba, layers, *w, *h));
                for bands in 2..=BANDS {
                    e.bands = bands;
                    assert!(one == owned(e.apply(mode, rgba, layers, *w, *h)), "{mode:?} {w}x{h} in {bands} bands");
                }
            }
        }
    }

    #[test]
    fn enhance_is_deterministic() {
        let (w, h) = (40, 30);
        let mut seed = 0x2545_F491u32;
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 24
        };
        let palette = [[0, 0, 0, 255], [255, 255, 255, 255], [200, 40, 40, 255], [40, 60, 220, 255]];
        let rgba: Vec<u8> = (0..w * h).flat_map(|_| palette[next() as usize & 3]).collect();
        let layers: Vec<u8> = (0..w * h).map(|_| (next() & 3) as u8).collect();
        let mut one = Enhancer::new();
        let mut two = Enhancer::new();
        for mode in [Mode::Hd, Mode::Depth] {
            let first = owned(one.apply(mode, &rgba, &layers, w, h));
            // A warm enhancer that has seen another size, and a fresh one.
            one.apply(Mode::Depth, &rgba[..20 * 10 * 4], &layers[..20 * 10], 20, 10);
            assert_eq!(first, owned(one.apply(mode, &rgba, &layers, w, h)));
            assert_eq!(first, owned(two.apply(mode, &rgba, &layers, w, h)));
        }
    }

    type Shot = (Vec<u8>, Vec<u8>, usize, usize);

    /// The NES game's picture after `frames` frames, Start held for 6 frames from each of `starts`.
    fn nes_shot(frames: usize, starts: &[usize]) -> Option<Shot> {
        let rom = std::fs::read(NES_ROM).ok()?;
        let mut console = ebobo_console::Console::new(&rom, None).ok()?;
        for frame in 0..frames {
            let start = starts.iter().any(|s| (*s..*s + 6).contains(&frame));
            console.run_frame([if start { u16::from(ebobo_console::nes::pad::START) } else { 0 }, 0, 0, 0]);
        }
        let (mut rgba, mut layers) = (Vec::new(), Vec::new());
        let (w, h) = console.frame_rgba(&mut rgba);
        console.frame_layers(&mut layers);
        Some((rgba, layers, w, h))
    }

    /// A Mega Drive homebrew's picture after `frames` frames.
    fn md_shot(file: &str, frames: usize) -> Option<Shot> {
        let rom = std::fs::read(format!("{MD_ROMS}/{file}")).ok()?;
        let mut md = ebobo_console::md::Md::new(&rom, None).ok()?;
        for _ in 0..frames {
            md.run_frame([0u16; 4]);
        }
        let (mut rgba, mut layers) = (Vec::new(), Vec::new());
        let (w, h) = md.frame_rgba(&mut rgba);
        md.frame_layers(&mut layers);
        (w > 0).then_some((rgba, layers, w, h))
    }

    /// A stand-in picture when a ROM is missing: round shapes of four colours on stripes.
    fn drawn_shot(w: usize, h: usize) -> Shot {
        let (mut rgba, mut layers) = (Vec::new(), Vec::new());
        for y in 0..h {
            for x in 0..w {
                let (cx, cy) = ((x % 32) as i32 - 16, (y % 32) as i32 - 16);
                let ball = cx * cx + cy * cy < 100;
                let stripe = (x + y) / 8 % 2 == 0;
                rgba.extend_from_slice(match (ball, stripe) {
                    (true, _) => &[230, 60, 40, 255],
                    (false, true) => &[30, 90, 60, 255],
                    (false, false) => &[20, 40, 110, 255],
                });
                layers.push(u8::from(ball));
            }
        }
        (rgba, layers, w, h)
    }

    /// `cargo test --release -p ebobo enhance_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn enhance_speed() {
        let shots = [
            ("NES 256x240", nes_shot(420, &[]).map(|s| (s, "game")).unwrap_or((drawn_shot(256, 240), "drawn"))),
            ("NES 256x240 play", nes_shot(2700, &[]).map(|s| (s, "game")).unwrap_or((drawn_shot(256, 240), "drawn"))),
            ("MD 320x224", md_shot("sgdk-platformer.bin", 300).map(|s| (s, "game")).unwrap_or((drawn_shot(320, 224), "drawn"))),
            ("MD 320x224 cave", md_shot("cave-story-md.gen", 600).map(|s| (s, "game")).unwrap_or((drawn_shot(320, 224), "drawn"))),
        ];
        let mut e = Enhancer::new();
        for (name, ((rgba, layers, w, h), kind)) in &shots {
            for (mode, bands) in [(Mode::Hd, 1), (Mode::Hd, BANDS), (Mode::Depth, 1), (Mode::Depth, BANDS)] {
                e.bands = bands;
                let mut sum = 0u64;
                e.apply(mode, rgba, layers, *w, *h);
                let mut times = Vec::new();
                for _ in 0..200 {
                    let t = Instant::now();
                    let out = e.apply(mode, rgba, layers, *w, *h);
                    sum += u64::from(out.rgba[out.rgba.len() / 2]);
                    times.push(t.elapsed().as_secs_f64() * 1000.0);
                }
                times.sort_by(f64::total_cmp);
                println!(
                    "{name} ({w}x{h}, {kind} picture) {:5} {bands} band(s): fastest {:.2} ms, median {:.2} ms [{sum}]",
                    mode.name(),
                    times[0],
                    times[100]
                );
            }
        }
    }

    fn crop(rgba: &[u8], w: usize, x0: usize, y0: usize, cw: usize, ch: usize) -> Vec<u8> {
        (y0..y0 + ch).flat_map(|y| rgba[(y * w + x0) * 4..][..cw * 4].iter().copied()).collect()
    }

    fn save(path: &str, rgba: Vec<u8>, w: usize, h: usize) {
        let png = ebobo_art::image::encode_png(&ebobo_art::Picture { w: w as u32, h: h as u32, rgba });
        std::fs::write(path, png).unwrap();
    }

    /// The 64×48 window (on an 8-pixel grid) with the most going on: borders between layers
    /// count four times as much as borders between colours.
    fn busiest(rgba: &[u8], layers: &[u8], w: usize, h: usize) -> (usize, usize) {
        let mut best = (0, (0, 0));
        for y0 in (0..=h.saturating_sub(48)).step_by(8) {
            for x0 in (0..=w.saturating_sub(64)).step_by(8) {
                let mut score = 0;
                for y in y0..y0 + 48 {
                    for x in x0..x0 + 63 {
                        let i = y * w + x;
                        score += 4 * usize::from(layers[i] != layers[i + 1]) + usize::from(rgba[i * 4..i * 4 + 3] != rgba[i * 4 + 4..i * 4 + 7]);
                    }
                }
                if score > best.0 {
                    best = (score, (x0, y0));
                }
            }
        }
        best.1
    }

    /// The game named by `EBOBO_ENHANCE_ROM` (NES or Mega Drive) after `EBOBO_ENHANCE_FRAMES`
    /// frames, with the pads of `EBOBO_ENHANCE_INPUT`: lines `<frame> <p1 hex> [p2 p3 p4]` in
    /// the console's pad bits, each held until the next line (the headless runners' format).
    fn env_shot() -> Option<Shot> {
        let rom = std::fs::read(std::env::var("EBOBO_ENHANCE_ROM").ok()?).ok()?;
        let frames: usize = std::env::var("EBOBO_ENHANCE_FRAMES").ok()?.parse().ok()?;
        let script = std::env::var("EBOBO_ENHANCE_INPUT").ok().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        let mut holds: Vec<(usize, [u16; 4])> = Vec::new();
        for line in script.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let mut words = line.split_whitespace();
            let Some(Ok(frame)) = words.next().map(str::parse) else { continue };
            let mut pads = [0u16; 4];
            for (pad, word) in pads.iter_mut().zip(words) {
                *pad = u16::from_str_radix(word, 16).unwrap_or(0);
            }
            holds.push((frame, pads));
        }
        let mut console = ebobo_console::Console::new(&rom, None).ok()?;
        let mut pads = [0u16; 4];
        for frame in 0..frames {
            if let Some((_, p)) = holds.iter().find(|(f, _)| *f == frame) {
                pads = *p;
            }
            console.run_frame(pads);
        }
        let (mut rgba, mut layers) = (Vec::new(), Vec::new());
        let (w, h) = console.frame_rgba(&mut rgba);
        console.frame_layers(&mut layers);
        Some((rgba, layers, w, h))
    }

    /// `cargo test --release -p ebobo enhance_snapshots -- --ignored --nocapture`: writes
    /// `/tmp/ebobo-enhance-<name>-{raw,hd,depth,crop}.png` for the games that are on this disk,
    /// and as `env` for the game `env_shot` names.
    #[test]
    #[ignore]
    fn enhance_snapshots() {
        let shots = [
            ("nes-title", nes_shot(420, &[])),
            // The attract demo: real play, no input.
            ("nes-play", nes_shot(2700, &[])),
            ("md-platformer", md_shot("sgdk-platformer.bin", 300)),
            ("md-cave", md_shot("cave-story-md.gen", 600)),
            ("env", env_shot()),
        ];
        let mut e = Enhancer::new();
        for (name, shot) in shots {
            let Some((rgba, layers, w, h)) = shot else { continue };
            let (hd, bw, bh, scale) = owned(e.apply(Mode::Hd, &rgba, &layers, w, h));
            let (depth, ..) = owned(e.apply(Mode::Depth, &rgba, &layers, w, h));
            let scale = scale as usize;
            let raw = nearest(&rgba, w, h, scale);
            let mut count = [0usize; 4];
            layers.iter().for_each(|l| count[usize::from(*l).min(3)] += 1);
            let shadowed = hd.chunks_exact(4).zip(depth.chunks_exact(4)).filter(|(a, b)| a != b).count();
            let (x0, y0) = busiest(&rgba, &layers, w, h);
            println!("{name}: {w}x{h} x{scale}, layers {count:?}, {shadowed} pixels in shadow, crop at {x0},{y0}");

            // The crop sheet: the same 64×48 source region of the three at 8 times, side by side.
            let (cw, ch, gap) = (64 * 8, 48 * 8, 8);
            let mut sheet = vec![32u8; (3 * cw + 2 * gap) * ch * 4];
            sheet.chunks_exact_mut(4).for_each(|p| p[3] = 255);
            for (n, big) in [&raw, &hd, &depth].into_iter().enumerate() {
                let part = crop(big, bw, x0 * scale, y0 * scale, 64 * scale, 48 * scale);
                let part = nearest(&part, 64 * scale, 48 * scale, 8 / scale);
                for y in 0..ch {
                    let to = (y * (3 * cw + 2 * gap) + n * (cw + gap)) * 4;
                    sheet[to..to + cw * 4].copy_from_slice(&part[y * cw * 4..][..cw * 4]);
                }
            }
            save(&format!("/tmp/ebobo-enhance-{name}-crop.png"), sheet, 3 * cw + 2 * gap, ch);
            save(&format!("/tmp/ebobo-enhance-{name}-raw.png"), raw, bw, bh);
            save(&format!("/tmp/ebobo-enhance-{name}-hd.png"), hd, bw, bh);
            save(&format!("/tmp/ebobo-enhance-{name}-depth.png"), depth, bw, bh);
        }
    }
}
