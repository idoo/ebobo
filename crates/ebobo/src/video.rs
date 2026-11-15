//! Window and frame presentation. Owner: platform slice.
//!
//! A frame is three layers, bottom to top: the game at its own size, the pictures the
//! interface placed (box art), the 256x240 interface canvas with alpha. They are composed
//! into one source picture the size of the destination rectangle; the picture effect, if
//! any, then runs on that. Around a game's picture the bars carry its ambient light.

use std::time::Instant;

use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::{FPoint, Rect};
use sdl2::render::{BlendMode, Texture, TextureCreator, Vertex, VertexIndices, WindowCanvas};
use sdl2::video::{FullscreenType, WindowContext};

use crate::canvas::{Canvas, Placed};
use crate::config::ScreenMode;
use crate::crt::{self, Effect};
use crate::input::WindowPos;

/// The interface canvas.
const W: u32 = 256;
const H: u32 = 240;
/// Largest integer prescale for non-integer modes (8x = 2048x1920, enough for 4K).
const MAX_PRESCALE: u32 = 8;
/// Picture textures kept; the least recently drawn goes first.
const MAX_PICTURES: usize = 64;
/// Largest side of a game frame or a picture, pixels.
const MAX_SIDE: u32 = 4096;
/// Brightness of the ambient light at the picture's edge (1 = the blurred picture itself).
const AMBIENT: f32 = 0.55;
/// How much of the ambient light is left at the output's outer edge (0.55 × 0.27 ≈ 15 % of
/// the blurred picture).
const AMBIENT_EDGE: f32 = 0.27;
/// Rings of quads the ambient light's falloff is drawn with.
const SHADE_RINGS: u32 = 8;

/// One frame to show: the layers, bottom to top.
pub struct Frame<'a> {
    /// The console's picture; `None` in the menus.
    pub game: Option<GameFrame<'a>>,
    /// The interface: 256x240, straight alpha, with the pictures it placed.
    pub ui: &'a Canvas,
    /// Where the placed pictures' pixels come from.
    pub pictures: &'a dyn Pictures,
}

/// The console's frame: `w * h * 4` RGBA bytes. `scale` is how many texture pixels one of the
/// console's own pixels takes on each axis: 1 = the console's own pixels, more for an enhanced
/// picture, which is then drawn with linear filtering.
pub struct GameFrame<'a> {
    pub rgba: &'a [u8],
    pub w: u32,
    pub h: u32,
    pub scale: u32,
}

impl GameFrame<'_> {
    /// The console's own picture size, `w / scale` × `h / scale`: what the picture rectangle,
    /// the pixel-perfect factors and the effect's rows and columns follow.
    fn own_size(&self) -> (u32, u32) {
        let k = self.scale.max(1);
        ((self.w / k).max(1), (self.h / k).max(1))
    }
}

/// A picture's pixels (`w * h * 4` RGBA bytes); `stamp` changes when the pixels do.
pub struct PictureRef<'a> {
    pub w: u32,
    pub h: u32,
    pub rgba: &'a [u8],
    pub stamp: u32,
}

/// The pictures the interface can place, by `Placed::id`.
pub trait Pictures {
    /// The picture, or `None` while it is not there (then only the box's back is drawn).
    fn picture(&self, id: u64) -> Option<PictureRef<'_>>;
}

/// A frame's layers as `compose` takes them.
struct Layers<'a> {
    game: Option<&'a GameFrame<'a>>,
    ui: &'a [u8],
    placed: &'a [Placed],
    pictures: &'a dyn Pictures,
}

impl<'a> Layers<'a> {
    fn of(frame: &'a Frame<'a>) -> Layers<'a> {
        Layers { game: frame.game.as_ref(), ui: &frame.ui.px, placed: &frame.ui.placed, pictures: frame.pictures }
    }

    /// Size of the source picture: the game's own, else the interface's.
    fn size(&self) -> (u32, u32) {
        self.game.map_or((W, H), |g| g.own_size())
    }
}

/// A cached picture texture.
struct Pic {
    id: u64,
    stamp: u32,
    w: u32,
    h: u32,
    tex: Texture,
    /// `Video::tick` of the last frame it was available for; drawn only in that frame.
    used: u64,
}

/// SDL window + renderer + the layer textures.
pub struct Video {
    // `unsafe_textures`: the textures are freed with the renderer; keep the creator alive.
    /// The interface: streaming 256x240, alpha blended.
    ui: Texture,
    /// The game: streaming, recreated when its size (w, h) or its filtering changes (true =
    /// linear: an enhanced frame, `scale` above 1).
    game: Option<(Texture, u32, u32, bool)>,
    /// Integer prescale targets for non-integer scales, keyed by (w, h, factor): the layer is
    /// drawn into it with nearest-neighbour, then scaled to the picture with linear filtering
    /// (sharp, no shimmering). `None` until needed.
    ui_pre: Option<(Texture, (u32, u32, u32))>,
    game_pre: Option<(Texture, (u32, u32, u32))>,
    /// The composed source picture (w, h = the destination rectangle's size).
    source: Option<(Texture, u32, u32)>,
    /// Picture textures by id, at most `MAX_PICTURES`.
    pics: Vec<Pic>,
    /// Frames composed.
    tick: u64,
    /// Source size of the last presented frame (`to_canvas` maps through its rectangle).
    shown: (u32, u32),
    /// Render targets work on this renderer (else the layers go straight to the output
    /// with nearest scaling).
    targets: bool,
    _creator: TextureCreator<WindowContext>,
    canvas: WindowCanvas,
    mouse: sdl2::mouse::MouseUtil,
    /// Mouse cursor currently shown.
    cursor: bool,
    fullscreen: bool,
    screen: ScreenMode,
    /// Picture effect; `Off` draws exactly the plain path.
    effect: Effect,
    /// Effect textures for the current effect, destination rect and source size.
    fx: Option<Fx>,
    /// Power-on animation start (tube effects only).
    power: Option<Instant>,
    /// The blurred copy of the source picture: the effects' glow and the ambient light.
    blur: Option<Blur>,
    /// Ambient light in the bars around a game's picture.
    ambient: bool,
    /// The ambient light's falloff toward the output's edges.
    shade: Shade,
}

/// GPU resources of one effect at one destination rect and source size; rebuilt when any changes.
struct Fx {
    key: (Effect, Rect, (u32, u32)),
    params: crt::Params,
    /// Destination-sized target the picture, bleed, mask and glow are composed in.
    scene: Texture,
    /// Multiplied over the scene (`BlendMode::Mod`).
    mask: Texture,
    /// Additive glow weight compensating the mask's darkening.
    glow: f32,
    /// Previous frames blended in (LCD ghosting), destination-sized.
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
            self.ghost.destroy();
        }
    }
}

/// The blurred copy of the composed source picture, for a source size (w, h) and a blur
/// width; rebuilt when any changes.
struct Blur {
    key: (u32, u32, u32),
    /// The glow copy: the source picture back at the source's resolution, then two linear
    /// downscales.
    base: Texture,
    mid: Texture,
    small: Texture,
    /// The ambient light: two more halvings of `small`; `haze` carries the `AMBIENT`
    /// colour modulation.
    dim: Texture,
    haze: Texture,
}

