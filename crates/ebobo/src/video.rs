//! Window and frame presentation. Owner: platform slice.

use std::time::Instant;

use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::{FPoint, Rect};
use sdl2::render::{BlendMode, Canvas, Texture, TextureCreator, Vertex, VertexIndices};
use sdl2::video::{FullscreenType, Window, WindowContext};

use crate::config::ScreenMode;
use crate::crt::{self, Effect};
use crate::input::WindowPos;

const W: u32 = 256;
const H: u32 = 240;
/// Largest integer prescale for non-integer modes (8x = 2048x1920, enough for 4K).
const MAX_PRESCALE: u32 = 8;

/// SDL window + renderer + streaming 256x240 texture.
pub struct Video {
    // `unsafe_textures`: the textures are freed with the renderer; keep the creator alive.
    texture: Texture,
    /// Integer prescale target for non-integer modes: the picture is drawn into it with
    /// nearest-neighbour, then scaled to the screen with linear filtering (sharp, no
    /// shimmering). `None` until needed, or when render targets fail.
    prescaled: Option<(Texture, u32)>,
    /// Render targets work on this renderer (else plain nearest scaling).
    targets: bool,
    _creator: TextureCreator<WindowContext>,
    canvas: Canvas<Window>,
    mouse: sdl2::mouse::MouseUtil,
    /// Mouse cursor currently shown.
    cursor: bool,
    fullscreen: bool,
    screen: ScreenMode,
    /// Picture effect; `Off` draws exactly the plain path.
    effect: Effect,
    /// Effect textures for the current effect and destination rect.
    fx: Option<Fx>,
    /// Power-on animation start (tube effects only).
    power: Option<Instant>,
}

/// GPU resources of one effect at one destination rect; rebuilt when either changes.
struct Fx {
    key: (Effect, Rect),
    params: crt::Params,
    /// Destination-sized target the picture, bleed, mask and glow are composed in.
    scene: Texture,
    /// Multiplied over the scene (`BlendMode::Mod`).
    mask: Texture,
    /// Additive glow weight compensating the mask's darkening.
    glow: f32,
    /// Two-step linear downscale of the picture: the blurred glow copy.
    mid: Texture,
    small: Texture,
    /// Previous frames blended in (LCD ghosting).
    ghost: Texture,
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
}

impl Fx {
    /// SAFETY: the textures are not used again; the renderer is alive.
    unsafe fn destroy(self) {
        unsafe {
            self.scene.destroy();
            self.mask.destroy();
            self.mid.destroy();
            self.small.destroy();
            self.ghost.destroy();
        }
    }
}

impl Video {
    /// Creates the window (`scale` × 256x240 in windowed mode, desktop fullscreen when
    /// `fullscreen`); `screen` picks how the picture fills it.
    pub fn new(sdl: &sdl2::Sdl, fullscreen: bool, scale: u32, screen: ScreenMode) -> Result<Video, String> {
        let video = sdl.video()?;
        sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
        let scale = scale.clamp(1, 6);
        let make = |accelerated: bool| -> Result<Canvas<Window>, String> {
            let mut builder = video.window("ebobo", W * scale, H * scale);
            builder.position_centered().resizable().allow_highdpi();
            if fullscreen {
                builder.fullscreen_desktop();
            }
            let canvas = builder.build().map_err(|e| e.to_string())?.into_canvas();
            let canvas = if accelerated { canvas.accelerated() } else { canvas.software() };
            canvas.build().map_err(|e| e.to_string())
        };
        // Broken or missing GPU drivers (VMs, odd desktop sessions) must not stop the game:
        // a 256x240 picture is cheap to scale on the CPU.
        let mut canvas = match make(true) {
            Ok(canvas) => canvas,
            Err(e) => {
                eprintln!("ebobo: no GPU renderer ({e}), using software rendering");
                make(false)?
            }
        };
        canvas.set_draw_color(Color::RGB(0, 0, 0));
        canvas.clear();
        canvas.present();
        let targets = canvas.render_target_supported();
        let creator = canvas.texture_creator();
        let texture = creator
            .create_texture_streaming(PixelFormatEnum::ABGR8888, W, H)
            .map_err(|e| e.to_string())?;
        let mouse = sdl.mouse();
        // Hidden until the mouse moves (`set_cursor_visible`).
        mouse.show_cursor(false);
        Ok(Video {
            texture,
            prescaled: None,
            targets,
            _creator: creator,
            canvas,
            mouse,
            cursor: false,
            fullscreen,
            screen,
            effect: Effect::Off,
            fx: None,
            power: None,
        })
    }

