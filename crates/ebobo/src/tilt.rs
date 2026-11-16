//! Tilt: the far planes of the picture move as the device is tilted (a parallax "window"
//! effect). This module turns motion sensor readings into where the far plane should sit;
//! the consoles re-compose the picture (`Console::frame_tilted`).
//!
//! The sensor is a Steam Deck's own gyroscope, read from its HID device, or the gyroscope of
//! a gamepad (`Input::gyro`). Turn rates are in rad/s on SDL's controller axes: x points
//! right, y out of the face toward the viewer, z toward the player (the bottom edge).

/// Turn that moves the far plane all the way, in radians (12 degrees).
const RANGE: f32 = 12.0 * DEG;
/// Time constant of the return to centre, seconds.
const LEAK_SECS: f32 = 4.0;
/// Time constant of the rest bias estimate once it has settled, seconds. Long, so that a
/// slow turn of the device is not taken for bias and lost.
const BIAS_SECS: f32 = 20.0;
/// The device counts as still while every axis is this close to the rest bias estimate.
const STILL_BAND: f32 = 3.0 * DEG;
/// It also counts as still after turning this evenly (around its own short average)...
const STEADY_BAND: f32 = 1.0 * DEG;
/// ...for this long...
const STEADY_SECS: f32 = 1.0;
/// ...no faster than this: a rest bias larger than `STILL_BAND`, or a wrong estimate.
const STEADY_MAX: f32 = 10.0 * DEG;
/// Time constant of that short average, seconds.
const SMOOTH_SECS: f32 = 0.5;
/// A pixel step is left only when the value is further than this from it, in pixels.
const STEP_SLACK: f32 = 0.7;
const DEG: f32 = std::f32::consts::PI / 180.0;

/// The far plane sits behind the glass, so it moves toward the side the viewer is on.
/// A positive turn about z makes the screen face to the left of the viewer; the viewer is
/// then on its right, and the far plane goes right (+x).
/// The sign comes from SDL's documented axes and has not been felt on a device.
const X_FROM_Z: f32 = 1.0;
/// A positive turn about x makes the screen face downward; the viewer is then above it, and
/// the far plane goes up (-y).
/// The sign comes from SDL's documented axes and has not been felt on a device.
const Y_FROM_X: f32 = -1.0;

/// Turn rates → tilt. Subtracts the rest bias of the gyroscope and sums the turn into an
/// angle that leaks back to zero, so the picture returns to centre while the device is held
/// still in any pose.
struct Filter {
    /// Summed turn about x and about z, radians, each within ± `RANGE`.
    angle_x: f32,
    angle_z: f32,
    /// What the gyroscope reads at rest.
    bias: [f32; 3],
    /// Seconds of stillness the bias estimate rests on.
    learned: f32,
    /// Short average of the rate, and how long the rate has stayed close to it.
    smooth: [f32; 3],
    steady: f32,
    /// No reading yet.
    fresh: bool,
}

impl Filter {
    fn new() -> Filter {
        Filter {
            angle_x: 0.0,
            angle_z: 0.0,
            bias: [0.0; 3],
            learned: 0.0,
            smooth: [0.0; 3],
            steady: 0.0,
            fresh: true,
        }
    }

    /// Forgets everything: the picture starts centred.
    fn reset(&mut self) {
        *self = Filter::new();
    }

    /// One reading: `rate` in rad/s over the last `dt` seconds.
    fn feed(&mut self, rate: [f32; 3], dt: f32) {
        if !(dt > 0.0) || rate.iter().any(|r| !r.is_finite()) {
            return;
        }
        let far = |a: &[f32; 3], b: &[f32; 3]| {
            (0..3).map(|i| (a[i] - b[i]).abs()).fold(0.0f32, f32::max)
        };
        if self.fresh {
            self.fresh = false;
            self.smooth = rate;
        }
        let k = 1.0 - (-dt / SMOOTH_SECS).exp();
        for i in 0..3 {
            self.smooth[i] += (rate[i] - self.smooth[i]) * k;
        }
        let even = far(&rate, &self.smooth) < STEADY_BAND && far(&rate, &[0.0; 3]) < STEADY_MAX;
        self.steady = if even { self.steady + dt } else { 0.0 };

        if far(&rate, &self.bias) < STILL_BAND {
            // The mean of the readings during the first seconds of stillness, a slow
            // follower after: a constant bias is gone from the first reading on.
            self.learned += dt;
            let k = (dt / self.learned.min(BIAS_SECS)).min(1.0);
            for i in 0..3 {
                self.bias[i] += (rate[i] - self.bias[i]) * k;
            }
        } else if self.steady >= STEADY_SECS {
            // Lying still with a reading far from the estimate: the estimate is wrong.
            self.bias = self.smooth;
            self.learned = 0.0;
        }

        let leak = (-dt / LEAK_SECS).exp();
        self.angle_x = ((self.angle_x + (rate[0] - self.bias[0]) * dt) * leak).clamp(-RANGE, RANGE);
        self.angle_z = ((self.angle_z + (rate[2] - self.bias[2]) * dt) * leak).clamp(-RANGE, RANGE);
    }

