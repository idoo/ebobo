//! Keyboard and gamepads → NES pads, menu navigation and hotkeys. Owner: platform slice.

use std::path::PathBuf;
use std::time::Duration;

use ebobo_core::pad;
use sdl2::controller::{Axis, Button, GameController};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Mod, Scancode};
use sdl2::mouse::MouseButton;
use sdl2::{GameControllerSubsystem, JoystickSubsystem};

/// Menu navigation, edge-triggered. Directions auto-repeat while held (400 ms delay, then
/// every 80 ms). Keyboard: arrows, Enter = confirm, Esc/Backspace = back. Gamepad: D-pad or
/// left stick, South (A on Xbox/Deck, Cross on PlayStation) = confirm, East (B / Circle) = back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nav {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub confirm: bool,
    pub back: bool,
}

/// A spot in the window, before mapping to the UI canvas (`Video::to_canvas`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WindowPos {
    /// Mouse, in window points.
    Point(f32, f32),
    /// Touchscreen finger, normalised 0..1 of the window.
    Touch(f32, f32),
}

/// Mouse (left button) or first finger, in 256x240 canvas pixels; `None` = outside the
/// picture (letterbox bars).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEvent {
    Down(Option<(i32, i32)>),
    /// Movement while pressed.
    Move(Option<(i32, i32)>),
    Up(Option<(i32, i32)>),
    /// Mouse wheel, positive = away from the user (scroll up).
    Wheel(i32),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputFrame {
    /// NES pad bits per seat: [0] = keyboard merged with the gamepad in seat 1, [1..4] = the
    /// gamepads in seats 2-4. A gamepad without a seat feeds nothing; it takes the lowest free
    /// seat the first time it presses a game button while seating is on (`set_seating`).
    /// Opposite face buttons do the same: South and North = NES A, West and East = NES B
    /// (Nintendo pads: East and West = A, South and North = B, so the printed A and B match).
    /// Left shoulder = turbo B, right shoulder = turbo A, Back/Share = Select,
    /// Start/Options = Start, D-pad and left stick = D-pad. Keyboard: arrows, X = A, Z = B,
    /// Enter = Start, Right Shift = Select, S = turbo A, A = turbo B. Turbo toggles every
    /// 2 frames of `poll`.
    pub pads: [u8; 4],
    /// A keyboard key went down (auto-repeat not counted).
    pub key_used: bool,
    pub nav: Nav,
    /// Text typed since the last poll (only while text input is enabled).
    pub text: String,
    /// Backspace pressed (text fields).
    pub backspace: bool,
    /// Cmd+V / Ctrl+V clipboard text.
    pub paste: Option<String>,
    /// Open/close the menu: Esc, a click of the right stick, gamepad Guide/PS button,
    /// PlayStation touchpad click, or Back+Start held for 0.6 s.
    pub menu: bool,
    /// 0.0-1.0 progress of the Back+Start hold (for an on-screen hint), 0 when not held.
    pub menu_hold: f32,
    /// F11, Alt+Enter, Cmd+Ctrl+F.
    pub fullscreen: bool,
    /// F5 / F7 quick save and load (solo only).
    pub save_state: bool,
    pub load_state: bool,
    /// Rewind held: Backspace (only while no text field has focus) or Left Trigger / L2.
    pub rewind: bool,
    /// Fast-forward held: Tab or Right Trigger / R2.
    pub fast_forward: bool,
    /// Screenshot pressed: F12, gamepad Back+North (Share+Triangle on PlayStation) or Misc1
    /// (Xbox Share, Switch Capture).
    pub screenshot: bool,
    /// Window closed or Cmd+Q.
    pub quit: bool,
    /// A file dropped onto the window.
    pub dropped_file: Option<PathBuf>,
    /// Connected gamepads.
    pub gamepads: usize,
    /// Gamepads that hold a seat.
    pub seated: usize,
    /// Kind of the gamepad used last, for button labels in hints; `None` after a keyboard key
    /// or with no gamepad.
    pub hint_kind: Option<PadKind>,
    /// Hot-plug notices since the last poll (UI shows a toast).
    pub pad_events: Vec<PadEvent>,
    /// Pointer events since the last poll, in order (touch-synthesised mouse events dropped).
    pub pointer: Vec<PointerEvent>,
    /// The real mouse moved or clicked (show the cursor).
    pub mouse_moved: bool,
    /// Gamepad buttons the on-screen keyboard uses.
    pub pad: PadPress,
}

/// Gamepad presses this frame beyond `Nav`, for the on-screen keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadPress {
    /// Any gamepad button went down.
    pub used: bool,
    /// Y / Triangle.
    pub shift: bool,
    /// X / Square.
    pub layout: bool,
    pub start: bool,
}

