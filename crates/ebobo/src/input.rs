//! Keyboard and gamepads → NES pads, menu navigation and hotkeys. Owner: platform slice.

use std::path::PathBuf;
use std::time::Duration;

use ebobo_core::pad;
use sdl2::controller::{Axis, Button, GameController};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Mod, Scancode};
use sdl2::mouse::MouseButton;
use sdl2::{GameControllerSubsystem, JoystickSubsystem};

use crate::config::ButtonLayout;

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
    /// NES pad bits per local device: [0] = keyboard merged with the first gamepad,
    /// [1..4] = second..fourth gamepad. Gamepad face buttons are positional and follow the
    /// "Buttons" setting: by default West = NES B, South = NES A, North = turbo B, East =
    /// turbo A; with `ButtonLayout::BottomRight` South = NES B, East = NES A, West = turbo B,
    /// North = turbo A. Back/Share = Select, Start/Options = Start, D-pad and left stick =
    /// D-pad. Keyboard: arrows, X = A, Z = B, Enter = Start, Right Shift = Select,
    /// S = turbo A, A = turbo B. Turbo toggles every 2 frames of `poll`.
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
    /// Open/close the menu: Esc, gamepad Guide/PS button, or Back+Start held for 0.6 s.
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
    /// Screenshot pressed: F12 or gamepad Back+North (Share+Triangle on PlayStation).
    pub screenshot: bool,
    /// Window closed or Cmd+Q.
    pub quit: bool,
    /// A file dropped onto the window.
    pub dropped_file: Option<PathBuf>,
    /// Connected gamepads.
    pub gamepads: usize,
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
    /// Index into `InputFrame::pads` this controller feeds.
    pub slot: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PadEvent {
    Connected(PadInfo),
    Disconnected(PadInfo),
}

// ---------------------------------------------------------------------------------------------
// Pure parts (no SDL state): mapping, timers, slot assignment.

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

