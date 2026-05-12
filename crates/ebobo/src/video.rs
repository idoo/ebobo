//! Window and frame presentation. Owner: platform slice.

use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::render::{Canvas, Texture, TextureCreator};
use sdl2::video::{FullscreenType, Window, WindowContext};

const W: u32 = 256;
const H: u32 = 240;

/// SDL window + renderer + streaming 256x240 texture.
pub struct Video {
    // `unsafe_textures`: the texture is freed with the renderer; keep the creator alive.
    texture: Texture,
    _creator: TextureCreator<WindowContext>,
    canvas: Canvas<Window>,
    mouse: sdl2::mouse::MouseUtil,
    fullscreen: bool,
    integer_scale: bool,
}

impl Video {
    /// Creates the window (`scale` × 256x240 in windowed mode, desktop fullscreen when
    /// `fullscreen`). Nearest-neighbour scaling, letterboxed, integer scale when it fits.
    pub fn new(sdl: &sdl2::Sdl, fullscreen: bool, scale: u32) -> Result<Video, String> {
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
        canvas
            .set_logical_size(W, H)
            .map_err(|e| e.to_string())?;
        canvas.set_integer_scale(true)?;
        canvas.set_draw_color(Color::RGB(0, 0, 0));
        canvas.clear();
        canvas.present();
        let creator = canvas.texture_creator();
        let texture = creator
            .create_texture_streaming(PixelFormatEnum::ABGR8888, W, H)
            .map_err(|e| e.to_string())?;
        let mouse = sdl.mouse();
        mouse.show_cursor(!fullscreen);
        Ok(Video {
            texture,
            _creator: creator,
            canvas,
            mouse,
            fullscreen,
            integer_scale: true,
        })
    }

    /// Shows one 256x240 RGBA8888 picture (game plus UI overlay already composited).
    pub fn present(&mut self, rgba: &[u8]) -> Result<(), String> {
        if rgba.len() < (W * H * 4) as usize {
            return Err(format!("frame too small: {} bytes", rgba.len()));
        }
        // Integer scale only when the drawable holds at least one full 256x240 picture.
        let (ow, oh) = self.canvas.output_size()?;
        let fits = ow >= W && oh >= H;
        if fits != self.integer_scale {
            self.canvas.set_integer_scale(fits)?;
            self.integer_scale = fits;
        }
        self.texture
            .update(None, &rgba[..(W * H * 4) as usize], (W * 4) as usize)
            .map_err(|e| e.to_string())?;
        self.canvas.set_draw_color(Color::RGB(0, 0, 0));
        self.canvas.clear();
        self.canvas.copy(&self.texture, None, None)?;
        self.canvas.present();
        Ok(())
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