/// Controller family, for button labels in UI hints (Cross/Circle vs A/B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadKind {
    Xbox,
    PlayStation,
    Nintendo,
    SteamDeck,
    Generic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadInfo {
    pub name: String,
    pub kind: PadKind,
    /// Seat: index into `InputFrame::pads` this controller feeds; `None` = no seat yet.
    pub slot: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PadEvent {
    /// Plugged in; `slot` is `Some` only when it got its old seat back.
    Connected(PadInfo),
    /// Took a seat by pressing a game button; `slot` is `Some`.
    Seated(PadInfo),
    Disconnected(PadInfo),
}

// ---------------------------------------------------------------------------------------------
// Pure parts (no SDL state): mapping, timers, seats.

const STICK_THRESHOLD: f32 = 0.5;
/// Analog triggers count as pressed past this (0.0-1.0).
const TRIGGER_THRESHOLD: f32 = 0.5;
const REPEAT_DELAY: Duration = Duration::from_millis(400);
const REPEAT_EVERY: Duration = Duration::from_millis(80);
const HOLD_TIME: Duration = Duration::from_millis(600);
/// `SDL_TOUCH_MOUSEID`: `which` of mouse events SDL synthesises from touches.
const TOUCH_MOUSE_ID: u32 = u32::MAX;

/// Slot colours: 1 blue, 2 red, 3 green, 4 pink.
const SLOT_RGB: [(u8, u8, u8); 4] = [
    (0x3B, 0x7B, 0xE4),
    (0xE4, 0x3B, 0x2C),
    (0x4C, 0xC6, 0x6A),
    (0xE4, 0x6A, 0xB4),
];
/// Light of a gamepad without a seat: dim white.
const NO_SEAT_RGB: (u8, u8, u8) = (0x20, 0x20, 0x20);

/// Positional button state of one gamepad. Stick axes are -1.0..1.0 (negative = up/left).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PadButtons {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    south: bool,
    east: bool,
    west: bool,
    north: bool,
    back: bool,
    start: bool,
    /// Left and right shoulder (L1/LB, R1/RB).
    l1: bool,
    r1: bool,
    lx: f32,
    ly: f32,
    /// Analog triggers, 0.0 (released) - 1.0.
    lt: f32,
    rt: f32,
}

impl PadButtons {
    /// D-pad merged with the left stick: (up, down, left, right).
    fn dirs(&self) -> (bool, bool, bool, bool) {
        (
            self.up || self.ly <= -STICK_THRESHOLD,
            self.down || self.ly >= STICK_THRESHOLD,
            self.left || self.lx <= -STICK_THRESHOLD,
            self.right || self.lx >= STICK_THRESHOLD,
        )
    }

    /// Anything held: a button, a direction or a trigger.
    fn active(&self) -> bool {
        let (up, down, left, right) = self.dirs();
        up || down
            || left
            || right
            || self.south
            || self.east
            || self.west
            || self.north
            || self.back
            || self.start
            || self.l1
            || self.r1
            || self.lt >= TRIGGER_THRESHOLD
            || self.rt >= TRIGGER_THRESHOLD
    }
}

/// Emulator hotkeys on a gamepad: (rewind held, fast-forward held, screenshot combo held).
/// Left Trigger / L2 = rewind, Right Trigger / R2 = fast-forward, Back+North = screenshot.
fn pad_hotkeys(b: &PadButtons) -> (bool, bool, bool) {
    (b.lt >= TRIGGER_THRESHOLD, b.rt >= TRIGGER_THRESHOLD, b.back && b.north)
}

/// The game does not see the buttons of the Back+North screenshot combo while both are held.
fn mask_shot_combo(b: &PadButtons) -> PadButtons {
    if b.back && b.north {
        PadButtons { back: false, north: false, ..*b }
    } else {
        *b
    }
}

/// Positional gamepad → NES bits. Opposite face buttons do the same, so any two neighbouring
/// ones are B + A: South and North = A, West and East = B; on Nintendo pads East and West = A,
/// South and North = B (the printed A and B). Shoulders are turbo B (left) and turbo A (right);
/// `turbo_on` = current turbo phase.
fn pad_bits(b: &PadButtons, kind: PadKind, turbo_on: bool) -> u8 {
    let (nes_a, nes_b) = match kind {
        PadKind::Nintendo => (b.east || b.west, b.south || b.north),
        _ => (b.south || b.north, b.west || b.east),
    };
    let (up, down, left, right) = b.dirs();
    let mut bits = 0;
    let mut set = |on: bool, bit: u8| {
        if on {
            bits |= bit;
        }
    };
    set(nes_b, pad::B);
    set(nes_a, pad::A);
    set(b.l1 && turbo_on, pad::B);
    set(b.r1 && turbo_on, pad::A);
    set(b.back, pad::SELECT);
    set(b.start, pad::START);
    set(up, pad::UP);
    set(down, pad::DOWN);
    set(left, pad::LEFT);
    set(right, pad::RIGHT);
    bits
}

/// Keyboard → NES bits: arrows, X = A, Z = B, Enter = Start, Right Shift = Select,
/// S = turbo A, A = turbo B.
fn keyboard_bits(pressed: impl Fn(Scancode) -> bool, turbo_on: bool) -> u8 {
    let mut bits = 0;
    let map = [
        (Scancode::X, pad::A),
        (Scancode::Z, pad::B),
        (Scancode::Return, pad::START),
        (Scancode::RShift, pad::SELECT),
        (Scancode::Up, pad::UP),
        (Scancode::Down, pad::DOWN),
        (Scancode::Left, pad::LEFT),
        (Scancode::Right, pad::RIGHT),
    ];
    for (sc, bit) in map {
        if pressed(sc) {
            bits |= bit;
        }
    }
    if turbo_on {
        if pressed(Scancode::S) {
            bits |= pad::A;
        }
        if pressed(Scancode::A) {
            bits |= pad::B;
        }
    }
    bits
}

/// Turbo phase: on for 2 polls, off for 2 polls.
#[derive(Default)]
struct Turbo {
    n: u32,
}

impl Turbo {
    fn tick(&mut self) -> bool {
        let on = (self.n / 2) % 2 == 0;
        self.n = self.n.wrapping_add(1);
        on
    }
}

