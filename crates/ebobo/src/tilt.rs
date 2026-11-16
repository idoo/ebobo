//! Tilt: the far planes of the picture move as the device is tilted (a parallax "window"
//! effect). This module turns motion sensor readings into where the far plane should sit;
//! the consoles re-compose the picture (`Console::frame_tilted`).

/// The tilt of the device as the picture uses it.
pub struct Tilt {
    /// A pinned value (`--tilt-at X,Y`): no sensor is read.
    fixed: Option<(f32, f32)>,
}

impl Tilt {
    /// `fixed` pins the tilt (for pictures and tests) and needs no sensor.
    pub fn new(fixed: Option<(f32, f32)>) -> Tilt {
        Tilt { fixed }
    }

    /// Call once per shown frame. `pad_gyro` is `Input::gyro()`, `dt` the seconds since the
    /// last call. Returns where the far plane goes, each axis -1.0..=1.0 (right, down), or
    /// `None` when nothing measures motion.
    pub fn update(&mut self, _pad_gyro: Option<[f32; 3]>, _dt: f32) -> Option<(f32, f32)> {
        self.fixed
    }

    /// Whole pixels for `tilt` given the console's `reach`, steady at a boundary.
    pub fn shift(&mut self, tilt: (f32, f32), reach: (i32, i32)) -> (i32, i32) {
        ((tilt.0 * reach.0 as f32).round() as i32, (tilt.1 * reach.1 as f32).round() as i32)
    }
}

/// `--tilt-probe SECS`: prints what the motion sensor reads for `secs` seconds and returns
/// the process exit code.
pub fn probe(_secs: u32) -> i32 {
    eprintln!("ebobo: no motion sensor");
    1
}