    /// Where the far plane goes, each axis -1.0..=1.0 (right, down).
    fn tilt(&self) -> (f32, f32) {
        (
            (X_FROM_Z * self.angle_z / RANGE).clamp(-1.0, 1.0),
            (Y_FROM_X * self.angle_x / RANGE).clamp(-1.0, 1.0),
        )
    }
}

/// One motion reading on SDL's controller axes.
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Motion {
    /// Turn rate, rad/s.
    gyro: [f32; 3],
    /// Acceleration in g (1.0 at rest).
    accel: [f32; 3],
}

/// Seconds between two reports of a Steam Deck (250 per second).
#[cfg(any(target_os = "linux", test))]
const DECK_REPORT_SECS: f32 = 0.004;

/// `HID_ID` of a Steam Deck's controller (USB, Valve 28DE:1205) in a hidraw `uevent`.
#[cfg(any(target_os = "linux", test))]
const DECK_HID_ID: &str = "0003:000028DE:00001205";

/// Whether a hidraw device's `uevent` text names a Steam Deck's controller. Three interfaces
/// share the id; one of them delivers the state reports.
#[cfg(any(target_os = "linux", test))]
fn is_deck_uevent(text: &str) -> bool {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("HID_ID="))
        .any(|id| id.eq_ignore_ascii_case(DECK_HID_ID))
}

/// Decodes a Steam Deck state report; `None` for anything else. Layout as in Linux
/// `drivers/hid/hid-steam.c` (the deck report table) and SDL2
/// `src/joystick/hidapi/SDL_hidapi_steamdeck.c`: 64 bytes starting 01 00 09, little-endian
/// i16 accelerometer X, Y, Z at 24, 26, 28 (16384 per g) and gyroscope X, Y, Z at 30, 32, 34
/// (full scale 2000 degrees/s). The Deck's Y and Z are SDL's -Z and Y.
#[cfg(any(target_os = "linux", test))]
fn parse_deck_report(buf: &[u8]) -> Option<Motion> {
    if buf.len() < 64 || buf[0] != 0x01 || buf[1] != 0x00 || buf[2] != 0x09 {
        return None;
    }
    let at = |i: usize| i16::from_le_bytes([buf[i], buf[i + 1]]) as f32;
    let g = |i: usize| at(i) / 16384.0;
    let r = |i: usize| at(i) / 32768.0 * 2000.0 * DEG;
    Some(Motion { gyro: [r(30), r(34), -r(32)], accel: [g(24), g(28), -g(26)] })
}

