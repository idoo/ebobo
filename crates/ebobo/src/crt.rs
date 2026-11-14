//! Picture effects ("Screen effect"): presets, CPU-made masks, the curvature mesh and its
//! inverse, and the CRT power-on curve. Pure functions; `video.rs` composes them on the GPU.
//!
//! Composition per frame (see `Video::present`): picture → (colour bleed) → glow added →
//! mask multiplied → copied or warped onto the screen with vignette vertex colours. Masks
//! and meshes are rebuilt only when the destination size changes.

/// Source picture rows/columns the scanlines and mask cells align to.
const SRC_W: f32 = 256.0;
const SRC_H: f32 = 240.0;

/// Switchable picture effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    /// Plain pixels: today's output, untouched.
    #[default]
    Off,
    /// Consumer CRT TV: curved glass, soft scanlines, slot mask, glow, colour bleed.
    CrtTv,
    /// Flat professional monitor: sharp scanlines, aperture grille, mild glow.
    Pvm,
    /// Handheld LCD: pixel grid, slight ghosting.
    Lcd,
}

/// Per-preset knobs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Barrel strength (0 = flat).
    pub curve: f32,
    /// Corner/edge darkening strength (0 = none).
    pub vignette: f32,
    /// Horizontal composite bleed: alpha of a copy shifted by ~half a source pixel.
    pub bleed: f32,
    /// Weight of the previous frame (LCD ghosting).
    pub ghost: f32,
    /// Blur width of the glow copy in source pixels (bigger = softer).
    pub glow_blur: u32,
}

impl Effect {
    pub const ALL: [Effect; 4] = [Effect::Off, Effect::CrtTv, Effect::Pvm, Effect::Lcd];