    /// Picture effect; applies from the next frame.
    pub fn set_effect(&mut self, effect: Effect) {
        self.effect = effect;
        if !effect.has_power_on() {
            self.power = None;
        }
    }

    /// Starts the CRT turn-on animation (tube effects only; no-op otherwise). Never blocks:
    /// the game runs underneath and shows through as the picture opens.
    pub fn power_on(&mut self) {
        if self.effect.has_power_on() {
            self.power = Some(Instant::now());
        }
    }

    /// Shows one 256x240 RGBA8888 picture (game plus UI overlay already composited).
    pub fn present(&mut self, rgba: &[u8]) -> Result<(), String> {
        // Computed every frame from the drawable size, so resizes and fullscreen toggles apply.
        let out = self.canvas.output_size()?;
        self.compose(rgba, out, None)?;
        self.canvas.present();
        Ok(())
    }

    /// The picture as the screen shows it (effects included) composed at `size` (default:
    /// the drawable size): (width, height, RGBA bytes). Screenshots; needs render targets.
    pub fn capture(&mut self, rgba: &[u8], size: Option<(u32, u32)>) -> Result<(u32, u32, Vec<u8>), String> {
        let (w, h) = match size {
            Some(size) => size,
            None => self.canvas.output_size()?,
        };
        let mut shot = self
            ._creator
            .create_texture_target(PixelFormatEnum::ABGR8888, w, h)
            .map_err(|e| e.to_string())?;
        let px = self.compose(rgba, (w, h), Some(&mut shot));
        // SAFETY: not used again; the renderer is alive.
        unsafe { shot.destroy() };
        Ok((w, h, px?.ok_or_else(|| "no pixels".to_string())?))
    }

    /// Draws `rgba` into an `out`-sized output: the window, or `shot` (then read back).
    fn compose(&mut self, rgba: &[u8], out: (u32, u32), shot: Option<&mut Texture>) -> Result<Option<Vec<u8>>, String> {
        if rgba.len() < (W * H * 4) as usize {
            return Err(format!("frame too small: {} bytes", rgba.len()));
        }
        let dest = dest_rect(self.screen, out.0, out.1);
        self.texture
            .update(None, &rgba[..(W * H * 4) as usize], (W * 4) as usize)
            .map_err(|e| e.to_string())?;
        let k = prescale(dest.width(), dest.height());
        let pre = k > 1 && self.draw_prescaled(k);
        let fx = self.effect != Effect::Off && self.draw_fx(dest, pre);
        let power = self.power.and_then(|t| crt::power_on(t.elapsed().as_secs_f32() * 1000.0));
        if power.is_none() {
            self.power = None;
        }
        let src = match &self.prescaled {
            Some((target, _)) if pre => target,
            _ => &self.texture,
        };
        let fx = if fx { self.fx.as_ref() } else { None };
        let draw = |c: &mut Canvas<Window>| -> Result<(), String> {
            c.set_draw_color(Color::RGB(0, 0, 0));
            c.clear();
            match fx {
                Some(fx) if fx.params.curve > 0.0 || fx.params.vignette > 0.0 => {
                    c.render_geometry(&fx.vertices, Some(&fx.scene), VertexIndices::U32(&fx.indices))?
                }
                Some(fx) => c.copy(&fx.scene, None, dest)?,
                None => c.copy(src, None, dest)?,
            }
            if let Some((open, flash)) = power {
                draw_power(c, dest, open, flash)?;
            }
            Ok(())
        };
        match shot {
            None => draw(&mut self.canvas).map(|_| None),
            Some(target) => {
                let mut px = Ok(None);
                self.canvas
                    .with_texture_canvas(target, |c| {
                        px = draw(c).and_then(|_| c.read_pixels(None, PixelFormatEnum::ABGR8888).map(Some));
                    })
                    .map_err(|e| e.to_string())?;
                px
            }
        }
    }