impl Blur {
    fn new(creator: &TextureCreator<WindowContext>, key: (u32, u32, u32)) -> Result<Blur, String> {
        let (sw, sh, blur) = key;
        let base = make_target(creator, sw, sh, true)?;
        let mid = make_target(creator, (sw / 2).max(1), (sh / 2).max(1), true)?;
        let (w, h) = ((sw / blur.max(1)).max(1), (sh / blur.max(1)).max(1));
        let small = make_target(creator, w, h, true)?;
        let dim = make_target(creator, (w / 2).max(1), (h / 2).max(1), true)?;
        let mut haze = make_target(creator, (w / 4).max(1), (h / 4).max(1), true)?;
        let a = (AMBIENT * 255.0).round() as u8;
        haze.set_blend_mode(BlendMode::None);
        haze.set_color_mod(a, a, a);
        Ok(Blur { key, base, mid, small, dim, haze })
    }

    /// SAFETY: the textures are not used again; the renderer is alive.
    unsafe fn destroy(self) {
        unsafe {
            self.base.destroy();
            self.mid.destroy();
            self.small.destroy();
            self.dim.destroy();
            self.haze.destroy();
        }
    }
}

/// The ambient light's falloff: black quads, clear at the picture rectangle's edge and
/// growing opaque toward the output's edge, for (picture rectangle, output size).
#[derive(Default)]
struct Shade {
    key: Option<(Rect, (u32, u32))>,
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
}

impl Shade {
    /// Rebuilds the quads when the picture rectangle or the output size changed.
    fn fit(&mut self, dest: Rect, out: (u32, u32)) {
        if self.key == Some((dest, out)) {
            return;
        }
        let (verts, indices) = shade_rings(dest, out);
        self.key = Some((dest, out));
        self.vertices = verts
            .iter()
            .map(|&(x, y, a)| Vertex {
                position: FPoint::new(x, y),
                color: Color::RGBA(0, 0, 0, a),
                tex_coord: FPoint::new(0.0, 0.0),
            })
            .collect();
        self.indices = indices;
    }
}