    pub fn next(self) -> Effect {
        let i = Self::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Effect {
        let i = Self::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// CLI / config name.
    pub fn name(self) -> &'static str {
        match self {
            Effect::Off => "off",
            Effect::CrtTv => "crt",
            Effect::Pvm => "pvm",
            Effect::Lcd => "lcd",
        }
    }

    pub fn parse(s: &str) -> Option<Effect> {
        Self::ALL.into_iter().find(|e| e.name().eq_ignore_ascii_case(s.trim()))
    }

    pub fn params(self) -> Params {
        let none = Params { curve: 0.0, vignette: 0.0, bleed: 0.0, ghost: 0.0, glow_blur: 8 };
        match self {
            Effect::Off => none,
            Effect::CrtTv => Params { curve: 0.06, vignette: 0.35, bleed: 0.2, ghost: 0.0, glow_blur: 5 },
            Effect::Pvm => Params { vignette: 0.08, glow_blur: 4, ..none },
            Effect::Lcd => Params { ghost: 0.35, ..none },
        }
    }

    /// The power-on animation applies (tube displays only).
    pub fn has_power_on(self) -> bool {
        matches!(self, Effect::CrtTv | Effect::Pvm)
    }
}

/// Mask multiplier (0-255 per channel, RGBA bytes) for a `dw`×`dh` destination and the
/// additive glow weight (0..1) that brings the average brightness back to ≈ 1.0.
pub struct Mask {
    pub rgba: Vec<u8>,
    pub glow: f32,
}

/// Source-row phase (0..1 within a source row) of destination row `y` out of `dh`.
fn row_phase(y: u32, dh: u32) -> f32 {
    let f = (y as f32 + 0.5) * SRC_H / dh as f32;
    f - f.floor()
}

fn col_phase(x: u32, dw: u32) -> f32 {
    let f = (x as f32 + 0.5) * SRC_W / dw as f32;
    f - f.floor()
}

/// Scanline multiplier for one destination row (same for every column).
fn scanline(effect: Effect, y: u32, dh: u32) -> f32 {
    // Less than 2 destination rows per source row: scanlines would alias into bands.
    let rows = dh as f32 / SRC_H;
    if rows < 2.0 {
        return 1.0;
    }
    let d = row_phase(y, dh) - 0.5; // -0.5..0.5 from the beam centre
    match effect {
        // Soft Gaussian beam; the gap between rows keeps ~half the light.
        Effect::CrtTv => 0.5 + 0.5 * (-(d / 0.3).powi(2)).exp(),
        // Sharp: full beam over 70 % of the row, dark gap.
        Effect::Pvm => {
            let edge = (d.abs() - 0.3) * rows; // destination pixels past the beam edge
            1.0 - 0.7 * edge.clamp(0.0, 1.0)
        }
        Effect::Lcd => {
            // Thin dark line at the bottom of each source row (about one destination pixel).
            let line = (1.0 / rows).min(0.2);
            if row_phase(y, dh) > 1.0 - line { 0.72 } else { 1.0 }
        }
        Effect::Off => 1.0,
    }
}

/// Per-channel mask for one destination column (and row, for the slot mask stagger).
fn phosphor(effect: Effect, x: u32, y: u32, dw: u32) -> [f32; 3] {
    let cols = dw as f32 / SRC_W;
    match effect {
        // 3-pixel triads need ≥ 2 px per source column to not swamp the picture.
        Effect::CrtTv if cols >= 2.0 => {
            // Slot mask: RGB triads, every other triad column shifted by 2 rows, thin dark
            // slot breaks between cells.
            let triad = x / 3;
            let shift = if triad % 2 == 1 { 2 } else { 0 };
            let slot = (y + shift) % 4 == 0;
            let mut c = [0.72f32; 3];
            c[(x % 3) as usize] = 1.0;
            if slot {
                c.iter_mut().for_each(|v| *v *= 0.85);
            }
            c
        }
        Effect::Pvm if cols >= 2.0 => {
            let mut c = [0.68f32; 3];
            c[(x % 3) as usize] = 1.0;
            c
        }
        Effect::Lcd => {
            let line = (1.0 / cols).min(0.2);
            if cols >= 2.0 && col_phase(x, dw) > 1.0 - line { [0.72; 3] } else { [1.0; 3] }
        }
        _ => [1.0; 3],
    }
}

/// Builds the mask for `effect` at `dw`×`dh` (empty for `Off`).
pub fn mask(effect: Effect, dw: u32, dh: u32) -> Mask {
    if effect == Effect::Off || dw == 0 || dh == 0 {
        return Mask { rgba: Vec::new(), glow: 0.0 };
    }
    let mut rgba = vec![0u8; (dw * dh * 4) as usize];
    let mut sum = 0.0f64;
    for y in 0..dh {
        let s = scanline(effect, y, dh);
        let row = &mut rgba[(y * dw * 4) as usize..((y + 1) * dw * 4) as usize];
        for x in 0..dw {
            let p = phosphor(effect, x, y, dw);
            let px = &mut row[(x * 4) as usize..(x * 4 + 4) as usize];
            for c in 0..3 {
                let v = (s * p[c]).clamp(0.0, 1.0);
                sum += v as f64;
                px[c] = (v * 255.0 + 0.5) as u8;
            }
            px[3] = 255;
        }
    }
    let mean = (sum / (dw as f64 * dh as f64 * 3.0)) as f32;
    Mask { rgba, glow: glow_for(effect, mean) }
}

/// Glow weight compensating the mask's average darkening (`mean`), so a mid-grey frame
/// keeps its brightness: mean + glow ≈ 1.
fn glow_for(effect: Effect, mean: f32) -> f32 {
    match effect {
        Effect::Off => 0.0,
        // The LCD grid is barely darker; a halo would look wrong on an LCD.
        Effect::Lcd => (1.0 - mean).clamp(0.0, 0.15),
        _ => (1.0 - mean).clamp(0.0, 0.6),
    }
}

/// Average gain of `mask` × picture + glow on a uniform grey frame.
pub fn mid_grey_gain(m: &Mask) -> f32 {
    if m.rgba.is_empty() {
        return 1.0;
    }
    let sum: u64 = m.rgba.chunks_exact(4).map(|p| p[0] as u64 + p[1] as u64 + p[2] as u64).sum();
    let mean = sum as f32 / (m.rgba.len() / 4 * 3) as f32 / 255.0;
    mean + m.glow
}

/// Barrel warp of the screen: screen position (0..1) → picture position (0..1). Picture
/// positions outside 0..1 are the black glass corners. `k` = 0 is the identity.
pub fn warp(x: f32, y: f32, k: f32) -> (f32, f32) {
    if k == 0.0 {
        return (x, y);
    }
    let (cx, cy) = (x * 2.0 - 1.0, y * 2.0 - 1.0);
    let (fx, fy) = (1.0 + k * cy * cy, 1.0 + k * cx * cx);
    // Edge centres stay on the edges; the picture bulges toward the corners, which fall
    // off the glass.
    ((cx * fx + 1.0) * 0.5, (cy * fy + 1.0) * 0.5)
}

/// Inverse of `warp`: picture position → screen position (Newton iterations).
pub fn unwarp(u: f32, v: f32, k: f32) -> (f32, f32) {
    if k == 0.0 {
        return (u, v);
    }
    let (mut x, mut y) = (u, v);
    for _ in 0..12 {
        let (wx, wy) = warp(x, y, k);
        let (ex, ey) = (wx - u, wy - v);
        if ex.abs() < 1e-6 && ey.abs() < 1e-6 {
            break;
        }
        // Numeric Jacobian.
        let h = 1e-3;
        let (ax, ay) = warp(x + h, y, k);
        let (bx, by) = warp(x, y + h, k);
        let (j00, j10, j01, j11) = ((ax - wx) / h, (ay - wy) / h, (bx - wx) / h, (by - wy) / h);
        let det = j00 * j11 - j01 * j10;
        if det.abs() < 1e-9 {
            break;
        }
        x -= (j11 * ex - j01 * ey) / det;
        y -= (-j10 * ex + j00 * ey) / det;
    }
    (x, y)
}

/// One mesh vertex: destination pixel position, picture coordinate (0..1) and brightness.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeshVertex {
    pub x: f32,
    pub y: f32,
    pub u: f32,
    pub v: f32,
    pub shade: f32,
}