    /// Composes the effect scene for `dest` (picture, bleed, mask, glow); false when render
    /// targets fail (then the plain picture is drawn).
    fn draw_fx(&mut self, dest: Rect, pre: bool) -> bool {
        if !self.targets {
            return false;
        }
        let key = (self.effect, dest);
        if self.fx.as_ref().map(|fx| fx.key) != Some(key) {
            if let Some(old) = self.fx.take() {
                // SAFETY: replaced below; the renderer is alive.
                unsafe { old.destroy() };
            }
            match self.make_fx(key) {
                Ok(fx) => self.fx = Some(fx),
                Err(e) => {
                    eprintln!("ebobo: picture effect unavailable ({e})");
                    self.targets = false;
                    return false;
                }
            }
        }
        let Video { canvas, texture, prescaled, fx, .. } = self;
        let fx = fx.as_mut().expect("made above");
        let p = fx.params;
        let (dw, dh) = (dest.width(), dest.height());
        let mut ok = true;
        let mut run = |target: &mut Texture, f: &mut dyn FnMut(&mut Canvas<Window>) -> Result<(), String>| {
            if let Err(e) = canvas.with_texture_canvas(target, |c| {
                if let Err(e) = f(c) {
                    eprintln!("ebobo: picture effect: {e}");
                }
            }) {
                eprintln!("ebobo: picture effect: {e}");
                ok = false;
            }
        };
        // LCD ghosting: the new frame over the previous ones.
        if p.ghost > 0.0 {
            texture.set_blend_mode(BlendMode::Blend);
            texture.set_alpha_mod(((1.0 - p.ghost) * 255.0) as u8);
            run(&mut fx.ghost, &mut |c| c.copy(texture, None, None));
            texture.set_alpha_mod(255);
            // Back to SDL's default for an alpha format, which the plain path relies on.
            texture.set_blend_mode(BlendMode::Blend);
        }
        // Glow source: the picture blurred by two linear downscales.
        run(&mut fx.mid, &mut |c| c.copy(texture, None, None));
        let mid = &fx.mid;
        run(&mut fx.small, &mut |c| c.copy(mid, None, None));
        let src: &mut Texture = match prescaled {
            _ if p.ghost > 0.0 => &mut fx.ghost,
            Some((target, _)) if pre => target,
            _ => texture,
        };
        let (mask, small, glow) = (&fx.mask, &mut fx.small, fx.glow);
        run(&mut fx.scene, &mut |c| {
            let full = Rect::new(0, 0, dw, dh);
            c.set_draw_color(Color::RGB(0, 0, 0));
            c.clear();
            src.set_blend_mode(BlendMode::None);
            c.copy(src, None, full)?;
            if p.bleed > 0.0 {
                // Composite colour bleed: a copy ~0.6 source pixel to the right.
                let shift = (dw as f32 / W as f32 * 0.6).round() as i32;
                src.set_blend_mode(BlendMode::Blend);
                src.set_alpha_mod((p.bleed * 255.0) as u8);
                c.copy(src, None, Rect::new(shift, 0, dw, dh))?;
                src.set_alpha_mod(255);
            }
            // Back to SDL's default for an alpha format, which the plain path relies on.
            src.set_blend_mode(BlendMode::Blend);
            c.copy(mask, None, full)?;
            let g = (glow * 255.0).round() as u8;
            small.set_blend_mode(BlendMode::Add);
            small.set_color_mod(g, g, g);
            c.copy(small, None, full)?;
            Ok(())
        });
        ok
    }

    fn make_fx(&mut self, key: (Effect, Rect)) -> Result<Fx, String> {
        let (effect, dest) = key;
        let p = effect.params();
        let (dw, dh) = (dest.width(), dest.height());
        let target = |w: u32, h: u32, linear: bool| {
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", if linear { "1" } else { "0" });
            let t = self._creator.create_texture_target(PixelFormatEnum::ABGR8888, w, h);
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
            t.map_err(|e| e.to_string())
        };
        let scene = target(dw, dh, true)?;
        let mid = target(W / 2, H / 2, true)?;
        let blur = p.glow_blur.max(1);
        let small = target(W / blur, H / blur, true)?;
        let mut ghost = target(W, H, false)?;
        let m = crt::mask(effect, dw, dh);
        let mut mask = self
            ._creator
            .create_texture_static(PixelFormatEnum::ABGR8888, dw, dh)
            .map_err(|e| e.to_string())?;
        mask.update(None, &m.rgba, (dw * 4) as usize).map_err(|e| e.to_string())?;
        mask.set_blend_mode(BlendMode::Mod);
        let texture = &self.texture;
        self.canvas
            .with_texture_canvas(&mut ghost, |c| {
                let _ = c.copy(texture, None, None);
            })
            .map_err(|e| e.to_string())?;
        let d = (dest.x() as f32, dest.y() as f32, dw as f32, dh as f32);
        let (verts, indices) = crt::mesh(d, p);
        let vertices = verts
            .iter()
            .map(|v| {
                let s = (v.shade.clamp(0.0, 1.0) * 255.0).round() as u8;
                Vertex {
                    position: FPoint::new(v.x, v.y),
                    color: Color::RGBA(s, s, s, 255),
                    tex_coord: FPoint::new(v.u, v.v),
                }
            })
            .collect();
        Ok(Fx { key, params: p, scene, mask, glow: m.glow, mid, small, ghost, vertices, indices })
    }