/// Edge + auto-repeat for one held direction.
#[derive(Default)]
struct Repeat {
    held: bool,
    t: Duration,
    next: Duration,
}

impl Repeat {
    fn update(&mut self, down: bool, dt: Duration) -> bool {
        if !down {
            self.held = false;
            return false;
        }
        if !self.held {
            self.held = true;
            self.t = Duration::ZERO;
            self.next = REPEAT_DELAY;
            return true;
        }
        self.t += dt;
        if self.t >= self.next {
            while self.next <= self.t {
                self.next += REPEAT_EVERY;
            }
            return true;
        }
        false
    }
}

/// Back+Start hold timer: fires once after 0.6 s, reports progress while held.
#[derive(Default)]
struct Hold {
    t: Duration,
    fired: bool,
}

impl Hold {
    /// Returns (fired now, progress 0..1; 0 when not held or already fired).
    fn update(&mut self, held: bool, dt: Duration) -> (bool, f32) {
        if !held {
            self.t = Duration::ZERO;
            self.fired = false;
            return (false, 0.0);
        }
        if self.fired {
            return (false, 0.0);
        }
        self.t += dt;
        if self.t >= HOLD_TIME {
            self.fired = true;
            return (true, 0.0);
        }
        (false, self.t.as_secs_f32() / HOLD_TIME.as_secs_f32())
    }
}

/// While Back+Start are both held, the game sees neither (no accidental pause/select).
fn mask_combo(bits: u8) -> u8 {
    if bits & (pad::START | pad::SELECT) == pad::START | pad::SELECT {
        bits & !(pad::START | pad::SELECT)
    } else {
        bits
    }
}

/// Player seats. A pad takes a seat with `sit` (its first game button): the free seat it held
/// before (same key = GUID + name), else the lowest free one. A pad that drops out keeps its
/// key on the seat, so `rejoin` gives the seat back when it reconnects.
#[derive(Default)]
struct Seats {
    owner: [Option<u32>; 4],
    last_key: [Option<String>; 4],
}

impl Seats {
    fn free_with_key(&self, key: &str) -> Option<usize> {
        (0..4).find(|&s| self.owner[s].is_none() && self.last_key[s].as_deref() == Some(key))
    }

    /// The free seat this key held last, if any.
    fn rejoin(&mut self, id: u32, key: &str) -> Option<usize> {
        let seat = self.free_with_key(key)?;
        self.owner[seat] = Some(id);
        Some(seat)
    }

    /// The free seat this key held last, else the lowest free seat; `None` when all are taken.
    fn sit(&mut self, id: u32, key: &str) -> Option<usize> {
        let seat = self
            .free_with_key(key)
            .or_else(|| self.owner.iter().position(Option::is_none))?;
        self.owner[seat] = Some(id);
        self.last_key[seat] = Some(key.to_owned());
        Some(seat)
    }

    /// Frees the seat of `id`; its key stays for `rejoin`.
    fn release(&mut self, id: u32) -> Option<usize> {
        let seat = self.owner.iter().position(|o| *o == Some(id))?;
        self.owner[seat] = None;
        Some(seat)
    }

    /// Frees every seat and forgets every key.
    fn clear(&mut self) {
        *self = Seats::default();
    }
}

fn kind_from(name: &str, ty: Option<PadKind>, on_deck: bool) -> PadKind {
    let steam_virtual = name.contains("Steam Virtual") || name.contains("Steam Controller");
    if name.contains("Steam Deck") || (on_deck && steam_virtual) {
        return PadKind::SteamDeck;
    }
    ty.unwrap_or(PadKind::Generic)
}

// ---------------------------------------------------------------------------------------------
// SDL glue.

struct Pad {
    ctrl: GameController,
    info: PadInfo,
    hold: Hold,
    /// Screenshot combo held at the previous poll (edge detection).
    shot: bool,
    /// GUID + name: tells a reconnecting pad from a new one.
    key: String,
}

impl Pad {
    fn id(&self) -> u32 {
        self.ctrl.instance_id()
    }

    fn buttons(&self) -> PadButtons {
        let c = &self.ctrl;
        let axis = |a: Axis| c.axis(a) as f32 / 32767.0;
        PadButtons {
            up: c.button(Button::DPadUp),
            down: c.button(Button::DPadDown),
            left: c.button(Button::DPadLeft),
            right: c.button(Button::DPadRight),
            south: c.button(Button::A),
            east: c.button(Button::B),
            west: c.button(Button::X),
            north: c.button(Button::Y),
            back: c.button(Button::Back),
            start: c.button(Button::Start),
            l1: c.button(Button::LeftShoulder),
            r1: c.button(Button::RightShoulder),
            lx: axis(Axis::LeftX),
            ly: axis(Axis::LeftY),
            lt: axis(Axis::TriggerLeft),
            rt: axis(Axis::TriggerRight),
        }
    }
}

fn sdl_kind(ctrl: &GameController) -> Option<PadKind> {
    use sdl2::sys::SDL_GameControllerType as T;
    // sdl2 0.38 hides the raw pointer; look it up by instance id (owned by `ctrl`).
    let raw = unsafe { sdl2::sys::SDL_GameControllerFromInstanceID(ctrl.instance_id() as i32) };
    if raw.is_null() {
        return None;
    }
    let ty = unsafe { sdl2::sys::SDL_GameControllerGetType(raw) };
    match ty {
        T::SDL_CONTROLLER_TYPE_XBOX360 | T::SDL_CONTROLLER_TYPE_XBOXONE => Some(PadKind::Xbox),
        T::SDL_CONTROLLER_TYPE_PS3 | T::SDL_CONTROLLER_TYPE_PS4 | T::SDL_CONTROLLER_TYPE_PS5 => {
            Some(PadKind::PlayStation)
        }
        T::SDL_CONTROLLER_TYPE_NINTENDO_SWITCH_PRO => Some(PadKind::Nintendo),
        _ => None,
    }
}