/// Grid density of the curvature mesh (cells per side).
pub const MESH_CELLS: u32 = 32;

/// Mesh drawing the picture into `dest` (x, y, w, h): uniform picture grid placed at the
/// unwarped screen positions, shaded by the vignette. Returns vertices and triangle indices.
pub fn mesh(dest: (f32, f32, f32, f32), p: Params) -> (Vec<MeshVertex>, Vec<u32>) {
    let n = MESH_CELLS;
    let (dx, dy, dw, dh) = dest;
    let mut verts = Vec::with_capacity(((n + 1) * (n + 1)) as usize);
    for j in 0..=n {
        for i in 0..=n {
            let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
            let (sx, sy) = unwarp(u, v, p.curve);
            verts.push(MeshVertex { x: dx + sx * dw, y: dy + sy * dh, u, v, shade: vignette(u, v, p.vignette) });
        }
    }
    let mut idx = Vec::with_capacity((n * n * 6) as usize);
    for j in 0..n {
        for i in 0..n {
            let a = j * (n + 1) + i;
            let b = a + 1;
            let c = a + n + 1;
            let d = c + 1;
            idx.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    (verts, idx)
}

/// Brightness at picture position `u`, `v`: 1 in the middle, darker toward the edges.
pub fn vignette(u: f32, v: f32, strength: f32) -> f32 {
    if strength == 0.0 {
        return 1.0;
    }
    let e = (16.0 * u * v * (1.0 - u) * (1.0 - v)).clamp(0.0, 1.0);
    1.0 - strength * (1.0 - e.powf(0.35))
}

/// Length of the power-on animation.
pub const POWER_ON_MS: f32 = 250.0;

/// Power-on state `t_ms` after switching on: (`open`: visible fraction of the height,
/// centred; `flash`: white added on top, 0..1). `None` once finished.
pub fn power_on(t_ms: f32) -> Option<(f32, f32)> {
    if !(0.0..POWER_ON_MS).contains(&t_ms) {
        return None;
    }
    let t = t_ms / POWER_ON_MS;
    // A bright line for the first 15 %, then it opens with ease-out by 60 %, then settles.
    let open_t = ((t - 0.15) / 0.45).clamp(0.0, 1.0);
    let open = 0.006 + 0.994 * (1.0 - (1.0 - open_t).powi(3));
    let flash = (1.0 - t).powi(2) * 0.9;
    Some((open, flash))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_cycle_and_names() {
        assert_eq!(Effect::Off.next(), Effect::CrtTv);
        assert_eq!(Effect::Off.prev(), Effect::Lcd);
        for e in Effect::ALL {
            assert_eq!(e.next().prev(), e);
            assert_eq!(Effect::parse(e.name()), Some(e));
        }
        assert_eq!(Effect::parse("CRT"), Some(Effect::CrtTv));
        assert_eq!(Effect::parse("vhs"), None);
    }

    #[test]
    fn off_is_identity() {
        let p = Effect::Off.params();
        assert_eq!(p.curve, 0.0);
        for &(x, y) in &[(0.0, 0.0), (0.3, 0.7), (0.999, 0.5)] {
            assert_eq!(warp(x, y, p.curve), (x, y));
            assert_eq!(unwarp(x, y, p.curve), (x, y));
        }
        assert!(mask(Effect::Off, 1280, 800).rgba.is_empty());
        for e in [Effect::Pvm, Effect::Lcd] {
            assert_eq!(e.params().curve, 0.0, "{e:?} is flat");
        }
    }

    #[test]
    fn inverse_warp_round_trips() {
        let k = Effect::CrtTv.params().curve;
        for j in 0..=20 {
            for i in 0..=20 {
                let (x, y) = (i as f32 / 20.0, j as f32 / 20.0);
                let (u, v) = warp(x, y, k);
                let (bx, by) = unwarp(u, v, k);
                // Within 1 canvas pixel (and far better).
                assert!(((bx - x) * SRC_W).abs() < 1.0 && ((by - y) * SRC_H).abs() < 1.0, "{x},{y}");
                assert!((bx - x).abs() < 1e-3 && (by - y).abs() < 1e-3);
            }
        }
        // Centre fixed, edge centres stay on the edges, corners fall off the glass.
        let (cx, cy) = warp(0.5, 0.5, k);
        assert!((cx - 0.5).abs() < 1e-6 && (cy - 0.5).abs() < 1e-6);
        let (u, _) = warp(0.0, 0.5, k);
        assert!(u.abs() < 0.02, "{u}");
        let (u, v) = warp(0.0, 0.0, k);
        assert!(u < 0.0 && v < 0.0);
    }

    #[test]
    fn scanlines_follow_source_rows() {
        for dh in [800u32, 720] {
            for effect in [Effect::CrtTv, Effect::Pvm, Effect::Lcd] {
                let m = mask(effect, 1024, dh);
                let at = |y: u32| m.rgba[(y * 1024 * 4) as usize + 4 * 7]; // any column
                // The brightest row of every source row is near its centre, the darkest at
                // its edges, whatever the fractional scale.
                for src in 0..240u32 {
                    let rows: Vec<u32> = (0..dh).filter(|&y| ((y as f32 + 0.5) * 240.0 / dh as f32) as u32 == src).collect();
                    let best = *rows.iter().max_by_key(|&&y| (at(y), u32::MAX - y)).unwrap();
                    let phase = row_phase(best, dh);
                    if effect == Effect::Lcd {
                        assert!(phase < 0.8, "{effect:?} dh {dh} src {src}: line not at row end");
                    } else {
                        assert!((phase - 0.5).abs() <= 0.5 * 240.0 / dh as f32 + 0.21, "{effect:?} dh {dh} src {src} phase {phase}");
                    }
                    assert!(rows.len() >= 2, "{dh}");
                }
            }
        }
    }

    #[test]
    fn mesh_covers_dest() {
        let dest = (213.0, 0.0, 853.0, 800.0);
        for effect in Effect::ALL {
            let (v, idx) = mesh(dest, effect.params());
            assert_eq!(idx.len(), (MESH_CELLS * MESH_CELLS * 6) as usize);
            let minx = v.iter().map(|p| p.x).fold(f32::MAX, f32::min);
            let maxx = v.iter().map(|p| p.x).fold(f32::MIN, f32::max);
            let miny = v.iter().map(|p| p.y).fold(f32::MAX, f32::min);
            let maxy = v.iter().map(|p| p.y).fold(f32::MIN, f32::max);
            // Inside the rect, spanning (almost) all of it: the edge centres touch the edges.
            assert!(minx >= 213.0 - 0.5 && maxx <= 213.0 + 853.0 + 0.5, "{effect:?}");
            assert!(miny >= -0.5 && maxy <= 800.5, "{effect:?}");
            assert!(minx < 213.0 + 853.0 * 0.02 && maxx > 213.0 + 853.0 * 0.98, "{effect:?}");
            assert!(miny < 800.0 * 0.02 && maxy > 800.0 * 0.98, "{effect:?}");
            assert!(v.iter().all(|p| p.shade > 0.4 && p.shade <= 1.0));
        }
        // Flat presets put the corners exactly on the rect's corners.
        let (v, _) = mesh(dest, Effect::Pvm.params());
        assert_eq!((v[0].x, v[0].y), (213.0, 0.0));
    }

    #[test]
    fn brightness_stays_close_to_off() {
        for (dw, dh) in [(1280u32, 800u32), (853, 800), (768, 720), (256, 240)] {
            for effect in [Effect::CrtTv, Effect::Pvm, Effect::Lcd] {
                let gain = mid_grey_gain(&mask(effect, dw, dh));
                assert!((gain - 1.0).abs() < 0.1, "{effect:?} {dw}x{dh}: {gain}");
            }
        }
    }

    #[test]
    fn power_on_curve() {
        let (open, flash) = power_on(0.0).unwrap();
        assert!(open < 0.02 && flash > 0.8);
        let (mid, _) = power_on(100.0).unwrap();
        assert!(mid > 0.2 && mid < 1.0);
        let (late, flash) = power_on(240.0).unwrap();
        assert!(late > 0.999 && flash < 0.01);
        assert_eq!(power_on(POWER_ON_MS), None);
        // Monotonic opening.
        let mut last = 0.0;
        for t in 0..250 {
            let (o, _) = power_on(t as f32).unwrap();
            assert!(o >= last);
            last = o;
        }
    }
}