    /// Draws the picture into the `k`× target; false when render targets are unavailable.
    fn draw_prescaled(&mut self, k: u32) -> bool {
        if !self.targets {
            return false;
        }
        if self.prescaled.as_ref().map(|(_, have)| *have) != Some(k) {
            // The scale-quality hint is read when a texture is created: linear for the
            // target, nearest (as set in `new`) for everything else.
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "1");
            let made = self._creator.create_texture_target(PixelFormatEnum::ABGR8888, W * k, H * k);
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
            match made {
                Ok(target) => {
                    if let Some((old, _)) = self.prescaled.replace((target, k)) {
                        // SAFETY: the old target is not used again; the renderer is alive.
                        unsafe { old.destroy() };
                    }
                }
                Err(e) => {
                    eprintln!("ebobo: no render target ({e}), using nearest scaling");
                    self.targets = false;
                    return false;
                }
            }
        }
        let (target, _) = self.prescaled.as_mut().expect("created above");
        let texture = &self.texture;
        let drawn = self.canvas.with_texture_canvas(target, |c| {
            let _ = c.copy(texture, None, None);
        });
        if let Err(e) = drawn {
            eprintln!("ebobo: render target failed ({e}), using nearest scaling");
            self.targets = false;
            return false;
        }
        true
    }

    /// "Screen" setting; applies from the next frame.
    pub fn set_screen_mode(&mut self, screen: ScreenMode) {
        self.screen = screen;
    }

    pub fn set_fullscreen(&mut self, on: bool) {
        let kind = if on {
            FullscreenType::Desktop
        } else {
            FullscreenType::Off
        };
        if self.canvas.window_mut().set_fullscreen(kind).is_ok() {
            self.fullscreen = on;
        }
    }

    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Windowed-mode scale (1-6); resizes the window.
    pub fn set_scale(&mut self, scale: u32) {
        if self.fullscreen {
            return;
        }
        let scale = scale.clamp(1, 6);
        let window = self.canvas.window_mut();
        let _ = window.set_size(W * scale, H * scale);
    }

    pub fn set_title(&mut self, title: &str) {
        let _ = self.canvas.window_mut().set_title(title);
    }

    /// The window has keyboard focus.
    pub fn focused(&self) -> bool {
        self.canvas.window().window_flags() & sdl2::sys::SDL_WindowFlags::SDL_WINDOW_INPUT_FOCUS as u32 != 0
    }

    /// Shows or hides the mouse cursor.
    pub fn set_cursor_visible(&mut self, on: bool) {
        if on != self.cursor {
            self.cursor = on;
            self.mouse.show_cursor(on);
        }
    }

    /// Window position → 256x240 UI canvas pixel through the destination rect `present`
    /// draws into; `None` in the letterbox bars or when the window size is unknown.
    pub fn to_canvas(&self, pos: WindowPos) -> Option<(i32, i32)> {
        let (ow, oh) = self.canvas.output_size().ok()?;
        map_to_canvas(self.screen, self.effect.params().curve, self.canvas.window().size(), (ow, oh), pos)
    }
}