/// Light bar / player LEDs of a pad: its seat colour and number, or dim white and no number
/// without a seat.
fn show_seat(ctrl: &mut GameController, slot: Option<usize>) {
    let (r, g, b) = slot.map_or(NO_SEAT_RGB, |s| SLOT_RGB[s]);
    let _ = ctrl.set_led(r, g, b);
    let raw = unsafe { sdl2::sys::SDL_GameControllerFromInstanceID(ctrl.instance_id() as i32) };
    if !raw.is_null() {
        unsafe { sdl2::sys::SDL_GameControllerSetPlayerIndex(raw, slot.map_or(-1, |s| s as i32)) };
    }
}

/// PlayStation, Xbox and Switch pads work out of the box, USB or Bluetooth, hot-plugged at any
/// time (SDL's HIDAPI PS4/PS5 drivers are enabled via hints before the controller subsystem
/// starts). One fixed layout, see `InputFrame::pads`. A connected pad has no seat: it takes the
/// lowest free one with its first game button while seating is on, so the pad in the player's
/// hands is player 1 and idle pads (the Deck's own controls next to an external pad) take
/// none. `reset_seats` empties the seats when a game starts; a pad that reconnects during the
/// game gets its seat back at once. The light bar / player LEDs show the seat (1 blue, 2 red,
/// 3 green, 4 pink; dim white without one). Hotkeys and menu navigation work on every pad.
pub struct Input {
    gc: GameControllerSubsystem,
    joy: JoystickSubsystem,
    clipboard: sdl2::clipboard::ClipboardUtil,
    text_input: sdl2::keyboard::TextInputUtil,
    text_on: bool,
    pads: Vec<Pad>,
    seats: Seats,
    /// Pads may take seats (a game runs, menu hidden).
    seating: bool,
    /// Gamepad used last (instance id, kind); `None` after a keyboard key.
    hint: Option<(u32, PadKind)>,
    turbo: Turbo,
    repeat: [Repeat; 4],
    events: Vec<PadEvent>,
    /// The finger being tracked (`touch_id`, `finger_id`); other fingers are ignored.
    finger: Option<(i64, i64)>,
}

impl Input {
    /// Opens every connected game controller and keeps tracking hot-plugs. Sets the SDL hints
    /// (HIDAPI PS4/PS5 on, background events on) itself, so it must be created before the
    /// game-controller subsystem is first used.
    pub fn new(sdl: &sdl2::Sdl) -> Result<Input, String> {
        for (k, v) in [
            ("SDL_JOYSTICK_HIDAPI_PS4", "1"),
            ("SDL_JOYSTICK_HIDAPI_PS5", "1"),
            ("SDL_JOYSTICK_HIDAPI_PS4_RUMBLE", "0"),
            ("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1"),
            ("SDL_GAMECONTROLLER_USE_BUTTON_LABELS", "0"),
        ] {
            sdl2::hint::set(k, v);
        }
        // On the Deck SDL_StartTextInput would open Steam's keyboard, whose keys never reach
        // us; the built-in keyboard (osk.rs) takes its place. Physical keyboards still type.
        if crate::config::is_steam_deck() {
            sdl2::hint::set("SDL_ENABLE_SCREEN_KEYBOARD", "0");
        }
        let video = sdl.video()?;
        let joy = sdl.joystick()?;
        let gc = sdl.game_controller()?;
        let text_input = video.text_input();
        // SDL2 starts with text input on; only text fields enable it.
        text_input.stop();
        let mut input = Input {
            gc,
            joy,
            clipboard: video.clipboard(),
            text_input,
            text_on: false,
            pads: Vec::new(),
            seats: Seats::default(),
            seating: false,
            hint: None,
            turbo: Turbo::default(),
            repeat: Default::default(),
            events: Vec::new(),
            finger: None,
        };
        let n = input.joy.num_joysticks().unwrap_or(0);
        for i in 0..n {
            input.open(i);
        }
        // Start-up pads are not hot-plug news.
        input.events.clear();
        input.hint = input.pads.first().map(|p| (p.id(), p.info.kind));
        Ok(input)
    }

    fn open(&mut self, index: u32) {
        if !self.gc.is_game_controller(index) {
            return;
        }
        let Ok(ctrl) = self.gc.open(index) else { return };
        let id = ctrl.instance_id();
        if self.pads.iter().any(|p| p.id() == id) {
            return;
        }
        let name = ctrl.name();
        let guid = self
            .joy
            .device_guid(index)
            .map(|g| g.string())
            .unwrap_or_default();
        let key = format!("{guid}|{name}");
        let slot = self.seats.rejoin(id, &key);
        let kind = kind_from(&name, sdl_kind(&ctrl), crate::config::is_steam_deck());
        let mut ctrl = ctrl;
        show_seat(&mut ctrl, slot);
        let info = PadInfo { name, kind, slot };
        self.events.push(PadEvent::Connected(info.clone()));
        self.pads.push(Pad {
            ctrl,
            info,
            key,
            hold: Hold::default(),
            shot: false,
        });
    }