/// The Steam Deck's own motion sensor. In Game Mode Steam hides the Deck's controller behind
/// a virtual Xbox pad without sensors, and the bundled SDL 2.26 has no Deck driver, so the
/// HID device is read directly.
#[cfg(any(target_os = "linux", all(test, unix)))]
mod deck_hid {
    use super::{is_deck_uevent, parse_deck_report, Motion};
    use std::fs::File;
    use std::io::{ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    /// Linux `O_NONBLOCK`. Elsewhere this code only reads plain files in tests.
    #[cfg(target_os = "linux")]
    const O_NONBLOCK: i32 = 0o4000;
    #[cfg(not(target_os = "linux"))]
    const O_NONBLOCK: i32 = 0;

    /// Seconds between two searches while there is no device.
    const RETRY_SECS: f32 = 3.0;
    /// A device that stays silent this long is dropped and searched for again.
    const QUIET_SECS: f32 = 2.0;
    /// Reports read in one go at most (the kernel queues 64 per device).
    const READ_MAX: usize = 512;

    pub struct Sensor {
        /// Where hidraw devices are listed and where their nodes are.
        sys: PathBuf,
        dev: PathBuf,
        /// Open devices: every candidate until one delivers a state report, then that one.
        open: Vec<(PathBuf, File)>,
        chosen: bool,
        /// Seconds until the next search, and seconds the chosen device has been silent.
        retry: f32,
        quiet: f32,
        /// The "found" and the "cannot open" lines have been printed.
        announced: bool,
        complained: bool,
        /// Why there is no reading, for `probe`.
        problem: String,
    }

    impl Sensor {
        pub fn new() -> Sensor {
            Sensor::at(Path::new("/sys/class/hidraw"), Path::new("/dev"))
        }

        pub fn at(sys: &Path, dev: &Path) -> Sensor {
            Sensor {
                sys: sys.to_owned(),
                dev: dev.to_owned(),
                open: Vec::new(),
                chosen: false,
                retry: 0.0,
                quiet: 0.0,
                announced: false,
                complained: false,
                problem: "no Steam Deck controller among the hidraw devices".to_owned(),
            }
        }

        /// Why no report has arrived.
        pub fn why(&self) -> &str {
            &self.problem
        }

        /// Opens every hidraw device that is a Deck's controller.
        fn search(&mut self) {
            let mut names: Vec<_> = std::fs::read_dir(&self.sys)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name())
                .collect();
            names.sort();
            let mut found = false;
            for name in names {
                let uevent = self.sys.join(&name).join("device/uevent");
                if !std::fs::read_to_string(uevent).is_ok_and(|t| is_deck_uevent(&t)) {
                    continue;
                }
                found = true;
                let path = self.dev.join(&name);
                match File::options().read(true).custom_flags(O_NONBLOCK).open(&path) {
                    Ok(f) => self.open.push((path, f)),
                    Err(e) => {
                        self.problem = format!("{}: {e}", path.display());
                        if !self.complained {
                            self.complained = true;
                            eprintln!("ebobo: tilt: {}: {e}", path.display());
                        }
                    }
                }
            }
            if !self.open.is_empty() {
                self.problem = "the Steam Deck controller sends no state reports".to_owned();
            } else if !found {
                self.problem = "no Steam Deck controller among the hidraw devices".to_owned();
            }
        }

        /// Hands every pending report to `on_report`; `dt` is the time since the last call.
        /// True while a device delivers.
        pub fn poll(&mut self, dt: f32, on_report: &mut impl FnMut(&Motion)) -> bool {
            if self.open.is_empty() {
                self.chosen = false;
                self.retry -= dt;
                if self.retry > 0.0 {
                    return false;
                }
                self.retry = RETRY_SECS;
                self.search();
            }
            let mut buf = [0u8; 64];
            let mut i = 0;
            while i < self.open.len() {
                let mut got = 0;
                // A read error: the device is gone (it re-enumerates after sleep).
                let mut gone = false;
                for _ in 0..READ_MAX {
                    match self.open[i].1.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Some(m) = parse_deck_report(&buf[..n]) {
                                got += 1;
                                on_report(&m);
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(_) => {
                            gone = true;
                            break;
                        }
                    }
                }
                if got > 0 && !self.chosen {
                    // This interface is the one: close the others.
                    let keep = self.open.swap_remove(i);
                    self.open.clear();
                    self.open.push(keep);
                    self.chosen = true;
                    self.quiet = 0.0;
                    if !self.announced {
                        self.announced = true;
                        println!("ebobo: tilt: Steam Deck sensor {}", self.open[0].0.display());
                    }
                    i = 0;
                }
                if gone {
                    self.open.remove(i);
                    self.retry = RETRY_SECS;
                    continue;
                }
                if self.chosen {
                    self.quiet = if got > 0 { 0.0 } else { self.quiet + dt };
                    if self.quiet > QUIET_SECS {
                        self.open.clear();
                        self.retry = RETRY_SECS;
                    }
                    break;
                }
                i += 1;
            }
            self.chosen && !self.open.is_empty()
        }
    }
}

#[cfg(target_os = "linux")]
use deck_hid::Sensor as Deck;

/// No built-in motion sensor on this system.
#[cfg(not(target_os = "linux"))]
struct Deck;

#[cfg(not(target_os = "linux"))]
impl Deck {
    fn new() -> Deck {
        Deck
    }

