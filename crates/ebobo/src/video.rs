//! Window and frame presentation. Owner: platform slice.

use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::rect::Rect;
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{FullscreenType, Window, WindowContext};

use crate::config::ScreenMode;

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
    fullscreen: bool,
    screen: ScreenMode,
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
        mouse.show_cursor(!fullscreen);
        Ok(Video {
            texture,
            prescaled: None,
            targets,
            _creator: creator,
            canvas,
            mouse,
            fullscreen,
            screen,
        })
    }

    /// Shows one 256x240 RGBA8888 picture (game plus UI overlay already composited).
    pub fn present(&mut self, rgba: &[u8]) -> Result<(), String> {
        if rgba.len() < (W * H * 4) as usize {
            return Err(format!("frame too small: {} bytes", rgba.len()));
        }
        // Computed every frame from the drawable size, so resizes and fullscreen toggles apply.
        let (ow, oh) = self.canvas.output_size()?;
        let dest = dest_rect(self.screen, ow, oh);
        self.texture
            .update(None, &rgba[..(W * H * 4) as usize], (W * 4) as usize)
            .map_err(|e| e.to_string())?;
        self.canvas.set_draw_color(Color::RGB(0, 0, 0));
        self.canvas.clear();
        let k = prescale(dest.width(), dest.height());
        if k > 1 && self.draw_prescaled(k) {
            let (target, _) = self.prescaled.as_ref().expect("drawn above");
            self.canvas.copy(target, None, dest)?;
        } else {
            self.canvas.copy(&self.texture, None, dest)?;
        }
        self.canvas.present();
        Ok(())
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
        self.mouse.show_cursor(!self.fullscreen);
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
}