/// CRT turn-on over `dest`: only the centred `open` fraction of the height shows, with
/// `flash` white added on top (a bright line that opens vertically, then settles).
fn draw_power(c: &mut Canvas<Window>, dest: Rect, open: f32, flash: f32) -> Result<(), String> {
    let h = dest.height() as f32;
    let band = (h * open).round().max(1.0) as u32;
    let top = dest.y() + ((h as u32 - band.min(h as u32)) / 2) as i32;
    c.set_blend_mode(BlendMode::None);
    c.set_draw_color(Color::RGB(0, 0, 0));
    if top > dest.y() {
        c.fill_rect(Rect::new(dest.x(), dest.y(), dest.width(), (top - dest.y()) as u32))?;
    }
    let bottom = top + band as i32;
    if bottom < dest.bottom() {
        c.fill_rect(Rect::new(dest.x(), bottom, dest.width(), (dest.bottom() - bottom) as u32))?;
    }
    let f = (flash.clamp(0.0, 1.0) * 255.0) as u8;
    if f > 0 {
        c.set_blend_mode(BlendMode::Add);
        c.set_draw_color(Color::RGB(f, f, f));
        c.fill_rect(Rect::new(dest.x(), top, dest.width(), band))?;
        c.set_blend_mode(BlendMode::None);
    }
    Ok(())
}

/// Where the 256x240 picture goes in an `ow`×`oh` drawable.
fn dest_rect(screen: ScreenMode, ow: u32, oh: u32) -> Rect {
    let (ow, oh) = (ow.max(1), oh.max(1));
    let (w, h) = match screen {
        ScreenMode::Stretch => (ow, oh),
        ScreenMode::Pixel if ow >= W && oh >= H => {
            let k = (ow / W).min(oh / H);
            (W * k, H * k)
        }
        // Fit, and Pixel in a drawable smaller than one picture.
        _ => {
            // Wider than 256:240 → full height, else full width.
            if ow as u64 * H as u64 > oh as u64 * W as u64 {
                (((oh as u64 * W as u64 + H as u64 / 2) / H as u64) as u32, oh)
            } else {
                (ow, ((ow as u64 * H as u64 + W as u64 / 2) / W as u64) as u32)
            }
        }
    };
    let (w, h) = (w.max(1), h.max(1));
    Rect::new(((ow - w) / 2) as i32, ((oh - h) / 2) as i32, w, h)
}

/// `pos` in a window of `win` points whose drawable is `out` pixels (HiDPI: out = 2 × win on
/// Retina) → canvas pixel of the picture drawn by `dest_rect`, through the inverse of the
/// screen curvature `curve` (0 = flat; the glass corners outside the picture → `None`).
fn map_to_canvas(screen: ScreenMode, curve: f32, win: (u32, u32), out: (u32, u32), pos: WindowPos) -> Option<(i32, i32)> {
    let (ww, wh) = (win.0.max(1) as f32, win.1.max(1) as f32);
    let (ow, oh) = (out.0.max(1) as f32, out.1.max(1) as f32);
    let (px, py) = match pos {
        WindowPos::Point(x, y) => (x * ow / ww, y * oh / wh),
        WindowPos::Touch(x, y) => (x * ow, y * oh),
    };
    let d = dest_rect(screen, out.0, out.1);
    let fx = (px - d.x() as f32) / d.width() as f32;
    let fy = (py - d.y() as f32) / d.height() as f32;
    let (fx, fy) = crt::warp(fx, fy, curve);
    if !(0.0..1.0).contains(&fx) || !(0.0..1.0).contains(&fy) {
        return None;
    }
    Some(((fx * W as f32) as i32, (fy * H as f32) as i32))
}