    /// Feeds the pending reports to `filter`; true while the sensor delivers.
    fn feed(&mut self, _dt: f32, _filter: &mut Filter) -> bool {
        false
    }
}

#[cfg(target_os = "linux")]
impl Deck {
    /// Feeds the pending reports to `filter`; true while the sensor delivers.
    fn feed(&mut self, dt: f32, filter: &mut Filter) -> bool {
        self.poll(dt, &mut |m: &Motion| filter.feed(m.gyro, DECK_REPORT_SECS))
    }
}

/// What the filter is fed from; a change starts it afresh.
#[derive(Clone, Copy, PartialEq)]
enum Source {
    None,
    Deck,
    Pad,
}

/// The tilt of the device as the picture uses it.
pub struct Tilt {
    /// A pinned value (`--tilt-at X,Y`): no sensor is read.
    fixed: Option<(f32, f32)>,
    filter: Filter,
    deck: Deck,
    source: Source,
    /// The pixel step `shift` last returned.
    step: (i32, i32),
}

impl Tilt {
    /// `fixed` pins the tilt (for pictures and tests) and needs no sensor.
    pub fn new(fixed: Option<(f32, f32)>) -> Tilt {
        Tilt { fixed, filter: Filter::new(), deck: Deck::new(), source: Source::None, step: (0, 0) }
    }

    /// Call once per shown frame. `pad_gyro` is `Input::gyro()`, `dt` the seconds since the
    /// last call. Returns where the far plane goes, each axis -1.0..=1.0 (right, down), or
    /// `None` when nothing measures motion.
    pub fn update(&mut self, pad_gyro: Option<[f32; 3]>, dt: f32) -> Option<(f32, f32)> {
        if self.fixed.is_some() {
            return self.fixed;
        }
        // A Deck's sensor wins over a pad's; a pad's readings never mix into it.
        if self.source != Source::Deck {
            let mut probe = Filter::new();
            if self.deck.feed(dt, &mut probe) {
                self.filter = probe;
                self.source = Source::Deck;
                return Some(self.filter.tilt());
            }
        } else if self.deck.feed(dt, &mut self.filter) {
            return Some(self.filter.tilt());
        }
        match pad_gyro {
            Some(rate) => {
                if self.source != Source::Pad {
                    self.filter.reset();
                    self.source = Source::Pad;
                }
                self.filter.feed(rate, dt.min(0.1));
                Some(self.filter.tilt())
            }
            None => {
                // Centred when a sensor appears.
                self.filter.reset();
                self.source = Source::None;
                None
            }
        }
    }

    /// Whole pixels for `tilt` given the console's `reach`, steady at a boundary: the step
    /// is kept until the value is more than `STEP_SLACK` pixels away, so a trembling hand
    /// does not make the plane flicker between two positions.
    pub fn shift(&mut self, tilt: (f32, f32), reach: (i32, i32)) -> (i32, i32) {
        let axis = |tilt: f32, reach: i32, step: i32| {
            let reach = reach.max(0);
            let v = (tilt * reach as f32).clamp(-(reach as f32), reach as f32);
            let step = if (v - step as f32).abs() > STEP_SLACK { v.round() as i32 } else { step };
            step.clamp(-reach, reach)
        };
        self.step = (axis(tilt.0, reach.0, self.step.0), axis(tilt.1, reach.1, self.step.1));
        self.step
    }
}