impl Video {
    /// Creates the window (`scale` × 256x240 in windowed mode, desktop fullscreen when
    /// `fullscreen`); `screen` picks how the picture fills it.
    pub fn new(sdl: &sdl2::Sdl, fullscreen: bool, scale: u32, screen: ScreenMode) -> Result<Video, String> {
        let video = sdl.video()?;
        sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
        let scale = scale.clamp(1, 6);
        let make = |accelerated: bool| -> Result<WindowCanvas, String> {
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
        // a small picture is cheap to scale on the CPU.
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
        let ui = creator
            .create_texture_streaming(PixelFormatEnum::ABGR8888, W, H)
            .map_err(|e| e.to_string())?;
        let mouse = sdl.mouse();
        // Hidden until the mouse moves (`set_cursor_visible`).
        mouse.show_cursor(false);
        Ok(Video {
            ui,
            game: None,
            ui_pre: None,
            game_pre: None,
            source: None,
            pics: Vec::with_capacity(MAX_PICTURES),
            tick: 0,
            shown: (W, H),
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
            blur: None,
            ambient: true,
            shade: Shade::default(),
        })
    }

    /// Ambient light: the bars around a game's picture glow softly in its colours. Applies
    /// from the next frame; nothing in the menus or when the picture fills the output.
    pub fn set_ambient(&mut self, on: bool) {
        self.ambient = on;
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

    /// Shows one frame: the game (if any), the placed pictures and the interface, composed
    /// in the picture rectangle of the game's size (256x240 without a game).
    pub fn present_frame(&mut self, frame: &Frame) -> Result<(), String> {
        self.show(&Layers::of(frame))
    }

    /// The frame as the screen shows it (effects included) composed at `size` (default: the
    /// drawable size): (width, height, RGBA bytes). Screenshots; needs render targets.
    pub fn capture_frame(&mut self, frame: &Frame, size: Option<(u32, u32)>) -> Result<(u32, u32, Vec<u8>), String> {
        self.shoot(&Layers::of(frame), size)
    }

    fn show(&mut self, layers: &Layers) -> Result<(), String> {
        // Computed every frame from the drawable size, so resizes and fullscreen toggles apply.
        let out = self.canvas.output_size()?;
        self.compose(layers, out, None)?;
        self.shown = layers.size();
        self.canvas.present();
        Ok(())
    }

    fn shoot(&mut self, layers: &Layers, size: Option<(u32, u32)>) -> Result<(u32, u32, Vec<u8>), String> {
        let (w, h) = match size {
            Some(size) => size,
            None => self.canvas.output_size()?,
        };
        let mut shot = self
            ._creator
            .create_texture_target(PixelFormatEnum::ABGR8888, w, h)
            .map_err(|e| e.to_string())?;
        let px = self.compose(layers, (w, h), Some(&mut shot));
        // SAFETY: not used again; the renderer is alive.
        unsafe { shot.destroy() };
        Ok((w, h, px?.ok_or_else(|| "no pixels".to_string())?))
    }

    /// Draws the layers into an `out`-sized output: the window, or `shot` (then read back).
    fn compose(&mut self, layers: &Layers, out: (u32, u32), shot: Option<&mut Texture>) -> Result<Option<Vec<u8>>, String> {
        if layers.ui.len() < (W * H * 4) as usize {
            return Err(format!("frame too small: {} bytes", layers.ui.len()));
        }
        if let Some(g) = layers.game {
            if g.w == 0 || g.h == 0 || g.w > MAX_SIDE || g.h > MAX_SIDE {
                return Err(format!("game frame {}x{}", g.w, g.h));
            }
            if g.rgba.len() < (g.w * g.h * 4) as usize {
                return Err(format!("game frame too small: {} bytes for {}x{}", g.rgba.len(), g.w, g.h));
            }
        }
        let size = layers.size();
        let dest = dest_rect(self.screen, out.0, out.1, size.0, size.1);
        self.ui
            .update(None, &layers.ui[..(W * H * 4) as usize], (W * 4) as usize)
            .map_err(|e| e.to_string())?;
        if let Some(g) = layers.game {
            self.upload_game(g)?;
        }
        self.tick += 1;
        self.sync_pictures(layers.placed, layers.pictures);
        let composed = self.targets && self.draw_source(dest, layers.game, size, layers.placed);
        let power = self.power.and_then(|t| crt::power_on(t.elapsed().as_secs_f32() * 1000.0));
        if power.is_none() {
            self.power = None;
        }
        let fx_on = composed && self.effect != Effect::Off;
        // A tube that is still switching on lights nothing around it yet.
        let ambient = composed && power.is_none() && ambient_wanted(self.ambient, layers.game.is_some(), dest, out);
        let blurred = (fx_on || ambient) && self.draw_blur(size, ambient);
        let fx = fx_on && blurred && self.draw_fx(dest, size);
        let ambient = ambient && blurred;
        if ambient {
            self.shade.fit(dest, out);
        }
        let Video { canvas, ui, game, source, pics, fx: effect, blur, shade, tick, .. } = self;
        let fx = if fx { effect.as_ref() } else { None };
        let light = if ambient { blur.as_ref().map(|b| (&b.haze, &*shade)) } else { None };
        let mut source = if composed { source.as_mut().map(|(t, ..)| t) } else { None };
        let mut game = if layers.game.is_some() { game.as_mut().map(|(t, ..)| t) } else { None };
        let mut draw = |c: &mut WindowCanvas| -> Result<(), String> {
            c.set_draw_color(Color::RGB(0, 0, 0));
            c.clear();
            if let Some((haze, shade)) = light {
                draw_ambient(c, haze, shade)?;
            }
            match (fx, source.as_deref_mut()) {
                (Some(fx), _) if fx.params.curve > 0.0 || fx.params.vignette > 0.0 => {
                    c.render_geometry(&fx.vertices, Some(&fx.scene), VertexIndices::U32(&fx.indices))?
                }
                (Some(fx), _) => c.copy(&fx.scene, None, dest)?,
                (None, Some(source)) => {
                    source.set_blend_mode(BlendMode::None);
                    c.copy(source, None, dest)?
                }
                // No render targets: the layers straight to the output, nearest.
                (None, None) => draw_layers(c, dest, game.as_deref_mut(), ui, layers.placed, pics, *tick)?,
            }
            if let Some((open, flash)) = power {
                draw_power(c, dest, open, flash)?;
            }
            Ok(())
        };
        match shot {
            None => draw(canvas).map(|_| None),
            Some(target) => {
                let mut px = Ok(None);
                canvas
                    .with_texture_canvas(target, |c| {
                        px = draw(c).and_then(|_| c.read_pixels(None, PixelFormatEnum::ABGR8888).map(Some));
                    })
                    .map_err(|e| e.to_string())?;
                px
            }
        }
    }

    /// Uploads the game's pixels, recreating its texture when the size changed or the
    /// filtering has to: an enhanced frame (`scale` above 1) is scaled with linear filtering,
    /// the console's own pixels with nearest, and the scale-quality hint is read when a
    /// texture is created.
    fn upload_game(&mut self, g: &GameFrame) -> Result<(), String> {
        let linear = g.scale > 1;
        if self.game.as_ref().map(|(_, w, h, l)| (*w, *h, *l)) != Some((g.w, g.h, linear)) {
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", if linear { "1" } else { "0" });
            let made = self._creator.create_texture_streaming(PixelFormatEnum::ABGR8888, g.w, g.h);
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
            let made = made.map_err(|e| e.to_string())?;
            if let Some((old, ..)) = self.game.replace((made, g.w, g.h, linear)) {
                // SAFETY: the old texture is not used again; the renderer is alive.
                unsafe { old.destroy() };
            }
        }
        let (texture, ..) = self.game.as_mut().expect("created above");
        texture
            .update(None, &g.rgba[..(g.w * g.h * 4) as usize], (g.w * 4) as usize)
            .map_err(|e| e.to_string())
    }

    /// Brings the textures of the placed pictures up to date: uploads a missing one or one
    /// whose stamp changed, and marks every available one as drawn this frame. A full cache
    /// drops its least recently drawn picture; a picture that is not available is left alone.
    fn sync_pictures(&mut self, placed: &[Placed], pictures: &dyn Pictures) {
        for p in placed {
            let Some(r) = pictures.picture(p.id) else { continue };
            if r.w == 0 || r.h == 0 || r.w > MAX_SIDE || r.h > MAX_SIDE || r.rgba.len() < (r.w * r.h * 4) as usize {
                continue;
            }
            let pixels = &r.rgba[..(r.w * r.h * 4) as usize];
            if let Some(i) = self.pics.iter().position(|pic| pic.id == p.id) {
                let pic = &mut self.pics[i];
                if (pic.w, pic.h) == (r.w, r.h) {
                    if pic.stamp == r.stamp || pic.tex.update(None, pixels, (r.w * 4) as usize).is_ok() {
                        pic.stamp = r.stamp;
                        pic.used = self.tick;
                    }
                    continue;
                }
                // SAFETY: removed from the cache, not used again; the renderer is alive.
                unsafe { self.pics.swap_remove(i).tex.destroy() };
            } else if self.pics.len() >= MAX_PICTURES {
                let Some(oldest) = oldest(self.pics.iter().map(|pic| pic.used), self.tick) else { continue };
                // SAFETY: removed from the cache, not used again; the renderer is alive.
                unsafe { self.pics.swap_remove(oldest).tex.destroy() };
            }
            // Pictures are scaled with linear filtering (nearest without render targets).
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", if self.targets { "1" } else { "0" });
            let made = self._creator.create_texture_static(PixelFormatEnum::ABGR8888, r.w, r.h);
            sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
            let Ok(mut tex) = made else { continue };
            if tex.update(None, pixels, (r.w * 4) as usize).is_err() {
                // SAFETY: not used again; the renderer is alive.
                unsafe { tex.destroy() };
                continue;
            }
            tex.set_blend_mode(BlendMode::Blend);
            self.pics.push(Pic { id: p.id, stamp: r.stamp, w: r.w, h: r.h, tex, used: self.tick });
        }
    }

    /// Composes the source picture for `dest` (game, pictures, interface); false when render
    /// targets fail (then the layers are drawn straight to the output).
    fn draw_source(&mut self, dest: Rect, game: Option<&GameFrame>, size: (u32, u32), placed: &[Placed]) -> bool {
        match self.try_source(dest, game, size, placed) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("ebobo: no render target ({e}), using nearest scaling");
                self.targets = false;
                false
            }
        }
    }

    fn try_source(&mut self, dest: Rect, game: Option<&GameFrame>, size: (u32, u32), placed: &[Placed]) -> Result<(), String> {
        let (dw, dh) = (dest.width(), dest.height());
        let Video { canvas, _creator: creator, ui, game: game_tex, ui_pre, game_pre, source, pics, tick, .. } = self;
        let mut game_src: Option<&mut Texture> = None;
        if let Some(g) = game {
            let (texture, ..) = game_tex.as_mut().ok_or("no game texture")?;
            // An enhanced frame goes straight in, with linear filtering.
            let k = if g.scale > 1 { 1 } else { prescale(dw, dh, size.0, size.1) };
            game_src = Some(if k > 1 { prescale_into(canvas, creator, game_pre, texture, size, k)? } else { texture });
        }
        let k = prescale(dw, dh, W, H);
        let ui_src = if k > 1 { prescale_into(canvas, creator, ui_pre, ui, (W, H), k)? } else { ui };
        if source.as_ref().map(|(_, w, h)| (*w, *h)) != Some((dw, dh)) {
            // Linear: the glow copy is a downscale of it; everything else copies it 1:1.
            let made = make_target(creator, dw, dh, true)?;
            if let Some((old, ..)) = source.replace((made, dw, dh)) {
                // SAFETY: the old target is not used again; the renderer is alive.
                unsafe { old.destroy() };
            }
        }
        let (target, ..) = source.as_mut().expect("created above");
        let mut drawn = Ok(());
        canvas
            .with_texture_canvas(target, |c| {
                c.set_draw_color(Color::RGB(0, 0, 0));
                c.clear();
                drawn = draw_layers(c, Rect::new(0, 0, dw, dh), game_src, ui_src, placed, pics, *tick);
            })
            .map_err(|e| e.to_string())?;
        drawn
    }