/// Integer prescale for a `w`×`h` destination: 1 (draw directly with nearest) when the
/// destination is an exact integer multiple or not larger than the picture, else the
/// smallest factor covering it (capped), which is then scaled down linearly.
fn prescale(w: u32, h: u32) -> u32 {
    if w <= W && h <= H {
        return 1;
    }
    if w % W == 0 && h % H == 0 && w / W == h / H {
        return 1;
    }
    w.div_ceil(W).max(h.div_ceil(H)).clamp(1, MAX_PRESCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_screen_modes() {
        assert_eq!(dest_rect(ScreenMode::Stretch, 1280, 800), Rect::new(0, 0, 1280, 800));
        assert_eq!(dest_rect(ScreenMode::Fit, 1280, 800), Rect::new(213, 0, 853, 800));
        assert_eq!(dest_rect(ScreenMode::Pixel, 1280, 800), Rect::new(256, 40, 768, 720));
        assert_eq!(dest_rect(ScreenMode::Fit, 256, 400), Rect::new(0, 80, 256, 240));
        assert_eq!(dest_rect(ScreenMode::Pixel, 200, 100).height(), 100);
        assert_eq!(prescale(768, 720), 1);
        assert_eq!(prescale(1280, 800), 5);
        assert_eq!(prescale(853, 800), 4);
        assert_eq!(prescale(200, 100), 1);
        assert_eq!(prescale(7680, 4320), MAX_PRESCALE);
    }

    #[test]
    fn window_to_canvas_all_modes() {
        let deck = |mode, pos| map_to_canvas(mode, 0.0, (1280, 800), (1280, 800), pos);
        // Stretch: whole window, no bars.
        assert_eq!(deck(ScreenMode::Stretch, WindowPos::Point(0.0, 0.0)), Some((0, 0)));
        assert_eq!(deck(ScreenMode::Stretch, WindowPos::Point(1279.0, 799.0)), Some((255, 239)));
        assert_eq!(deck(ScreenMode::Stretch, WindowPos::Touch(0.5, 0.5)), Some((128, 120)));
        // Fit: picture at x 213..1066, bars left and right.
        assert_eq!(deck(ScreenMode::Fit, WindowPos::Point(100.0, 400.0)), None);
        assert_eq!(deck(ScreenMode::Fit, WindowPos::Point(1100.0, 400.0)), None);
        assert_eq!(deck(ScreenMode::Fit, WindowPos::Point(213.0, 0.0)), Some((0, 0)));
        assert_eq!(deck(ScreenMode::Fit, WindowPos::Touch(0.5, 0.5)), Some((128, 120)));
        // Pixel: 3x at (256, 40), bars on all sides.
        assert_eq!(deck(ScreenMode::Pixel, WindowPos::Point(640.0, 20.0)), None);
        assert_eq!(deck(ScreenMode::Pixel, WindowPos::Point(640.0, 790.0)), None);
        assert_eq!(deck(ScreenMode::Pixel, WindowPos::Point(200.0, 400.0)), None);
        assert_eq!(deck(ScreenMode::Pixel, WindowPos::Point(256.0, 40.0)), Some((0, 0)));
        assert_eq!(deck(ScreenMode::Pixel, WindowPos::Point(259.0, 43.0)), Some((1, 1)));
        assert_eq!(deck(ScreenMode::Pixel, WindowPos::Point(1023.0, 759.0)), Some((255, 239)));
        // HiDPI: 640x400 points, 1280x800 pixels → the same picture as the Deck.
        let retina = map_to_canvas(ScreenMode::Pixel, 0.0, (640, 400), (1280, 800), WindowPos::Point(129.5, 21.5));
        assert_eq!(retina, Some((1, 1)));
        assert_eq!(map_to_canvas(ScreenMode::Fit, 0.0, (640, 400), (1280, 800), WindowPos::Point(50.0, 200.0)), None);
        // Flat effects map exactly like Off.
        for effect in Effect::ALL {
            if effect.params().curve == 0.0 {
                for pos in [WindowPos::Point(259.0, 43.0), WindowPos::Touch(0.31, 0.77)] {
                    assert_eq!(
                        map_to_canvas(ScreenMode::Fit, effect.params().curve, (1280, 800), (1280, 800), pos),
                        deck(ScreenMode::Fit, pos)
                    );
                }
            }
        }
    }

    #[test]
    fn curved_taps_land_on_drawn_pixel() {
        let k = Effect::CrtTv.params().curve;
        let d = dest_rect(ScreenMode::Fit, 1280, 800);
        for (cx, cy) in [(128, 120), (10, 12), (240, 200), (5, 230), (128, 3)] {
            // Where the curved mesh draws the centre of canvas pixel (cx, cy).
            let (sx, sy) = crt::unwarp((cx as f32 + 0.5) / W as f32, (cy as f32 + 0.5) / H as f32, k);
            let pos = WindowPos::Point(d.x() as f32 + sx * d.width() as f32, d.y() as f32 + sy * d.height() as f32);
            let (x, y) = map_to_canvas(ScreenMode::Fit, k, (1280, 800), (1280, 800), pos).expect("on the picture");
            assert!((x - cx).abs() <= 1 && (y - cy).abs() <= 1, "({cx},{cy}) → ({x},{y})");
        }
        // The black glass corner is not the picture.
        assert_eq!(map_to_canvas(ScreenMode::Fit, k, (1280, 800), (1280, 800), WindowPos::Point(214.0, 1.0)), None);
    }
}