/// Positional gamepad → NES bits; `layout` picks the face buttons of NES B and A and of their
/// turbo versions. `turbo_on` = current turbo phase.
fn pad_bits(b: &PadButtons, layout: ButtonLayout, turbo_on: bool) -> u8 {
    let (nes_b, nes_a, turbo_b, turbo_a) = match layout {
        ButtonLayout::LeftBottom => (b.west, b.south, b.north, b.east),
        ButtonLayout::BottomRight => (b.south, b.east, b.west, b.north),
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
    set(turbo_b && turbo_on, pad::B);
    set(turbo_a && turbo_on, pad::A);
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

/// Slot assignment: a reconnecting pad (same key = GUID + name) gets its old slot back when it
/// is free; any other pad takes the lowest free slot (a pad that reconnects with a new GUID,
/// e.g. USB vs Bluetooth, or a replacement pad becomes player 1 again).
#[derive(Default)]
struct Slots {
    owner: [Option<u32>; 4],
    last_key: [Option<String>; 4],
}

impl Slots {
    fn assign(&mut self, id: u32, key: &str) -> Option<usize> {
        let free = |s: &usize| self.owner[*s].is_none();
        let slot = (0..4)
            .filter(free)
            .find(|&s| self.last_key[s].as_deref() == Some(key))
            .or_else(|| (0..4).find(free))?;
        self.owner[slot] = Some(id);
        self.last_key[slot] = Some(key.to_owned());
        Some(slot)
    }

    fn release(&mut self, id: u32) -> Option<usize> {
        let slot = self.owner.iter().position(|o| *o == Some(id))?;
        self.owner[slot] = None;
        Some(slot)
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

/// PlayStation pads (DualShock 4, DualSense, USB or Bluetooth) must work out of the box:
/// SDL's HIDAPI PS4/PS5 drivers are enabled via hints before the controller subsystem starts,
/// pads are hot-plugged at any time, and each pad's light bar / player LEDs show its slot
/// (1 blue, 2 red, 3 green, 4 pink) via `set_led` / `set_player_index`. A reconnecting pad gets
/// its old slot back when it is free; other pads take the lowest free slot.
pub struct Input {
    gc: GameControllerSubsystem,
    joy: JoystickSubsystem,
    clipboard: sdl2::clipboard::ClipboardUtil,
    text_input: sdl2::keyboard::TextInputUtil,
    text_on: bool,
    pads: Vec<Pad>,
    slots: Slots,
    turbo: Turbo,
    repeat: [Repeat; 4],
    events: Vec<PadEvent>,
    /// The finger being tracked (`touch_id`, `finger_id`); other fingers are ignored.
    finger: Option<(i64, i64)>,
    /// Which face buttons are NES B and A.
    buttons: ButtonLayout,
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
            slots: Slots::default(),
            turbo: Turbo::default(),
            repeat: Default::default(),
            events: Vec::new(),
            finger: None,
            buttons: ButtonLayout::default(),
        };
        let n = input.joy.num_joysticks().unwrap_or(0);
        for i in 0..n {
            input.open(i);
        }
        // Start-up pads are not hot-plug news.
        input.events.clear();
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
        let Some(slot) = self.slots.assign(id, &key) else { return };
        let kind = kind_from(&name, sdl_kind(&ctrl), crate::config::is_steam_deck());
        let (r, g, b) = SLOT_RGB[slot];
        let mut ctrl = ctrl;
        let _ = ctrl.set_led(r, g, b);
        let raw = unsafe { sdl2::sys::SDL_GameControllerFromInstanceID(id as i32) };
        if !raw.is_null() {
            unsafe { sdl2::sys::SDL_GameControllerSetPlayerIndex(raw, slot as i32) };
        }
        let info = PadInfo { name, kind, slot };
        self.events.push(PadEvent::Connected(info.clone()));
        self.pads.push(Pad {
            ctrl,
            info,
            hold: Hold::default(),
            shot: false,
        });
    }

    fn close(&mut self, id: u32) {
        if let Some(i) = self.pads.iter().position(|p| p.id() == id) {
            let pad = self.pads.remove(i);
            self.slots.release(id);
            self.events.push(PadEvent::Disconnected(pad.info));
        }
    }

    /// Applies the "Buttons" setting to every gamepad from the next `poll`.
    pub fn set_buttons(&mut self, layout: ButtonLayout) {
        self.buttons = layout;
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
                Event::ControllerButtonDown { button, .. } => {
                    f.pad.used = true;
                    match button {
                        Button::A => f.nav.confirm = true,
                        Button::B => f.nav.back = true,
                        Button::Y => f.pad.shift = true,
                        Button::X => f.pad.layout = true,
                        Button::Start => f.pad.start = true,
                        Button::Guide => f.menu = true,
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
            f.pads[p.info.slot] |=
                mask_combo(pad_bits(&mask_shot_combo(&b), self.buttons, turbo_on));
        }

        let [up, down, left, right] = dirs;
        f.nav.up = self.repeat[0].update(up, dt);
        f.nav.down = self.repeat[1].update(down, dt);
        f.nav.left = self.repeat[2].update(left, dt);
        f.nav.right = self.repeat[3].update(right, dt);

        f.gamepads = self.pads.len();
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

    /// Connected controllers in slot order.
    pub fn pads(&self) -> Vec<PadInfo> {
        let mut v: Vec<PadInfo> = self.pads.iter().map(|p| p.info.clone()).collect();
        v.sort_by_key(|p| p.slot);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn input_positional_mapping() {
        use ButtonLayout::{BottomRight, LeftBottom};
        let one = |f: fn(&mut PadButtons)| {
            let mut b = PadButtons::default();
            f(&mut b);
            b
        };
        let (south, east) = (one(|b| b.south = true), one(|b| b.east = true));
        let (west, north) = (one(|b| b.west = true), one(|b| b.north = true));

        // Default: left = B, bottom = A, top = turbo B, right = turbo A.
        for turbo_on in [true, false] {
            assert_eq!(pad_bits(&west, LeftBottom, turbo_on), pad::B);
            assert_eq!(pad_bits(&south, LeftBottom, turbo_on), pad::A);
        }
        assert_eq!(pad_bits(&north, LeftBottom, true), pad::B);
        assert_eq!(pad_bits(&north, LeftBottom, false), 0);
        assert_eq!(pad_bits(&east, LeftBottom, true), pad::A);
        assert_eq!(pad_bits(&east, LeftBottom, false), 0);

        // Bottom = B, right = A, left = turbo B, top = turbo A.
        for turbo_on in [true, false] {
            assert_eq!(pad_bits(&south, BottomRight, turbo_on), pad::B);
            assert_eq!(pad_bits(&east, BottomRight, turbo_on), pad::A);
        }
        assert_eq!(pad_bits(&west, BottomRight, true), pad::B);
        assert_eq!(pad_bits(&west, BottomRight, false), 0);
        assert_eq!(pad_bits(&north, BottomRight, true), pad::A);
        assert_eq!(pad_bits(&north, BottomRight, false), 0);

        for layout in [LeftBottom, BottomRight] {
            let b = PadButtons { back: true, start: true, ..Default::default() };
            assert_eq!(pad_bits(&b, layout, false), pad::SELECT | pad::START);
            let b = PadButtons { lx: -0.6, ly: 0.4, up: true, ..Default::default() };
            assert_eq!(pad_bits(&b, layout, false), pad::LEFT | pad::UP);
        }
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
        let layout = ButtonLayout::default();
        assert_eq!(pad_bits(&mask_shot_combo(&b), layout, true), pad::A);
        // Without Back, North is turbo B.
        let b = PadButtons { north: true, south: true, ..Default::default() };
        assert_eq!(pad_bits(&mask_shot_combo(&b), layout, true), pad::A | pad::B);
        let b = PadButtons { back: true, ..Default::default() };
        assert_eq!(pad_bits(&mask_shot_combo(&b), layout, true), pad::SELECT);
        // Triggers never reach the NES pad.
        let b = PadButtons { lt: 1.0, rt: 1.0, ..Default::default() };
        assert_eq!(pad_bits(&b, layout, true), 0);
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
        assert_eq!(mask_combo(pad_bits(&b, ButtonLayout::default(), true)), pad::A);
    }

    #[test]
    fn input_slot_reuse_on_reconnect() {
        let mut s = Slots::default();
        assert_eq!(s.assign(10, "ps5"), Some(0));
        assert_eq!(s.assign(11, "xbox"), Some(1));
        assert_eq!(s.assign(12, "deck"), Some(2));
        assert_eq!(s.release(10), Some(0));
        // The same pad reconnecting gets its old slot back.
        assert_eq!(s.assign(14, "ps5"), Some(0));
        assert_eq!(s.assign(13, "switch"), Some(3));
        assert_eq!(s.assign(15, "other"), None);
        assert_eq!(s.release(11), Some(1));
        assert_eq!(s.assign(15, "other"), Some(1));
        // Its old slot taken meanwhile: a reconnecting pad takes the lowest free one.
        assert_eq!(s.release(13), Some(3));
        assert_eq!(s.release(12), Some(2));
        assert_eq!(s.assign(16, "deck2"), Some(2));
        assert_eq!(s.assign(17, "deck"), Some(3));
    }

    #[test]
    fn input_slot_new_guid_takes_lowest_free() {
        let mut s = Slots::default();
        assert_eq!(s.assign(10, "ps5-usb"), Some(0));
        assert_eq!(s.assign(11, "xbox"), Some(1));
        assert_eq!(s.release(10), Some(0));
        // Same pad back over Bluetooth (new GUID): player 1 again, not a never-used slot.
        assert_eq!(s.assign(12, "ps5-bt"), Some(0));
        // A replacement pad also fills the lowest free slot.
        assert_eq!(s.release(11), Some(1));
        assert_eq!(s.assign(13, "switch"), Some(1));
    }

    #[test]
    fn input_kind_steam_deck() {
        assert_eq!(kind_from("Steam Deck", Some(PadKind::Xbox), false), PadKind::SteamDeck);
        assert_eq!(kind_from("Steam Virtual Gamepad", Some(PadKind::Xbox), true), PadKind::SteamDeck);
        assert_eq!(kind_from("Steam Virtual Gamepad", Some(PadKind::Xbox), false), PadKind::Xbox);
        assert_eq!(kind_from("Foo", None, false), PadKind::Generic);
    }
}