    /// Blurs the composed source picture into `blur` (the glow copy), and on into its haze
    /// when `ambient`; false when render targets fail.
    fn draw_blur(&mut self, size: (u32, u32), ambient: bool) -> bool {
        let key = (size.0, size.1, self.effect.params().glow_blur.max(1));
        if self.blur.as_ref().map(|b| b.key) != Some(key) {
            if let Some(old) = self.blur.take() {
                // SAFETY: replaced below; the renderer is alive.
                unsafe { old.destroy() };
            }
            match Blur::new(&self._creator, key) {
                Ok(blur) => self.blur = Some(blur),
                Err(e) => {
                    eprintln!("ebobo: no blurred picture ({e})");
                    return false;
                }
            }
        }
        let Video { canvas, source, blur, .. } = self;
        let (Some((source, ..)), Some(b)) = (source.as_mut(), blur.as_mut()) else { return false };
        // The picture at the source's resolution, blurred by two linear downscales.
        source.set_blend_mode(BlendMode::None);
        let source = &*source;
        let mut ok = pass(canvas, &mut b.base, &mut |c| c.copy(source, None, None));
        let base = &b.base;
        ok &= pass(canvas, &mut b.mid, &mut |c| c.copy(base, None, None));
        let mid = &b.mid;
        ok &= pass(canvas, &mut b.small, &mut |c| c.copy(mid, None, None));
        if ambient {
            // The effect leaves its glow weight on `small`; the haze takes it plain.
            b.small.set_blend_mode(BlendMode::None);
            b.small.set_color_mod(255, 255, 255);
            let small = &b.small;
            ok &= pass(canvas, &mut b.dim, &mut |c| c.copy(small, None, None));
            let dim = &b.dim;
            ok &= pass(canvas, &mut b.haze, &mut |c| c.copy(dim, None, None));
        }
        ok
    }

    /// Composes the effect scene for `dest` from the source picture (bleed, mask, glow);
    /// false when render targets fail (then the plain source picture is drawn).
    fn draw_fx(&mut self, dest: Rect, size: (u32, u32)) -> bool {
        if !self.targets {
            return false;
        }
        let key = (self.effect, dest, size);
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
        let Video { canvas, source, fx, blur, .. } = self;
        let (Some((source, ..)), Some(blur)) = (source.as_mut(), blur.as_mut()) else { return false };
        let fx = fx.as_mut().expect("made above");
        let p = fx.params;
        let (dw, dh) = (dest.width(), dest.height());
        let mut ok = true;
        // LCD ghosting: the new frame over the previous ones.
        if p.ghost > 0.0 {
            source.set_blend_mode(BlendMode::Blend);
            source.set_alpha_mod(((1.0 - p.ghost) * 255.0) as u8);
            let frame = &*source;
            ok &= pass(canvas, &mut fx.ghost, &mut |c| c.copy(frame, None, None));
            source.set_alpha_mod(255);
        }
        let src: &mut Texture = if p.ghost > 0.0 { &mut fx.ghost } else { source };
        let (mask, small, glow) = (&fx.mask, &mut blur.small, fx.glow);
        ok &= pass(canvas, &mut fx.scene, &mut |c| {
            let full = Rect::new(0, 0, dw, dh);
            c.set_draw_color(Color::RGB(0, 0, 0));
            c.clear();
            src.set_blend_mode(BlendMode::None);
            c.copy(src, None, full)?;
            if p.bleed > 0.0 {
                // Composite colour bleed: a copy ~0.6 of one of the console's pixels to the right.
                let shift = (dw as f32 / size.0 as f32 * 0.6).round() as i32;
                src.set_blend_mode(BlendMode::Blend);
                src.set_alpha_mod((p.bleed * 255.0) as u8);
                c.copy(src, None, Rect::new(shift, 0, dw, dh))?;
                src.set_alpha_mod(255);
            }
            src.set_blend_mode(BlendMode::None);
            c.copy(mask, None, full)?;
            let g = (glow * 255.0).round() as u8;
            small.set_blend_mode(BlendMode::Add);
            small.set_color_mod(g, g, g);
            c.copy(small, None, full)?;
            Ok(())
        });
        ok
    }

    /// Effect resources for (effect, destination rect, source size). The ghost starts as
    /// the source picture just composed.
    fn make_fx(&mut self, key: (Effect, Rect, (u32, u32))) -> Result<Fx, String> {
        let (effect, dest, (sw, sh)) = key;
        let p = effect.params();
        let (dw, dh) = (dest.width(), dest.height());
        let creator = &self._creator;
        let scene = make_target(creator, dw, dh, true)?;
        let mut ghost = make_target(creator, dw, dh, false)?;
        let m = crt::mask(effect, dw, dh, sw, sh);
        let mut mask = creator
            .create_texture_static(PixelFormatEnum::ABGR8888, dw, dh)
            .map_err(|e| e.to_string())?;
        mask.update(None, &m.rgba, (dw * 4) as usize).map_err(|e| e.to_string())?;
        mask.set_blend_mode(BlendMode::Mod);
        if let Some((source, ..)) = self.source.as_mut() {
            source.set_blend_mode(BlendMode::None);
            let source = &*source;
            self.canvas
                .with_texture_canvas(&mut ghost, |c| {
                    let _ = c.copy(source, None, None);
                })
                .map_err(|e| e.to_string())?;
        }
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
        Ok(Fx { key, params: p, scene, mask, glow: m.glow, ghost, vertices, indices })
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

    /// Window position → 256x240 UI canvas pixel through the picture rectangle of the last
    /// presented frame; `None` in the letterbox bars or when the window size is unknown.
    pub fn to_canvas(&self, pos: WindowPos) -> Option<(i32, i32)> {
        let (ow, oh) = self.canvas.output_size().ok()?;
        map_to_canvas(self.screen, self.effect.params().curve, self.canvas.window().size(), (ow, oh), self.shown, pos)
    }
}

/// Runs `f` drawing into `target`; false when the target cannot be drawn into.
fn pass(canvas: &mut WindowCanvas, target: &mut Texture, f: &mut dyn FnMut(&mut WindowCanvas) -> Result<(), String>) -> bool {
    let done = canvas.with_texture_canvas(target, |c| {
        if let Err(e) = f(c) {
            eprintln!("ebobo: picture effect: {e}");
        }
    });
    if let Err(e) = &done {
        eprintln!("ebobo: picture effect: {e}");
    }
    done.is_ok()
}

/// The ambient light shows: switched on, a game on screen, and its picture rectangle `dest`
/// leaves bars in an `out`-sized output.
fn ambient_wanted(on: bool, game: bool, dest: Rect, out: (u32, u32)) -> bool {
    on && game && (dest.width() < out.0 || dest.height() < out.1)
}

/// The ambient light over the whole (cleared) output: `haze`, dimmed by its colour
/// modulation, then `shade` darkening it toward the edges. The picture is drawn on top.
fn draw_ambient(c: &mut WindowCanvas, haze: &Texture, shade: &Shade) -> Result<(), String> {
    c.copy(haze, None, None)?;
    c.set_blend_mode(BlendMode::Blend);
    let drawn = c.render_geometry(&shade.vertices, None::<&Texture>, VertexIndices::U32(&shade.indices));
    c.set_blend_mode(BlendMode::None);
    Ok(drawn?)
}

/// The ambient light's falloff around the picture rectangle `dest` in an `out`-sized output:
/// `SHADE_RINGS` + 1 rectangles from the picture's edge to the output's, four corners each
/// (x, y, black's alpha), and the triangles between neighbours. The light falls evenly from
/// the picture's edge to the output's, down to `AMBIENT_EDGE` of itself.
fn shade_rings(dest: Rect, out: (u32, u32)) -> (Vec<(f32, f32, u8)>, Vec<u32>) {
    let n = SHADE_RINGS;
    let (l, t, r, b) = (dest.x() as f32, dest.y() as f32, dest.right() as f32, dest.bottom() as f32);
    let (ow, oh) = (out.0 as f32, out.1 as f32);
    let mut verts = Vec::with_capacity(((n + 1) * 4) as usize);
    for i in 0..=n {
        let f = i as f32 / n as f32;
        let a = ((1.0 - AMBIENT_EDGE) * f * 255.0).round() as u8;
        let (x0, y0, x1, y1) = (l * (1.0 - f), t * (1.0 - f), r + (ow - r) * f, b + (oh - b) * f);
        verts.extend([(x0, y0, a), (x1, y0, a), (x1, y1, a), (x0, y1, a)]);
    }
    let mut idx = Vec::with_capacity((n * 24) as usize);
    for i in 0..n {
        let (inner, outer) = (i * 4, i * 4 + 4);
        for side in 0..4 {
            let (p, q) = (side, (side + 1) % 4);
            idx.extend([inner + p, inner + q, outer + q, inner + p, outer + q, outer + p]);
        }
    }
    (verts, idx)
}

/// A render target; `linear` picks the filtering when it is drawn scaled.
fn make_target(creator: &TextureCreator<WindowContext>, w: u32, h: u32, linear: bool) -> Result<Texture, String> {
    // The scale-quality hint is read when a texture is created: nearest (as set in `new`)
    // for everything that does not ask for linear.
    sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", if linear { "1" } else { "0" });
    let made = creator.create_texture_target(PixelFormatEnum::ABGR8888, w, h);
    sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "0");
    made.map_err(|e| e.to_string())
}