    fn close(&mut self, id: u32) {
        if let Some(i) = self.pads.iter().position(|p| p.id() == id) {
            let pad = self.pads.remove(i);
            self.seats.release(id);
            if self.hint.is_some_and(|(shown, _)| shown == id) {
                self.hint = self.pads.first().map(|p| (p.id(), p.info.kind));
            }
            self.events.push(PadEvent::Disconnected(pad.info));
        }
    }

    /// Lets gamepads take seats: on while a game runs with the menu hidden.
    pub fn set_seating(&mut self, on: bool) {
        self.seating = on;
    }

    /// Every gamepad loses its seat (a game starts); the next one to press a button is player 1.
    pub fn reset_seats(&mut self) {
        self.seats.clear();
        for p in &mut self.pads {
            p.info.slot = None;
            show_seat(&mut p.ctrl, None);
        }
    }

    /// Drains SDL events. `dt` = time since the previous call (auto-repeat, hold timers);
    /// `to_canvas` maps mouse and touch positions to UI canvas pixels.
    pub fn poll(
        &mut self,
        events: &mut sdl2::EventPump,
        dt: Duration,
        to_canvas: impl Fn(WindowPos) -> Option<(i32, i32)>,
    ) -> InputFrame {
        let mut f = InputFrame::default();
        for ev in events.poll_iter() {
            match ev {
                Event::Quit { .. } => f.quit = true,
                Event::Window {
                    win_event: WindowEvent::Close,
                    ..
                } => f.quit = true,
                Event::DropFile { filename, .. } => f.dropped_file = Some(PathBuf::from(filename)),
                Event::TextInput { text, .. } if self.text_on => f.text.push_str(&text),
                Event::ControllerDeviceAdded { which, .. } => self.open(which),
                Event::ControllerDeviceRemoved { which, .. } => self.close(which),
                Event::ControllerButtonDown { which, button, .. } => {
                    f.pad.used = true;
                    match button {
                        Button::A => f.nav.confirm = true,
                        Button::B => f.nav.back = true,
                        Button::Y => f.pad.shift = true,
                        Button::X => f.pad.layout = true,
                        Button::Start => f.pad.start = true,
                        Button::Guide => f.menu = true,
                        Button::Touchpad => f.menu = true,
                        // Free on every pad and never pressed by accident: the left stick
                        // moves, the right one does nothing.
                        Button::RightStick => f.menu = true,
                        // Misc1 is Share on Xbox and Capture on Switch pads, but the
                        // microphone mute on a DualSense.
                        Button::Misc1 => {
                            let mute = self.pads.iter().any(|p| p.id() == which && p.info.kind == PadKind::PlayStation);
                            f.screenshot |= !mute;
                        }
                        _ => {}
                    }
                }
                Event::KeyDown {
                    scancode: Some(sc),
                    keymod,
                    repeat,
                    ..
                } => {
                    let cmd = keymod.intersects(Mod::LGUIMOD | Mod::RGUIMOD);
                    let ctrl = keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD);
                    let alt = keymod.intersects(Mod::LALTMOD | Mod::RALTMOD);
                    if !repeat {
                        f.key_used = true;
                        self.hint = None;
                    }
                    match sc {
                        Scancode::F11 if !repeat => f.fullscreen = true,
                        Scancode::Return | Scancode::KpEnter if alt => {
                            if !repeat {
                                f.fullscreen = true;
                            }
                        }
                        Scancode::F if cmd && ctrl && !repeat => f.fullscreen = true,
                        Scancode::Q if cmd => f.quit = true,
                        Scancode::V if cmd || ctrl => {
                            f.paste = self
                                .clipboard
                                .clipboard_text()
                                .ok()
                                .filter(|t| !t.is_empty());
                        }
                        Scancode::F5 if !repeat => f.save_state = true,
                        Scancode::F7 if !repeat => f.load_state = true,
                        Scancode::F12 if !repeat => f.screenshot = true,
                        Scancode::Escape if !repeat => {
                            f.menu = true;
                            f.nav.back = true;
                        }
                        Scancode::Backspace => {
                            f.backspace = true;
                            if !repeat {
                                f.nav.back = true;
                            }
                        }
                        Scancode::Return | Scancode::KpEnter if !repeat => f.nav.confirm = true,
                        _ => {}
                    }
                }
                // Touches also arrive as synthesised mouse events (`which` = SDL_TOUCH_MOUSEID);
                // the finger events below already count them.
                Event::MouseButtonDown { which, .. } | Event::MouseButtonUp { which, .. } | Event::MouseMotion { which, .. }
                    if which == TOUCH_MOUSE_ID => {}
                Event::MouseButtonDown { mouse_btn: MouseButton::Left, x, y, .. } => {
                    f.mouse_moved = true;
                    f.pointer.push(PointerEvent::Down(to_canvas(WindowPos::Point(x as f32, y as f32))));
                }
                Event::MouseButtonUp { mouse_btn: MouseButton::Left, x, y, .. } => {
                    f.mouse_moved = true;
                    f.pointer.push(PointerEvent::Up(to_canvas(WindowPos::Point(x as f32, y as f32))));
                }
                Event::MouseMotion { mousestate, x, y, .. } => {
                    f.mouse_moved = true;
                    if mousestate.left() {
                        f.pointer.push(PointerEvent::Move(to_canvas(WindowPos::Point(x as f32, y as f32))));
                    }
                }
                Event::MouseWheel { which, y, direction, .. } if which != TOUCH_MOUSE_ID && y != 0 => {
                    let y = if direction == sdl2::mouse::MouseWheelDirection::Flipped { -y } else { y };
                    f.pointer.push(PointerEvent::Wheel(y));
                }
                Event::FingerDown { touch_id, finger_id, x, y, .. } if self.finger.is_none() => {
                    self.finger = Some((touch_id, finger_id));
                    f.pointer.push(PointerEvent::Down(to_canvas(WindowPos::Touch(x, y))));
                }
                Event::FingerMotion { touch_id, finger_id, x, y, .. } if self.finger == Some((touch_id, finger_id)) => {
                    f.pointer.push(PointerEvent::Move(to_canvas(WindowPos::Touch(x, y))));
                }
                Event::FingerUp { touch_id, finger_id, x, y, .. } if self.finger == Some((touch_id, finger_id)) => {
                    self.finger = None;
                    f.pointer.push(PointerEvent::Up(to_canvas(WindowPos::Touch(x, y))));
                }
                _ => {}
            }
        }