/// `--tilt-probe SECS`: prints what the motion sensor reads for `secs` seconds and returns
/// the process exit code.
#[cfg(target_os = "linux")]
pub fn probe(secs: u32) -> i32 {
    use std::time::{Duration, Instant};
    let mut deck = Deck::new();
    let mut filter = Filter::new();
    let start = Instant::now();
    let (mut last, mut printed) = (start, start);
    let mut total = 0u64;
    // Reports, summed rate and summed acceleration since the last line.
    let mut sum = (0u32, [0.0f32; 3], [0.0f32; 3]);
    while start.elapsed() < Duration::from_secs(secs as u64) {
        std::thread::sleep(Duration::from_millis(2));
        let now = Instant::now();
        deck.poll((now - last).as_secs_f32(), &mut |m: &Motion| {
            filter.feed(m.gyro, DECK_REPORT_SECS);
            sum.0 += 1;
            for i in 0..3 {
                sum.1[i] += m.gyro[i];
                sum.2[i] += m.accel[i];
            }
        });
        last = now;
        let span = (now - printed).as_secs_f32();
        if span < 0.5 {
            continue;
        }
        printed = now;
        total += sum.0 as u64;
        let n = sum.0.max(1) as f32;
        let r = sum.1.map(|v| v / n / DEG);
        let a = sum.2.map(|v| v / n);
        let len = (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt();
        let t = filter.tilt();
        println!(
            "ebobo: tilt: {:3.0}/s  gyro {:+7.1} {:+7.1} {:+7.1} deg/s  accel {:+5.2} {:+5.2} {:+5.2} g |{:.2}|  tilt {:+5.2} {:+5.2}",
            sum.0 as f32 / span, r[0], r[1], r[2], a[0], a[1], a[2], len, t.0, t.1
        );
        sum = (0, [0.0; 3], [0.0; 3]);
    }
    if total + sum.0 as u64 > 0 {
        return 0;
    }
    eprintln!("ebobo: tilt: no reports: {}", deck.why());
    1
}

/// `--tilt-probe SECS`: prints what the motion sensor reads for `secs` seconds and returns
/// the process exit code.
#[cfg(not(target_os = "linux"))]
pub fn probe(_secs: u32) -> i32 {
    eprintln!(
        "ebobo: tilt: no built-in motion sensor on this system (a gamepad's gyroscope is read inside the app)"
    );
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 0.004;

    fn run(f: &mut Filter, rate: [f32; 3], secs: f32) {
        for _ in 0..(secs / DT).round() as usize {
            f.feed(rate, DT);
        }
    }

    #[test]
    fn rest_bias_does_not_move_the_picture() {
        let mut f = Filter::new();
        let bias = [1.5 * DEG; 3];
        run(&mut f, bias, 3.0);
        for _ in 0..(30.0 / DT) as usize {
            f.feed(bias, DT);
            let (x, y) = f.tilt();
            assert!(x.abs() < 0.05 && y.abs() < 0.05, "{x} {y}");
        }
    }

    #[test]
    fn large_rest_bias_is_learned_too() {
        let mut f = Filter::new();
        let bias = [6.0 * DEG, -5.0 * DEG, 7.0 * DEG];
        run(&mut f, bias, 20.0);
        for _ in 0..(10.0 / DT) as usize {
            f.feed(bias, DT);
            let (x, y) = f.tilt();
            assert!(x.abs() < 0.05 && y.abs() < 0.05, "{x} {y}");
        }
    }

    #[test]
    fn quick_turn_about_z_moves_right_and_returns() {
        let mut f = Filter::new();
        run(&mut f, [0.0; 3], 1.0);
        run(&mut f, [0.0, 0.0, 60.0 * DEG], 0.1);
        let (x, y) = f.tilt();
        assert!((x - 0.5).abs() < 0.03, "{x}");
        assert!(y.abs() < 0.01, "{y}");
        run(&mut f, [0.0; 3], 15.0);
        assert!(f.tilt().0.abs() < 0.05, "{}", f.tilt().0);
    }

    #[test]
    fn quick_turn_about_x_moves_up_and_returns() {
        let mut f = Filter::new();
        run(&mut f, [0.0; 3], 1.0);
        run(&mut f, [60.0 * DEG, 0.0, 0.0], 0.1);
        let (x, y) = f.tilt();
        assert!((y + 0.5).abs() < 0.03, "{y}");
        assert!(x.abs() < 0.01, "{x}");
        run(&mut f, [0.0; 3], 15.0);
        assert!(f.tilt().1.abs() < 0.05, "{}", f.tilt().1);
    }

    #[test]
    fn long_rotation_clamps_without_wind_up() {
        for sign in [1.0f32, -1.0] {
            let mut f = Filter::new();
            run(&mut f, [0.0, 0.0, sign * 100.0 * DEG], 3.0);
            assert_eq!(f.tilt().0, sign);
            // Wound up, it would stay at the limit for seconds after the motion stopped.
            run(&mut f, [0.0; 3], 1.0);
            let x = f.tilt().0 * sign;
            assert!(x > 0.6 && x < 0.85, "{x}");
            run(&mut f, [0.0; 3], 14.0);
            assert!(f.tilt().0.abs() < 0.05, "{}", f.tilt().0);
        }
    }

    #[test]
    fn shift_holds_its_step_at_a_boundary() {
        let mut t = Tilt::new(None);
        let first = t.shift((2.45 / 8.0, -2.45 / 4.0), (8, 4));
        for i in 0..200 {
            let px = if i % 2 == 0 { 2.55 } else { 2.45 };
            assert_eq!(t.shift((px / 8.0, -px / 4.0), (8, 4)), first);
        }
    }

    #[test]
    fn shift_ramp_visits_every_pixel_once() {
        let mut t = Tilt::new(None);
        let mut seen = vec![0];
        for i in 0..=4000 {
            let (x, y) = t.shift((i as f32 / 4000.0, -(i as f32) / 4000.0), (8, 4));
            assert!(y <= 0 && y >= -4);
            if *seen.last().unwrap() != x {
                seen.push(x);
            }
        }
        assert_eq!(seen, (0..=8).collect::<Vec<_>>());
        assert_eq!(t.shift((1.0, -1.0), (8, 4)), (8, -4));
        // Past the range it stays at the reach.
        assert_eq!(t.shift((3.0, -3.0), (8, 4)), (8, -4));
    }

    fn deck_report(accel: [i16; 3], gyro: [i16; 3]) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[..3].copy_from_slice(&[0x01, 0x00, 0x09]);
        for (i, v) in accel.iter().chain(gyro.iter()).enumerate() {
            b[24 + i * 2..26 + i * 2].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn deck_report_decodes_on_sdl_axes() {
        // 16384 raw = 1000 degrees/s; -8192 = -500; 4096 = 250.
        let b = deck_report([0, -16384, 8192], [16384, -8192, 4096]);
        let m = parse_deck_report(&b).unwrap();
        let want = [1000.0 * DEG, 250.0 * DEG, 500.0 * DEG];
        for i in 0..3 {
            assert!((m.gyro[i] - want[i]).abs() < 1e-3, "{:?}", m.gyro);
        }
        assert_eq!(m.accel, [0.0, 0.5, 1.0]);

        let mut other = b;
        other[2] = 0x04;
        assert_eq!(parse_deck_report(&other), None);
        assert_eq!(parse_deck_report(&b[..40]), None);
        assert_eq!(parse_deck_report(&[]), None);
    }

    #[test]
    fn uevent_names_the_deck() {
        let deck = "DRIVER=hid-steam\nHID_ID=0003:000028DE:00001205\nHID_NAME=Valve Software Steam Controller\n";
        assert!(is_deck_uevent(deck));
        assert!(!is_deck_uevent("HID_ID=0003:000028DE:00001102\n"));
        assert!(!is_deck_uevent("HID_NAME=0003:000028DE:00001205\n"));
    }

    #[cfg(unix)]
    #[test]
    fn sensor_picks_the_interface_that_reports() {
        let root = std::env::temp_dir().join(format!("ebobo-tilt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (sys, dev) = (root.join("sys"), root.join("dev"));
        std::fs::create_dir_all(&dev).unwrap();
        let add = |name: &str, id: &str, data: &[u8]| {
            let d = sys.join(name).join("device");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("uevent"), format!("HID_ID={id}\n")).unwrap();
            std::fs::write(dev.join(name), data).unwrap();
        };
        let state = deck_report([0, 0, 16384], [0, 0, 16384]);
        let mut other = state;
        other[2] = 0x04;
        // A keyboard, a Deck interface with other traffic, and the one with state reports.
        add("hidraw0", "0003:0000046D:0000C52B", &state);
        add("hidraw1", "0003:000028DE:00001205", &other);
        add("hidraw2", "0003:000028DE:00001205", &[other, state, state].concat());

        let mut s = deck_hid::Sensor::at(&sys, &dev);
        let mut got = Vec::new();
        assert!(s.poll(0.016, &mut |m: &Motion| got.push(*m)));
        assert_eq!(got.len(), 2);
        assert!((got[0].gyro[1] - 1000.0 * DEG).abs() < 1e-3, "{:?}", got[0]);
        // Silent for too long: dropped, then looked for again.
        let mut none = |_: &Motion| panic!("no reports are left");
        assert!(s.poll(1.5, &mut none));
        assert!(!s.poll(1.5, &mut none));
        let _ = std::fs::remove_dir_all(&root);
    }
}