/// Draws `texture` (`size`) with nearest-neighbour into the `k`× linear target kept in
/// `slot` (made anew when the size or the factor changed), alpha included; returns the target.
fn prescale_into<'a>(
    canvas: &mut WindowCanvas,
    creator: &TextureCreator<WindowContext>,
    slot: &'a mut Option<(Texture, (u32, u32, u32))>,
    texture: &mut Texture,
    size: (u32, u32),
    k: u32,
) -> Result<&'a mut Texture, String> {
    let key = (size.0, size.1, k);
    if slot.as_ref().map(|(_, have)| *have) != Some(key) {
        let made = make_target(creator, size.0 * k, size.1 * k, true)?;
        if let Some((old, _)) = slot.replace((made, key)) {
            // SAFETY: the old target is not used again; the renderer is alive.
            unsafe { old.destroy() };
        }
    }
    let (target, _) = slot.as_mut().expect("created above");
    texture.set_blend_mode(BlendMode::None);
    let texture = &*texture;
    let mut drawn = Ok(());
    canvas
        .with_texture_canvas(target, |c| drawn = c.copy(texture, None, None))
        .map_err(|e| e.to_string())?;
    drawn?;
    Ok(target)
}

/// Draws the layers over `at` (already cleared): the game filling it, each placed picture
/// (its box filled with `back`, the picture fitted inside, both clipped to `clip`, faded by
/// `alpha`), then the interface blended over all of it. Only pictures marked `tick` are drawn.
fn draw_layers(
    c: &mut WindowCanvas,
    at: Rect,
    game: Option<&mut Texture>,
    ui: &mut Texture,
    placed: &[Placed],
    pics: &mut [Pic],
    tick: u64,
) -> Result<(), String> {
    if let Some(game) = game {
        game.set_blend_mode(BlendMode::None);
        c.copy(game, None, at)?;
    }
    if !placed.is_empty() {
        let drawn = draw_placed(c, at, placed, pics, tick);
        c.set_clip_rect(None::<Rect>);
        c.set_blend_mode(BlendMode::None);
        drawn?;
    }
    ui.set_blend_mode(BlendMode::Blend);
    c.copy(ui, None, at)
}

/// The pictures of `draw_layers`; leaves the clip rectangle set.
fn draw_placed(c: &mut WindowCanvas, at: Rect, placed: &[Placed], pics: &mut [Pic], tick: u64) -> Result<(), String> {
    for p in placed {
        let (Some(clip), Some(boxed)) = (scaled(p.clip, at), scaled((p.x, p.y, p.w, p.h), at)) else { continue };
        let Some(clip) = clip.intersection(at) else { continue };
        c.set_clip_rect(Some(clip));
        c.set_blend_mode(BlendMode::Blend);
        c.set_draw_color(Color::RGBA(p.back[0], p.back[1], p.back[2], p.back[3]));
        c.fill_rect(boxed)?;
        if let Some(pic) = pics.iter_mut().find(|pic| pic.id == p.id && pic.used == tick) {
            pic.tex.set_alpha_mod(p.alpha);
            c.copy(&pic.tex, None, fitted(pic.w, pic.h, boxed))?;
        }
    }
    Ok(())
}

/// Index of the least recently drawn picture given each one's `used`; `None` when every
/// one was drawn in frame `tick` (nothing may be dropped).
fn oldest(used: impl Iterator<Item = u64>, tick: u64) -> Option<usize> {
    let (i, used) = used.enumerate().min_by_key(|&(_, used)| used)?;
    (used != tick).then_some(i)
}

/// Canvas rectangle (x, y, w, h in 256x240 canvas pixels) → pixels of the picture rectangle
/// `at`, edges rounded to the nearest pixel; `None` when it comes out empty.
fn scaled(b: (i32, i32, i32, i32), at: Rect) -> Option<Rect> {
    let (sx, sy) = (at.width() as f32 / W as f32, at.height() as f32 / H as f32);
    let (left, top) = ((b.0 as f32 * sx).round() as i32, (b.1 as f32 * sy).round() as i32);
    let (right, bottom) = (((b.0 + b.2) as f32 * sx).round() as i32, ((b.1 + b.3) as f32 * sy).round() as i32);
    (right > left && bottom > top).then(|| Rect::new(at.x() + left, at.y() + top, (right - left) as u32, (bottom - top) as u32))
}

/// The largest rectangle with the proportions of a `pw`×`ph` picture inside `boxed`, centred.
fn fitted(pw: u32, ph: u32, boxed: Rect) -> Rect {
    let (pw, ph) = (pw.max(1) as u64, ph.max(1) as u64);
    let (bw, bh) = (boxed.width() as u64, boxed.height() as u64);
    // Narrower than the box → full height, else full width.
    let (w, h) = if pw * bh <= ph * bw { ((pw * bh + ph / 2) / ph, bh) } else { (bw, (ph * bw + pw / 2) / pw) };
    let (w, h) = (w.clamp(1, bw) as u32, h.clamp(1, bh) as u32);
    Rect::new(boxed.x() + ((boxed.width() - w) / 2) as i32, boxed.y() + ((boxed.height() - h) / 2) as i32, w, h)
}