        let turbo_on = self.turbo.tick();
        let ks = events.keyboard_state();
        f.pads[0] = keyboard_bits(|sc| ks.is_scancode_pressed(sc), turbo_on);
        f.rewind = ks.is_scancode_pressed(Scancode::Backspace) && !self.text_on;
        f.fast_forward = ks.is_scancode_pressed(Scancode::Tab);
        let mut dirs = [
            ks.is_scancode_pressed(Scancode::Up),
            ks.is_scancode_pressed(Scancode::Down),
            ks.is_scancode_pressed(Scancode::Left),
            ks.is_scancode_pressed(Scancode::Right),
        ];

        for p in &mut self.pads {
            let b = p.buttons();
            if b.active() {
                self.hint = Some((p.ctrl.instance_id(), p.info.kind));
            }
            let (u, d, l, r) = b.dirs();
            for (acc, v) in dirs.iter_mut().zip([u, d, l, r]) {
                *acc |= v;
            }
            let (fired, progress) = p.hold.update(b.back && b.start, dt);
            f.menu |= fired;
            f.menu_hold = f.menu_hold.max(progress);
            let (rewind, fast_forward, shot) = pad_hotkeys(&b);
            f.rewind |= rewind;
            f.fast_forward |= fast_forward;
            f.screenshot |= shot && !p.shot;
            p.shot = shot;
            let game = mask_shot_combo(&b);
            if p.info.slot.is_none() && self.seating && mask_combo(pad_bits(&game, p.info.kind, true)) != 0 {
                if let Some(slot) = self.seats.sit(p.ctrl.instance_id(), &p.key) {
                    p.info.slot = Some(slot);
                    show_seat(&mut p.ctrl, Some(slot));
                    self.events.push(PadEvent::Seated(p.info.clone()));
                }
            }
            if let Some(slot) = p.info.slot {
                f.pads[slot] |= mask_combo(pad_bits(&game, p.info.kind, turbo_on));
            }
        }

        let [up, down, left, right] = dirs;
        f.nav.up = self.repeat[0].update(up, dt);
        f.nav.down = self.repeat[1].update(down, dt);
        f.nav.left = self.repeat[2].update(left, dt);
        f.nav.right = self.repeat[3].update(right, dt);

        f.gamepads = self.pads.len();
        f.seated = self.pads.iter().filter(|p| p.info.slot.is_some()).count();
        f.hint_kind = self.hint.map(|(_, kind)| kind);
        f.pad_events = std::mem::take(&mut self.events);
        f
    }

    /// Enables SDL text input while a text field has focus (shows the on-screen keyboard where
    /// the platform has one).
    pub fn set_text_input(&mut self, on: bool) {
        if on == self.text_on {
            return;
        }
        self.text_on = on;
        if on {
            self.text_input.start();
        } else {
            self.text_input.stop();
        }
    }

    /// Connected controllers: seated ones in seat order, then the rest in connect order.
    pub fn pads(&self) -> Vec<PadInfo> {
        let mut v: Vec<PadInfo> = self.pads.iter().map(|p| p.info.clone()).collect();
        v.sort_by_key(|p| p.slot.unwrap_or(usize::MAX));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn input_positional_mapping() {
        let one = |f: fn(&mut PadButtons)| {
            let mut b = PadButtons::default();
            f(&mut b);
            b
        };
        let (south, east) = (one(|b| b.south = true), one(|b| b.east = true));
        let (west, north) = (one(|b| b.west = true), one(|b| b.north = true));

        for turbo_on in [true, false] {
            // Bottom and top = A, left and right = B.
            for kind in [PadKind::Xbox, PadKind::PlayStation, PadKind::SteamDeck, PadKind::Generic] {
                assert_eq!(pad_bits(&south, kind, turbo_on), pad::A);
                assert_eq!(pad_bits(&north, kind, turbo_on), pad::A);
                assert_eq!(pad_bits(&west, kind, turbo_on), pad::B);
                assert_eq!(pad_bits(&east, kind, turbo_on), pad::B);
            }
            // Nintendo: the printed A (right) and Y (left) = A, B (bottom) and X (top) = B.
            assert_eq!(pad_bits(&east, PadKind::Nintendo, turbo_on), pad::A);
            assert_eq!(pad_bits(&west, PadKind::Nintendo, turbo_on), pad::A);
            assert_eq!(pad_bits(&south, PadKind::Nintendo, turbo_on), pad::B);
            assert_eq!(pad_bits(&north, PadKind::Nintendo, turbo_on), pad::B);
        }

        // Shoulders are turbo: pressed only on the turbo phase.
        let (l1, r1) = (one(|b| b.l1 = true), one(|b| b.r1 = true));
        for kind in [PadKind::Xbox, PadKind::Nintendo] {
            assert_eq!(pad_bits(&l1, kind, true), pad::B);
            assert_eq!(pad_bits(&l1, kind, false), 0);
            assert_eq!(pad_bits(&r1, kind, true), pad::A);
            assert_eq!(pad_bits(&r1, kind, false), 0);

            let b = PadButtons { back: true, start: true, ..Default::default() };
            assert_eq!(pad_bits(&b, kind, false), pad::SELECT | pad::START);
            let b = PadButtons { lx: -0.6, ly: 0.4, up: true, ..Default::default() };
            assert_eq!(pad_bits(&b, kind, false), pad::LEFT | pad::UP);
        }

        assert!(!PadButtons::default().active());
        assert!(l1.active() && south.active());
        assert!(PadButtons { lx: 0.6, ..Default::default() }.active());
        assert!(PadButtons { rt: 0.9, ..Default::default() }.active());
    }

    #[test]
    fn input_trigger_hotkeys() {
        assert_eq!(pad_hotkeys(&PadButtons::default()), (false, false, false));
        let b = PadButtons { lt: 0.8, ..Default::default() };
        assert_eq!(pad_hotkeys(&b), (true, false, false));
        let b = PadButtons { rt: 1.0, lt: 0.3, ..Default::default() };
        assert_eq!(pad_hotkeys(&b), (false, true, false));
        let b = PadButtons { back: true, north: true, south: true, ..Default::default() };
        assert_eq!(pad_hotkeys(&b), (false, false, true));
        // The combo is hidden from the game; other buttons pass through.
        let kind = PadKind::Xbox;
        let b = PadButtons { back: true, north: true, west: true, ..Default::default() };
        assert_eq!(pad_bits(&mask_shot_combo(&b), kind, true), pad::B);
        // Without Back, North is A.
        let b = PadButtons { north: true, west: true, ..Default::default() };
        assert_eq!(pad_bits(&mask_shot_combo(&b), kind, true), pad::A | pad::B);
        let b = PadButtons { back: true, ..Default::default() };
        assert_eq!(pad_bits(&mask_shot_combo(&b), kind, true), pad::SELECT);
        // Triggers never reach the NES pad.
        let b = PadButtons { lt: 1.0, rt: 1.0, ..Default::default() };
        assert_eq!(pad_bits(&b, kind, true), 0);
    }

    #[test]
    fn input_keyboard_map() {
        let only = |k: Scancode| move |sc: Scancode| sc == k;
        assert_eq!(keyboard_bits(only(Scancode::X), false), pad::A);
        assert_eq!(keyboard_bits(only(Scancode::Z), false), pad::B);
        assert_eq!(keyboard_bits(only(Scancode::Return), false), pad::START);
        assert_eq!(keyboard_bits(only(Scancode::RShift), false), pad::SELECT);
        assert_eq!(keyboard_bits(only(Scancode::Up), false), pad::UP);
        assert_eq!(keyboard_bits(only(Scancode::Down), false), pad::DOWN);
        assert_eq!(keyboard_bits(only(Scancode::Left), false), pad::LEFT);
        assert_eq!(keyboard_bits(only(Scancode::Right), false), pad::RIGHT);
        assert_eq!(keyboard_bits(only(Scancode::S), true), pad::A);
        assert_eq!(keyboard_bits(only(Scancode::A), true), pad::B);
        assert_eq!(keyboard_bits(only(Scancode::S), false), 0);
    }

    #[test]
    fn input_turbo_toggles_every_two_polls() {
        let mut t = Turbo::default();
        let seq: Vec<bool> = (0..8).map(|_| t.tick()).collect();
        assert_eq!(seq, [true, true, false, false, true, true, false, false]);
    }

    #[test]
    fn input_auto_repeat_timing() {
        let mut r = Repeat::default();
        assert!(r.update(true, MS(16)));
        let mut fires = Vec::new();
        let mut t = 0;
        for _ in 0..60 {
            t += 10;
            if r.update(true, MS(10)) {
                fires.push(t);
            }
        }
        assert_eq!(fires, [400, 480, 560]);
        assert!(!r.update(false, MS(10)));
        assert!(r.update(true, MS(10)));
    }

    #[test]
    fn input_back_start_hold_fires_once_and_masks() {
        let mut h = Hold::default();
        let mut fired = 0;
        let mut last_progress = 0.0;
        for i in 1..=100 {
            let (f, p) = h.update(true, MS(10));
            if f {
                fired += 1;
                assert_eq!(i, 60);
            } else if i < 60 {
                assert!(p > last_progress && p < 1.0);
                last_progress = p;
            }
        }
        assert_eq!(fired, 1);
        assert_eq!(h.update(false, MS(10)), (false, 0.0));
        assert_eq!(
            mask_combo(pad::START | pad::SELECT | pad::A),
            pad::A
        );
        assert_eq!(mask_combo(pad::START), pad::START);
        let b = PadButtons { back: true, start: true, south: true, ..Default::default() };
        assert_eq!(mask_combo(pad_bits(&b, PadKind::Xbox, true)), pad::A);
    }

    #[test]
    fn input_seats_go_in_order_of_first_press() {
        let mut s = Seats::default();
        // Nothing to get back before anyone sat.
        assert_eq!(s.rejoin(10, "deck"), None);
        assert_eq!(s.sit(11, "ps5"), Some(0));
        assert_eq!(s.sit(10, "deck"), Some(1));
        assert_eq!(s.sit(12, "xbox"), Some(2));
        assert_eq!(s.sit(13, "switch"), Some(3));
        assert_eq!(s.sit(14, "other"), None);
        assert_eq!(s.release(10), Some(1));
        assert_eq!(s.sit(14, "other"), Some(1));
        assert_eq!(s.release(99), None);
    }

    #[test]
    fn input_seat_back_on_reconnect() {
        let mut s = Seats::default();
        assert_eq!(s.sit(10, "ps5"), Some(0));
        assert_eq!(s.sit(11, "xbox"), Some(1));
        assert_eq!(s.release(11), Some(1));
        // Another pad gets no seat by plugging in; the same pad gets its own back.
        assert_eq!(s.rejoin(12, "switch"), None);
        assert_eq!(s.rejoin(13, "xbox"), Some(1));
        // Its seat is taken: nothing to rejoin.
        assert_eq!(s.rejoin(14, "xbox"), None);
        // `sit` prefers the old seat over a lower free one.
        assert_eq!(s.release(10), Some(0));
        assert_eq!(s.release(13), Some(1));
        assert_eq!(s.sit(15, "xbox"), Some(1));
        assert_eq!(s.sit(16, "switch"), Some(0));
        // `clear` forgets the keys: the former seat-2 pad sits first and is player 1.
        s.clear();
        assert_eq!(s.rejoin(17, "xbox"), None);
        assert_eq!(s.sit(17, "xbox"), Some(0));
    }

    /// Needs SDL: run with `SDL_VIDEODRIVER=dummy SDL_AUDIODRIVER=dummy` and `--ignored`.
    #[test]
    #[ignore]
    fn input_virtual_pads_sit_by_first_press() {
        use sdl2::sys;
        let sdl = sdl2::init().unwrap();
        let mut input = Input::new(&sdl).unwrap();
        let mut pump = sdl.event_pump().unwrap();
        let ids: Vec<i32> = (0..2)
            .map(|_| unsafe {
                let index = sys::SDL_JoystickAttachVirtual(sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER, 6, 15, 0);
                assert!(index >= 0);
                sys::SDL_JoystickGetDeviceInstanceID(index)
            })
            .collect();
        let mut poll = |input: &mut Input| input.poll(&mut pump, MS(16), |_| None);
        let press = |id: i32, button: Button, down: bool| unsafe {
            let joy = sys::SDL_JoystickFromInstanceID(id);
            assert!(!joy.is_null());
            assert_eq!(sys::SDL_JoystickSetVirtualButton(joy, button as i32, down as u8), 0);
        };
        let opened = |input: &Input| ids.iter().all(|id| input.pads.iter().any(|p| p.id() as i32 == *id));
        for _ in 0..20 {
            if opened(&input) {
                break;
            }
            poll(&mut input);
        }
        assert!(opened(&input));
        let (first, second) = (ids[0], ids[1]);

        // Seating off (menu): a press seats nobody.
        press(second, Button::A, true);
        let f = poll(&mut input);
        assert_eq!((f.pads, f.seated), ([0; 4], 0));

        input.set_seating(true);
        let f = poll(&mut input);
        assert_eq!((f.pads[0], f.seated), (pad::A, 1));
        assert!(f.pad_events.iter().any(|e| matches!(e, PadEvent::Seated(p) if p.slot == Some(0))));

        press(first, Button::X, true);
        let f = poll(&mut input);
        assert_ne!(f.pads[1] & pad::B, 0);
        assert_eq!(f.seated, 2);

        press(first, Button::X, false);
        press(second, Button::A, false);
        poll(&mut input);
        input.reset_seats();
        assert!(input.pads().iter().all(|p| p.slot.is_none()));
        press(first, Button::A, true);
        let f = poll(&mut input);
        assert_eq!((f.pads[0], f.seated), (pad::A, 1));

        // The menu: a click of the right stick at once; Back+Start once, after 0.6 s, also
        // from a pad without a seat, and the game sees neither button meanwhile.
        press(first, Button::A, false);
        press(first, Button::RightStick, true);
        assert!(poll(&mut input).menu);
        press(first, Button::RightStick, false);
        press(second, Button::Back, true);
        press(second, Button::Start, true);
        let frames: Vec<InputFrame> = (0..60).map(|_| poll(&mut input)).collect();
        let fired: Vec<usize> = frames.iter().enumerate().filter(|(_, f)| f.menu).map(|(i, _)| i).collect();
        assert_eq!(fired, [37]);
        assert!(frames.iter().all(|f| f.pads == [0; 4]));
    }

    #[test]
    fn input_kind_steam_deck() {
        assert_eq!(kind_from("Steam Deck", Some(PadKind::Xbox), false), PadKind::SteamDeck);
        assert_eq!(kind_from("Steam Virtual Gamepad", Some(PadKind::Xbox), true), PadKind::SteamDeck);
        assert_eq!(kind_from("Steam Virtual Gamepad", Some(PadKind::Xbox), false), PadKind::Xbox);
        assert_eq!(kind_from("Foo", None, false), PadKind::Generic);
    }
}