/// CRT turn-on over `dest`: only the centred `open` fraction of the height shows, with
/// `flash` white added on top (a bright line that opens vertically, then settles).
fn draw_power(c: &mut WindowCanvas, dest: Rect, open: f32, flash: f32) -> Result<(), String> {
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

/// Where the picture of a `sw`×`sh` source goes in an `ow`×`oh` drawable.
///
/// - `Stretch`: the whole drawable.
/// - `Fit`: the largest rectangle of the picture's shape: 256:240 for a 256x240 source (the
///   interface and the NES, as before), 4:3 for every other size (the console's TV picture,
///   whatever its pixel grid).
/// - `Pixel`: the largest integer multiple of the source's own size. A frame taller than
///   wide (interlaced double height, 320x448) doubles the horizontal factor, so its half-high
///   lines stay whole pixels on a picture of the usual shape. A drawable smaller than one
///   picture falls back to `Fit`.
fn dest_rect(screen: ScreenMode, ow: u32, oh: u32, sw: u32, sh: u32) -> Rect {
    let (ow, oh) = (ow.max(1), oh.max(1));
    let (sw, sh) = (sw.max(1), sh.max(1));
    // Width of one source pixel in units of its height's factor.
    let wide = if sh > sw { sw * 2 } else { sw };
    let (w, h) = match screen {
        ScreenMode::Stretch => (ow, oh),
        ScreenMode::Pixel if ow >= wide && oh >= sh => {
            let k = (ow / wide).min(oh / sh);
            (wide * k, sh * k)
        }
        // Fit, and Pixel in a drawable smaller than one picture.
        _ => {
            let (aw, ah) = if (sw, sh) == (W, H) { (W as u64, H as u64) } else { (4, 3) };
            // Wider than the picture's shape → full height, else full width.
            if ow as u64 * ah > oh as u64 * aw {
                (((oh as u64 * aw + ah / 2) / ah) as u32, oh)
            } else {
                (ow, ((ow as u64 * ah + aw / 2) / aw) as u32)
            }
        }
    };
    let (w, h) = (w.clamp(1, ow), h.clamp(1, oh));
    Rect::new(((ow - w) / 2) as i32, ((oh - h) / 2) as i32, w, h)
}

/// `pos` in a window of `win` points whose drawable is `out` pixels (HiDPI: out = 2 × win on
/// Retina) → canvas pixel of the picture `dest_rect` draws for a `src`-sized source, through
/// the inverse of the screen curvature `curve` (0 = flat; the glass corners outside the
/// picture → `None`).
fn map_to_canvas(
    screen: ScreenMode,
    curve: f32,
    win: (u32, u32),
    out: (u32, u32),
    src: (u32, u32),
    pos: WindowPos,
) -> Option<(i32, i32)> {
    let (ww, wh) = (win.0.max(1) as f32, win.1.max(1) as f32);
    let (ow, oh) = (out.0.max(1) as f32, out.1.max(1) as f32);
    let (px, py) = match pos {
        WindowPos::Point(x, y) => (x * ow / ww, y * oh / wh),
        WindowPos::Touch(x, y) => (x * ow, y * oh),
    };
    let d = dest_rect(screen, out.0, out.1, src.0, src.1);
    let fx = (px - d.x() as f32) / d.width() as f32;
    let fy = (py - d.y() as f32) / d.height() as f32;
    let (fx, fy) = crt::warp(fx, fy, curve);
    if !(0.0..1.0).contains(&fx) || !(0.0..1.0).contains(&fy) {
        return None;
    }
    Some(((fx * W as f32) as i32, (fy * H as f32) as i32))
}

/// Integer prescale of a `sw`×`sh` layer for a `w`×`h` destination: 1 (draw directly with
/// nearest) when the destination is an exact integer multiple or not larger than the layer,
/// else the smallest factor covering it (capped), which is then scaled down linearly.
fn prescale(w: u32, h: u32, sw: u32, sh: u32) -> u32 {
    let (sw, sh) = (sw.max(1), sh.max(1));
    if w <= sw && h <= sh {
        return 1;
    }
    if w % sw == 0 && h % sh == 0 && w / sw == h / sh {
        return 1;
    }
    w.div_ceil(sw).max(h.div_ceil(sh)).clamp(1, MAX_PRESCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoPictures;

    impl Pictures for NoPictures {
        fn picture(&self, _: u64) -> Option<PictureRef<'_>> {
            None
        }
    }

    fn game(w: u32, h: u32, scale: u32) -> GameFrame<'static> {
        GameFrame { rgba: &[], w, h, scale }
    }

    #[test]
    fn enhanced_frames_keep_the_console_rectangle() {
        assert_eq!(game(256, 240, 1).own_size(), (256, 240));
        assert_eq!(game(1024, 960, 4).own_size(), (256, 240));
        assert_eq!(game(1280, 896, 4).own_size(), (320, 224));
        assert_eq!(game(640, 896, 2).own_size(), (320, 448));
        // A scale of 0 counts as 1; the picture is never empty.
        assert_eq!(game(320, 224, 0).own_size(), (320, 224));
        assert_eq!(game(2, 2, 4).own_size(), (1, 1));
        let outs = [(1280, 800), (640, 480), (600, 800), (1920, 1080), (300, 200)];
        for (hd, plain) in [(game(1024, 960, 4), game(256, 240, 1)), (game(1280, 896, 4), game(320, 224, 1))] {
            let size = |g| Layers { game: Some(g), ui: &[], placed: &[], pictures: &NoPictures }.size();
            let (sw, sh) = size(&hd);
            assert_eq!((sw, sh), (plain.w, plain.h));
            assert_eq!((sw, sh), size(&plain));
            for mode in [ScreenMode::Stretch, ScreenMode::Fit, ScreenMode::Pixel] {
                for (ow, oh) in outs {
                    assert_eq!(dest_rect(mode, ow, oh, sw, sh), dest_rect(mode, ow, oh, plain.w, plain.h), "{mode:?} {ow}x{oh}");
                    assert_eq!(prescale(ow, oh, sw, sh), prescale(ow, oh, plain.w, plain.h));
                }
            }
        }
        // The rectangles themselves, on the Deck's screen.
        let (sw, sh) = game(1024, 960, 4).own_size();
        assert_eq!(dest_rect(ScreenMode::Stretch, 1280, 800, sw, sh), Rect::new(0, 0, 1280, 800));
        assert_eq!(dest_rect(ScreenMode::Fit, 1280, 800, sw, sh), Rect::new(213, 0, 853, 800));
        assert_eq!(dest_rect(ScreenMode::Pixel, 1280, 800, sw, sh), Rect::new(256, 40, 768, 720));
        let (sw, sh) = game(1280, 896, 4).own_size();
        assert_eq!(dest_rect(ScreenMode::Stretch, 1280, 800, sw, sh), Rect::new(0, 0, 1280, 800));
        assert_eq!(dest_rect(ScreenMode::Fit, 1280, 800, sw, sh), Rect::new(106, 0, 1067, 800));
        assert_eq!(dest_rect(ScreenMode::Pixel, 1280, 800, sw, sh), Rect::new(160, 64, 960, 672));
        // Taps map through the same rectangle.
        let tap = |src| map_to_canvas(ScreenMode::Pixel, 0.0, (1280, 800), (1280, 800), src, WindowPos::Point(1119.0, 735.0));
        assert_eq!(tap((sw, sh)), Some((255, 239)));
    }

    #[test]
    fn crt_mask_rows_follow_the_console_lines() {
        for (w, h, scale, lines) in [(1024, 960, 4, 240usize), (1280, 896, 4, 224), (256, 240, 1, 240), (320, 224, 1, 224)] {
            let (sw, sh) = game(w, h, scale).own_size();
            for effect in [Effect::CrtTv, Effect::Pvm] {
                // A column too narrow for phosphor cells: only the scanlines shape it.
                let m = crt::mask(effect, 64, 1920, sw, sh);
                let at = |y: u32| m.rgba[(y * 64 * 4) as usize];
                let beams = (0..1920).filter(|&y| at(y) >= 200 && (y == 0 || at(y - 1) < 200)).count();
                assert_eq!(beams, lines, "{effect:?} {w}x{h} / {scale}");
            }
        }
    }

    #[test]
    fn ambient_light_only_in_the_bars_around_a_game() {
        let fit = dest_rect(ScreenMode::Fit, 1280, 800, 256, 240);
        assert!(ambient_wanted(true, true, fit, (1280, 800)));
        assert!(!ambient_wanted(false, true, fit, (1280, 800)));
        // Nothing in the menus, nothing when the picture fills the output.
        assert!(!ambient_wanted(true, false, fit, (1280, 800)));
        assert!(!ambient_wanted(true, true, dest_rect(ScreenMode::Stretch, 1280, 800, 256, 240), (1280, 800)));
        assert!(!ambient_wanted(true, true, dest_rect(ScreenMode::Fit, 640, 480, 320, 224), (640, 480)));
        // Bars above and below count too.
        assert!(ambient_wanted(true, true, dest_rect(ScreenMode::Fit, 600, 800, 320, 224), (600, 800)));
        assert!(ambient_wanted(true, true, dest_rect(ScreenMode::Pixel, 1280, 800, 320, 224), (1280, 800)));

        let (v, idx) = shade_rings(fit, (1280, 800));
        assert_eq!(v.len(), ((SHADE_RINGS + 1) * 4) as usize);
        assert_eq!(idx.len(), (SHADE_RINGS * 24) as usize);
        assert!(idx.iter().all(|&i| (i as usize) < v.len()));
        // Clear on the picture's edge, darkest on the output's, never brighter going out.
        assert_eq!((v[0], v[2]), ((213.0, 0.0, 0), (1066.0, 800.0, 0)));
        let outer = &v[v.len() - 4..];
        assert_eq!((outer[0].0, outer[0].1, outer[2].0, outer[2].1), (0.0, 0.0, 1280.0, 800.0));
        assert_eq!(outer[0].2, ((1.0 - AMBIENT_EDGE) * 255.0).round() as u8);
        assert!(v.windows(2).all(|p| p[1].2 >= p[0].2));
        // About 55 % of the blurred picture next to the picture, about 15 % at the output's edge.
        assert_eq!(AMBIENT, 0.55);
        assert!((AMBIENT * AMBIENT_EDGE - 0.15).abs() < 0.005);
        let half = &v[(SHADE_RINGS / 2 * 4) as usize..];
        assert_eq!(half[0].2, ((1.0 - AMBIENT_EDGE) * 0.5 * 255.0).round() as u8);
    }

    #[test]
    fn deck_screen_modes() {
        assert_eq!(dest_rect(ScreenMode::Stretch, 1280, 800, 256, 240), Rect::new(0, 0, 1280, 800));
        assert_eq!(dest_rect(ScreenMode::Fit, 1280, 800, 256, 240), Rect::new(213, 0, 853, 800));
        assert_eq!(dest_rect(ScreenMode::Pixel, 1280, 800, 256, 240), Rect::new(256, 40, 768, 720));
        assert_eq!(dest_rect(ScreenMode::Fit, 256, 400, 256, 240), Rect::new(0, 80, 256, 240));
        assert_eq!(dest_rect(ScreenMode::Pixel, 200, 100, 256, 240).height(), 100);
        assert_eq!(prescale(768, 720, 256, 240), 1);
        assert_eq!(prescale(1280, 800, 256, 240), 5);
        assert_eq!(prescale(853, 800, 256, 240), 4);
        assert_eq!(prescale(200, 100, 256, 240), 1);
        assert_eq!(prescale(7680, 4320, 256, 240), MAX_PRESCALE);
    }

    #[test]
    fn picture_rect_follows_the_game_size() {
        // Stretch: the whole drawable whatever the source.
        for (sw, sh) in [(256, 240), (320, 224), (320, 448)] {
            for (ow, oh) in [(1280, 800), (640, 480), (600, 800), (1920, 1080)] {
                assert_eq!(dest_rect(ScreenMode::Stretch, ow, oh, sw, sh), Rect::new(0, 0, ow, oh));
            }
        }
        // Fit: 4:3 for the Mega Drive's sizes, 256:240 for a 256x240 source.
        for (sw, sh) in [(320, 224), (320, 448)] {
            assert_eq!(dest_rect(ScreenMode::Fit, 1280, 800, sw, sh), Rect::new(106, 0, 1067, 800));
            assert_eq!(dest_rect(ScreenMode::Fit, 640, 480, sw, sh), Rect::new(0, 0, 640, 480));
            assert_eq!(dest_rect(ScreenMode::Fit, 600, 800, sw, sh), Rect::new(0, 175, 600, 450));
            assert_eq!(dest_rect(ScreenMode::Fit, 1920, 1080, sw, sh), Rect::new(240, 0, 1440, 1080));
        }
        assert_eq!(dest_rect(ScreenMode::Fit, 640, 480, 256, 240), Rect::new(64, 0, 512, 480));
        assert_eq!(dest_rect(ScreenMode::Fit, 600, 800, 256, 240), Rect::new(0, 118, 600, 563));
        assert_eq!(dest_rect(ScreenMode::Fit, 1920, 1080, 256, 240), Rect::new(384, 0, 1152, 1080));
        // Pixel: integer multiples of the game's own size.
        assert_eq!(dest_rect(ScreenMode::Pixel, 1280, 800, 320, 224), Rect::new(160, 64, 960, 672));
        assert_eq!(dest_rect(ScreenMode::Pixel, 1920, 1080, 320, 224), Rect::new(320, 92, 1280, 896));
        assert_eq!(dest_rect(ScreenMode::Pixel, 640, 480, 320, 224), Rect::new(0, 16, 640, 448));
        assert_eq!(dest_rect(ScreenMode::Pixel, 640, 480, 256, 240), Rect::new(64, 0, 512, 480));
        assert_eq!(dest_rect(ScreenMode::Pixel, 1920, 1080, 256, 240), Rect::new(448, 60, 1024, 960));
        // Interlaced double height: half-high lines, the usual shape.
        assert_eq!(dest_rect(ScreenMode::Pixel, 1280, 800, 320, 448), Rect::new(320, 176, 640, 448));
        assert_eq!(dest_rect(ScreenMode::Pixel, 1920, 1080, 320, 448), Rect::new(320, 92, 1280, 896));
        assert_eq!(dest_rect(ScreenMode::Pixel, 640, 480, 320, 448), Rect::new(0, 16, 640, 448));
        // Smaller than one picture: as Fit.
        assert_eq!(dest_rect(ScreenMode::Pixel, 300, 200, 320, 224), Rect::new(16, 0, 267, 200));
        assert_eq!(dest_rect(ScreenMode::Pixel, 400, 500, 320, 448), Rect::new(0, 100, 400, 300));
        assert_eq!(dest_rect(ScreenMode::Pixel, 300, 200, 320, 224), dest_rect(ScreenMode::Fit, 300, 200, 320, 224));
    }

    #[test]
    fn prescale_uses_the_layer_size() {
        // Exact integer multiples and smaller destinations are drawn directly.
        assert_eq!(prescale(960, 672, 320, 224), 1);
        assert_eq!(prescale(320, 224, 320, 224), 1);
        assert_eq!(prescale(300, 200, 320, 224), 1);
        // Else the smallest factor covering the rectangle.
        assert_eq!(prescale(1067, 800, 320, 224), 4);
        assert_eq!(prescale(1067, 800, 320, 448), 4);
        assert_eq!(prescale(1440, 1080, 320, 224), 5);
        assert_eq!(prescale(640, 448, 320, 448), 2);
        assert_eq!(prescale(7680, 4320, 320, 224), MAX_PRESCALE);
        // The interface over a game's rectangle.
        assert_eq!(prescale(960, 672, 256, 240), 4);
    }

    #[test]
    fn pictures_fit_their_boxes() {
        // A 44x62 tile at canvas (10, 34), the canvas shown 4x.
        let at = Rect::new(0, 0, 1024, 960);
        let tile = scaled((10, 34, 44, 62), at).unwrap();
        assert_eq!(tile, Rect::new(40, 136, 176, 248));
        // Upright box art: full height, a pixel short of the width.
        assert_eq!(fitted(512, 727, tile), Rect::new(40, 136, 175, 248));
        // A 4:3 title screen sits in the middle.
        assert_eq!(fitted(320, 240, tile), Rect::new(40, 194, 176, 132));
        assert_eq!(fitted(176, 248, tile), tile);
        assert_eq!(fitted(1, 1000, tile), Rect::new(127, 136, 1, 248));
        // The rectangle's origin and a non-integer scale (853x800 at x 213).
        let fit = Rect::new(213, 0, 853, 800);
        assert_eq!(scaled((0, 0, 256, 240), fit), Some(fit));
        assert_eq!(scaled((10, 34, 44, 62), fit), Some(Rect::new(246, 113, 147, 207)));
        assert_eq!(scaled((10, 34, 0, 62), fit), None);
        assert_eq!(scaled((10, 34, 44, -3), fit), None);
    }

    #[test]
    fn least_recently_drawn_picture_goes() {
        assert_eq!(oldest([5, 3, 9, 3].into_iter(), 9), Some(1));
        assert_eq!(oldest([9, 9, 8].into_iter(), 9), Some(2));
        // Everything was drawn this frame: nothing to drop.
        assert_eq!(oldest([9, 9].into_iter(), 9), None);
        assert_eq!(oldest(std::iter::empty(), 9), None);
    }

    #[test]
    fn window_to_canvas_all_modes() {
        let deck = |mode, pos| map_to_canvas(mode, 0.0, (1280, 800), (1280, 800), (256, 240), pos);
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
        let retina = map_to_canvas(ScreenMode::Pixel, 0.0, (640, 400), (1280, 800), (256, 240), WindowPos::Point(129.5, 21.5));
        assert_eq!(retina, Some((1, 1)));
        assert_eq!(map_to_canvas(ScreenMode::Fit, 0.0, (640, 400), (1280, 800), (256, 240), WindowPos::Point(50.0, 200.0)), None);
        // Flat effects map exactly like Off.
        for effect in Effect::ALL {
            if effect.params().curve == 0.0 {
                for pos in [WindowPos::Point(259.0, 43.0), WindowPos::Touch(0.31, 0.77)] {
                    assert_eq!(
                        map_to_canvas(ScreenMode::Fit, effect.params().curve, (1280, 800), (1280, 800), (256, 240), pos),
                        deck(ScreenMode::Fit, pos)
                    );
                }
            }
        }
    }

    #[test]
    fn window_to_canvas_over_a_game() {
        // A 320x224 game, Pixel: 3x at (160, 64), 960x672; the canvas is stretched over it.
        let game = |mode, pos| map_to_canvas(mode, 0.0, (1280, 800), (1280, 800), (320, 224), pos);
        assert_eq!(game(ScreenMode::Pixel, WindowPos::Point(160.0, 64.0)), Some((0, 0)));
        assert_eq!(game(ScreenMode::Pixel, WindowPos::Point(1119.0, 735.0)), Some((255, 239)));
        assert_eq!(game(ScreenMode::Pixel, WindowPos::Point(159.0, 400.0)), None);
        assert_eq!(game(ScreenMode::Pixel, WindowPos::Point(640.0, 736.0)), None);
        assert_eq!(game(ScreenMode::Pixel, WindowPos::Touch(0.5, 0.5)), Some((128, 120)));
        // Fit: 4:3 at x 106..1173.
        assert_eq!(game(ScreenMode::Fit, WindowPos::Point(105.0, 400.0)), None);
        assert_eq!(game(ScreenMode::Fit, WindowPos::Point(106.0, 0.0)), Some((0, 0)));
        assert_eq!(game(ScreenMode::Fit, WindowPos::Point(1172.0, 799.0)), Some((255, 239)));
    }

    #[test]
    fn curved_taps_land_on_drawn_pixel() {
        let k = Effect::CrtTv.params().curve;
        let d = dest_rect(ScreenMode::Fit, 1280, 800, 256, 240);
        for (cx, cy) in [(128, 120), (10, 12), (240, 200), (5, 230), (128, 3)] {
            // Where the curved mesh draws the centre of canvas pixel (cx, cy).
            let (sx, sy) = crt::unwarp((cx as f32 + 0.5) / W as f32, (cy as f32 + 0.5) / H as f32, k);
            let pos = WindowPos::Point(d.x() as f32 + sx * d.width() as f32, d.y() as f32 + sy * d.height() as f32);
            let (x, y) = map_to_canvas(ScreenMode::Fit, k, (1280, 800), (1280, 800), (256, 240), pos).expect("on the picture");
            assert!((x - cx).abs() <= 1 && (y - cy).abs() <= 1, "({cx},{cy}) → ({x},{y})");
        }
        // The black glass corner is not the picture.
        assert_eq!(
            map_to_canvas(ScreenMode::Fit, k, (1280, 800), (1280, 800), (256, 240), WindowPos::Point(214.0, 1.0)),
            None
        );
    }
}
