//! Keyboard and gamepads → console pads, menu navigation and hotkeys. Owner: platform slice.
//!
//! The pad bits follow the running system (`Input::set_system`): a NES pad
//! (`ebobo_console::nes::pad`, with turbo A / B on the shoulders) or a six-button Mega Drive pad
//! (`ebobo_console::md::pad`, no turbo). Menu navigation, hotkeys, seats and the ways into the
//! menu are the same on both.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use ebobo_console::System;
use ebobo_console::md::pad as md_pad;
use ebobo_console::nes::pad;
use sdl2::controller::{Axis, Button, GameController};
use sdl2::event::{Event, WindowEvent};
use sdl2::keyboard::{Mod, Scancode};
use sdl2::mouse::MouseButton;
use sdl2::{GameControllerSubsystem, JoystickSubsystem};

/// Menu navigation, edge-triggered. Directions auto-repeat while held (400 ms delay, then
/// every 80 ms). Keyboard: arrows, Enter = confirm, Esc/Backspace = back. Gamepad: D-pad or
/// left stick, South (A on Xbox/Deck, Cross on PlayStation) = confirm, East (B / Circle) = back.
/// Tabs (no repeat): left / right shoulder or Shift+Tab / Tab = previous / next. North (Y /
/// Triangle; not while Back is held, that is the screenshot combo) = `fav`, West (X / Square)
/// = `search`, Start (Options, +) or Space or the keyboard's Menu key = `options`. Face
/// buttons count by position on every pad, Nintendo ones too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nav {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub confirm: bool,
    pub back: bool,
    pub prev_tab: bool,
    pub next_tab: bool,
    pub fav: bool,
    pub search: bool,
    /// The menu of what is selected (a game's own menu in the library).
    pub options: bool,
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
/// picture (letterbox bars). `Secondary` is the right mouse button going down (Ctrl+click on
/// a Mac).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEvent {
    Down(Option<(i32, i32)>),
    /// Movement while pressed.
    Move(Option<(i32, i32)>),
    Up(Option<(i32, i32)>),
    /// Mouse wheel, positive = away from the user (scroll up).
    Wheel(i32),
    Secondary(Option<(i32, i32)>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputFrame {
    /// Pad bits per seat that are plainly held (never turbo presses), 16 bits wide, of the
    /// system set with `Input::set_system`: NES bits (`ebobo_console::nes::pad`) in the low
    /// byte, or Mega Drive bits (`ebobo_console::md::pad`). [0] = keyboard merged
    /// with the gamepad in seat 1, [1..4] = the gamepads in seats 2-4. A gamepad without a seat
    /// feeds nothing; it takes the lowest free seat the first time it presses a game button
    /// (a turbo button counts) while seating is on (`set_seating`).
    /// Opposite face buttons do the same: South and North = NES A, West and East = NES B
    /// (Nintendo pads: East and West = A, South and North = B, so the printed A and B match).
    /// Back/Share = Select, Start/Options = Start, D-pad and left stick = D-pad. Keyboard:
    /// arrows, X = A, Z = B, Enter = Start, Right Shift = Select.
    /// Mega Drive, by position on every pad: West (X, Square) = A, South (A, Cross) = B, East
    /// (B, Circle) = C, North (Y, Triangle) = Y, left shoulder = X, right shoulder = Z, Start =
    /// Start, Back/Share = Mode, D-pad and left stick = D-pad. Keyboard: arrows, Z X C = A B C,
    /// A S D = X Y Z, Enter = Start, Right Shift = Mode.
    /// Back+Start (Select+Start, Mode+Start) held together are hidden from the game: that is
    /// the menu hold.
    pub pads: [u16; 4],
    /// NES only (always zero on the Mega Drive, whose shoulders are buttons).
    /// Per seat, the `pad::A` / `pad::B` bits whose turbo button is held: left shoulder =
    /// turbo B, right shoulder = turbo A; keyboard (seat 1) S = turbo A, A = turbo B. Held
    /// state, not presses: solo expands it with `solo_pads`, netplay sends it as held bits
    /// that every peer expands from the simulated frame number.
    pub turbo: [u8; 4],
    /// Local turbo phase of this poll: on for 2 polls, off for 2.
    pub turbo_on: bool,
    /// A keyboard key went down (auto-repeat not counted).
    pub key_used: bool,
    pub nav: Nav,
    /// Text typed since the last poll (only while text input is enabled).
    pub text: String,
    /// Backspace pressed (text fields).
    pub backspace: bool,
    /// Cmd+V / Ctrl+V clipboard text.
    pub paste: Option<String>,
    /// Cmd+C / Ctrl+C went down (not an auto-repeat): the screen that has something worth
    /// copying puts it on the clipboard with [`Input::copy_text`].
    pub copy: bool,
    /// Open/close the menu: Esc, a click of the right stick, gamepad Guide/PS button,
    /// PlayStation touchpad click, or Back+Start held for 0.6 s.
    pub menu: bool,
    /// 0.0-1.0 progress of the Back+Start hold (for an on-screen hint), 0 when not held.
    pub menu_hold: f32,
    /// F11, Alt+Enter, Cmd+Ctrl+F.
    pub fullscreen: bool,
    /// F5 / F7: quick save and load in solo, save / load for everyone in netplay.
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
    /// The window stopped being the front one (`Some(false)`) or became it again
    /// (`Some(true)`) on this frame; `None` when nothing changed.
    pub focus: Option<bool>,
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

impl InputFrame {
    /// Pad bits for solo play: the held bits plus the turbo bits while the local phase is on.
    pub fn solo_pads(&self) -> [u16; 4] {
        let mut pads = self.pads;
        if self.turbo_on {
            for (p, t) in pads.iter_mut().zip(self.turbo) {
                *p |= u16::from(t);
            }
        }
        pads
    }
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

/// Positional gamepad → (held NES bits, turbo bits). Opposite face buttons do the same, so any
/// two neighbouring ones are B + A: South and North = A, West and East = B; on Nintendo pads
/// East and West = A, South and North = B (the printed A and B). Shoulders are turbo B (left)
/// and turbo A (right): they set `pad::B` / `pad::A` in the turbo bits only.
fn pad_bits(b: &PadButtons, kind: PadKind) -> (u8, u8) {
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
    let mut turbo = 0;
    if b.l1 {
        turbo |= pad::B;
    }
    if b.r1 {
        turbo |= pad::A;
    }
    set(b.back, pad::SELECT);
    set(b.start, pad::START);
    set(up, pad::UP);
    set(down, pad::DOWN);
    set(left, pad::LEFT);
    set(right, pad::RIGHT);
    (bits, turbo)
}

/// Keyboard → (held NES bits, turbo bits): arrows, X = A, Z = B, Enter = Start,
/// Right Shift = Select; S = turbo A, A = turbo B.
fn keyboard_bits(pressed: impl Fn(Scancode) -> bool) -> (u8, u8) {
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
    let mut turbo = 0;
    if pressed(Scancode::S) {
        turbo |= pad::A;
    }
    if pressed(Scancode::A) {
        turbo |= pad::B;
    }
    (bits, turbo)
}

/// Positional gamepad → held Mega Drive bits, the same on every kind of pad: West = A,
/// South = B, East = C, North = Y, left shoulder = X, right shoulder = Z, Start = Start,
/// Back = Mode, D-pad and left stick = directions.
fn md_pad_bits(b: &PadButtons) -> u16 {
    let (up, down, left, right) = b.dirs();
    let map = [
        (up, md_pad::UP),
        (down, md_pad::DOWN),
        (left, md_pad::LEFT),
        (right, md_pad::RIGHT),
        (b.west, md_pad::A),
        (b.south, md_pad::B),
        (b.east, md_pad::C),
        (b.l1, md_pad::X),
        (b.north, md_pad::Y),
        (b.r1, md_pad::Z),
        (b.start, md_pad::START),
        (b.back, md_pad::MODE),
    ];
    map.iter().filter(|(on, _)| *on).fold(0, |bits, (_, bit)| bits | bit)
}

/// Keyboard → held Mega Drive bits: arrows, Z X C = A B C, A S D = X Y Z, Enter = Start,
/// Right Shift = Mode.
fn md_keyboard_bits(pressed: impl Fn(Scancode) -> bool) -> u16 {
    let map = [
        (Scancode::Up, md_pad::UP),
        (Scancode::Down, md_pad::DOWN),
        (Scancode::Left, md_pad::LEFT),
        (Scancode::Right, md_pad::RIGHT),
        (Scancode::Z, md_pad::A),
        (Scancode::X, md_pad::B),
        (Scancode::C, md_pad::C),
        (Scancode::A, md_pad::X),
        (Scancode::S, md_pad::Y),
        (Scancode::D, md_pad::Z),
        (Scancode::Return, md_pad::START),
        (Scancode::RShift, md_pad::MODE),
    ];
    map.iter().filter(|(sc, _)| pressed(*sc)).fold(0, |bits, (_, bit)| bits | bit)
}

/// Gamepad → (held bits of `system`, NES turbo bits; zero on the Mega Drive).
fn game_bits(system: System, b: &PadButtons, kind: PadKind) -> (u16, u8) {
    match system {
        System::MegaDrive => (md_pad_bits(b), 0),
        _ => {
            let (held, turbo) = pad_bits(b, kind);
            (u16::from(held), turbo)
        }
    }
}

/// Keyboard → (held bits of `system`, NES turbo bits; zero on the Mega Drive).
fn key_bits(system: System, pressed: impl Fn(Scancode) -> bool) -> (u16, u8) {
    match system {
        System::MegaDrive => (md_keyboard_bits(pressed), 0),
        _ => {
            let (held, turbo) = keyboard_bits(pressed);
            (u16::from(held), turbo)
        }
    }
}

/// A gamepad button going down → `Nav` presses. `back_held`: Back is down on that pad, which
/// makes North the screenshot combo instead of `fav`.
fn nav_button(nav: &mut Nav, button: Button, back_held: bool) {
    match button {
        Button::A => nav.confirm = true,
        Button::B => nav.back = true,
        Button::Y if !back_held => nav.fav = true,
        Button::X => nav.search = true,
        Button::LeftShoulder => nav.prev_tab = true,
        Button::RightShoulder => nav.next_tab = true,
        // Back+Start is the menu hold.
        Button::Start if !back_held => nav.options = true,
        _ => {}
    }
}

/// A key going down (not an auto-repeat) → the tab and options presses: Tab = next tab,
/// Shift+Tab = previous tab, Space or the Menu key = `options`.
fn nav_key(nav: &mut Nav, sc: Scancode, shift: bool) {
    match sc {
        Scancode::Tab if shift => nav.prev_tab = true,
        Scancode::Tab => nav.next_tab = true,
        Scancode::Space | Scancode::Application => nav.options = true,
        _ => {}
    }
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
fn mask_combo(bits: u16) -> u16 {
    let combo = u16::from(pad::START | pad::SELECT);
    if bits & combo == combo {
        bits & !combo
    } else {
        bits
    }
}

/// `mask_combo` for the bits of `system`: on the Mega Drive the pair is Mode+Start.
fn mask_menu_combo(system: System, bits: u16) -> u16 {
    let combo = md_pad::MODE | md_pad::START;
    match system {
        System::MegaDrive if bits & combo == combo => bits & !combo,
        System::MegaDrive => bits,
        _ => mask_combo(bits),
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

/// The seat owner id of the keyboard (gamepads have SDL's instance ids, counted from 0).
const KEYBOARD: u32 = u32::MAX;

/// The seat the keyboard's keys play on. Alone at a device (`sits` false) it is seat 1,
/// together with whatever gamepad sits there, so either works for one player. Where two or
/// more play at this device online (`sits`), the keyboard is a device like a gamepad: its
/// first game key (`pressed`, while `seating`) takes the lowest free seat, and it has none
/// before that.
fn keyboard_seat(seats: &mut Seats, sits: bool, seat: &mut Option<usize>, seating: bool, pressed: bool) -> Option<usize> {
    if !sits {
        return Some(0);
    }
    if seat.is_none() && seating && pressed {
        *seat = seats.sit(KEYBOARD, "keyboard");
    }
    *seat
}

/// Controller family from what SDL reports: the controller name and SDL's controller type.
/// A Deck's own controls, and on a Deck every Steam virtual gamepad, are `SteamDeck`. This is
/// the answer when Steam publishes nothing about the real controller behind a virtual pad;
/// `pad_identity` overrides it with the slot of `SteamVirtualGamepadInfo` when there is one.
fn kind_from(name: &str, ty: Option<PadKind>, on_deck: bool) -> PadKind {
    let steam_virtual = name.contains("Steam Virtual") || name.contains("Steam Controller");
    if name.contains("Steam Deck") || (on_deck && steam_virtual) {
        return PadKind::SteamDeck;
    }
    ty.unwrap_or(PadKind::Generic)
}

/// Steam Input's virtual Xbox pad, which hides the real controller from SDL.
fn is_steam_virtual(name: &str) -> bool {
    name.contains("Steam Virtual")
}

/// One `[slot N]` section of the file named by `SteamVirtualGamepadInfo`: the real controller
/// behind Steam's virtual gamepad N.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SteamSlot {
    name: String,
    vid: u16,
    pid: u16,
    /// `type=`: `xbox360`, `ps4`, `switchpro`, `steam`, ...
    ty: String,
}

/// `0x054c` or `1356`.
fn parse_id(s: &str) -> Option<u16> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16).ok(),
        None => s.parse().ok(),
    }
}

/// Parses Steam's virtual gamepad info: `[slot N]` sections of `key=value` lines. Unknown
/// keys, blank lines, lines before the first section and anything malformed are ignored.
fn parse_steam_info(text: &str) -> Vec<(u32, SteamSlot)> {
    let mut slots: Vec<(u32, SteamSlot)> = Vec::new();
    let mut open = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(head) = line.strip_prefix('[') {
            let n = head
                .strip_suffix(']')
                .and_then(|h| h.trim().strip_prefix("slot"))
                .and_then(|n| n.trim().parse::<u32>().ok());
            open = n.is_some();
            if let Some(n) = n {
                slots.push((n, SteamSlot::default()));
            }
            continue;
        }
        if !open {
            continue;
        }
        let (Some((key, value)), Some((_, slot))) = (line.split_once('='), slots.last_mut()) else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" => slot.name = value.to_owned(),
            "VID" => slot.vid = parse_id(value).unwrap_or(slot.vid),
            "PID" => slot.pid = parse_id(value).unwrap_or(slot.pid),
            "type" => slot.ty = value.to_owned(),
            _ => {}
        }
    }
    slots
}

/// Controller family of a Steam slot: its `type`, else its vendor id.
fn steam_slot_kind(ty: &str, vid: u16) -> PadKind {
    match ty {
        "ps3" | "ps4" | "ps5" => return PadKind::PlayStation,
        "xbox360" | "xboxone" => return PadKind::Xbox,
        "switchpro" => return PadKind::Nintendo,
        "steam" => return PadKind::SteamDeck,
        t if t.starts_with("joycon") => return PadKind::Nintendo,
        _ => {}
    }
    match vid {
        0x054c => PadKind::PlayStation,
        0x045e => PadKind::Xbox,
        0x057e => PadKind::Nintendo,
        0x28de => PadKind::SteamDeck,
        _ => PadKind::Generic,
    }
}

/// Steam's slot number of a virtual pad, from its kernel device name
/// `Microsoft X-Box 360 pad N` (`kernel_device_name`).
fn steam_slot_from_joystick_name(name: &str) -> Option<u32> {
    let rest = &name[name.rfind("pad ")? + 4..];
    let digits = rest.trim_start();
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse().ok()
}

/// The kernel's name of the input device behind SDL joystick `index`, from sysfs. SDL's own
/// joystick name will not do: it calls every Steam virtual pad `Steam Virtual Gamepad`.
#[cfg(target_os = "linux")]
fn kernel_device_name(index: u32) -> Option<String> {
    let path = unsafe { sdl2::sys::SDL_JoystickPathForIndex(index as i32) };
    if path.is_null() {
        return None;
    }
    // `/dev/input/event11`: the node name is the same under `/sys/class/input`.
    let path = unsafe { std::ffi::CStr::from_ptr(path) }.to_str().ok()?;
    let node = std::path::Path::new(path).file_name()?;
    let name = std::fs::read_to_string(std::path::Path::new("/sys/class/input").join(node).join("device/name")).ok()?;
    Some(name.trim().to_owned())
}

/// Steam's virtual pads carry their slot only in the Linux kernel name.
#[cfg(not(target_os = "linux"))]
fn kernel_device_name(_index: u32) -> Option<String> {
    None
}

/// Name and kind of the real controller in `slot`; `None` when Steam does not list the slot
/// or gives it no name.
fn steam_identity(slots: &[(u32, SteamSlot)], slot: u32) -> Option<(String, PadKind)> {
    let (_, s) = slots.iter().find(|(n, s)| *n == slot && !s.name.is_empty())?;
    Some((s.name.clone(), steam_slot_kind(&s.ty, s.vid)))
}

/// Name and kind shown for a pad: those of the real controller behind a Steam virtual gamepad
/// when Steam lists it (`steam`), else SDL's name and `kind_from`.
fn pad_identity(
    name: &str,
    ty: Option<PadKind>,
    on_deck: bool,
    steam: Option<(String, PadKind)>,
) -> (String, PadKind) {
    match steam {
        Some(real) if is_steam_virtual(name) => real,
        _ => (name.to_owned(), kind_from(name, ty, on_deck)),
    }
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
    /// Steam's slot number when this is a Steam virtual gamepad.
    steam_slot: Option<u32>,
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

/// USB vendor and product id of the SDL device (0 when unknown).
fn sdl_ids(ctrl: &GameController) -> (u16, u16) {
    let raw = unsafe { sdl2::sys::SDL_GameControllerFromInstanceID(ctrl.instance_id() as i32) };
    if raw.is_null() {
        return (0, 0);
    }
    unsafe {
        (
            sdl2::sys::SDL_GameControllerGetVendor(raw),
            sdl2::sys::SDL_GameControllerGetProduct(raw),
        )
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
///
/// In Steam's Game Mode every controller arrives as a "Steam Virtual Gamepad". When Steam
/// names the real controllers in the file of the `SteamVirtualGamepadInfo` environment
/// variable, a virtual pad shows the name and kind of the controller in its slot. The file is
/// re-read when a pad connects and, at most once a second, when its modification time
/// changed; open pads then change name and kind in place and keep their seats. Without the
/// variable the file system is never touched and virtual pads keep SDL's name.
/// Every connect prints one `ebobo: gamepad ...` line to stdout.
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
    /// The keyboard takes a seat of its own (`keyboard_seat`), and the one it has.
    keyboard_sits: bool,
    keyboard_at: Option<usize>,
    /// Gamepad used last (instance id, kind); `None` after a keyboard key.
    hint: Option<(u32, PadKind)>,
    turbo: Turbo,
    repeat: [Repeat; 4],
    events: Vec<PadEvent>,
    /// The finger being tracked (`touch_id`, `finger_id`); other fingers are ignored.
    finger: Option<(i64, i64)>,
    /// Path from `SteamVirtualGamepadInfo`; `None` = variable not set.
    steam_info: Option<PathBuf>,
    /// Modification time of that file when it was last read (`None` = unreadable).
    steam_mtime: Option<SystemTime>,
    /// Time since the modification time was last checked.
    steam_checked: Duration,
    /// System whose pad bits `poll` builds.
    system: System,
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
            ("SDL_MAC_CTRL_CLICK_EMULATE_RIGHT_CLICK", "1"),
        ] {
            sdl2::hint::set(k, v);
        }
        // With `SteamDeck` in the environment (Steam sets it on a Deck) SDL opens Steam's
        // keyboard on every SDL_StartTextInput, so it popped up over the library, which keeps
        // text input on for typing a search. Its keys never reach us in Game Mode anyway; the
        // built-in keyboard (osk.rs) takes its place and physical keyboards still type. The
        // bundled SDL 2.26 has no SDL_HINT_ENABLE_SCREEN_KEYBOARD; the X11 driver reads this
        // hint once, when video starts, and nothing else in SDL reads it.
        sdl2::hint::set_with_priority("SteamDeck", "0", &sdl2::hint::Hint::Override);
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
            keyboard_sits: false,
            keyboard_at: None,
            hint: None,
            turbo: Turbo::default(),
            repeat: Default::default(),
            events: Vec::new(),
            finger: None,
            steam_info: std::env::var_os("SteamVirtualGamepadInfo")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            steam_mtime: None,
            steam_checked: Duration::ZERO,
            system: System::Nes,
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
        let (name, kind) =
            pad_identity(&name, sdl_kind(&ctrl), crate::config::is_steam_deck(), None);
        let steam_slot = (is_steam_virtual(&name))
            .then(|| kernel_device_name(index))
            .flatten()
            .and_then(|n| steam_slot_from_joystick_name(&n));
        let (vid, pid) = sdl_ids(&ctrl);
        let mut ctrl = ctrl;
        show_seat(&mut ctrl, slot);
        self.pads.push(Pad {
            ctrl,
            info: PadInfo { name, kind, slot },
            key,
            steam_slot,
            hold: Hold::default(),
            shot: false,
        });
        // A new controller also moves the others between Steam's slots.
        self.refresh_steam_pads();
        let info = self.pads[self.pads.len() - 1].info.clone();
        let slot_text = steam_slot.map_or("-".to_owned(), |n| n.to_string());
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "ebobo: gamepad \"{}\" kind={:?} steam_slot={slot_text} vid={vid:04x} pid={pid:04x}",
            info.name, info.kind
        );
        let _ = out.flush();
        self.events.push(PadEvent::Connected(info));
    }

    /// Re-reads Steam's virtual gamepad info and gives every open virtual pad the name and
    /// kind of the real controller in its slot. A pad whose slot is not listed stays as it is.
    fn refresh_steam_pads(&mut self) {
        let Some(path) = &self.steam_info else { return };
        self.steam_checked = Duration::ZERO;
        self.steam_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let Ok(text) = std::fs::read_to_string(path) else { return };
        let slots = parse_steam_info(&text);
        for p in &mut self.pads {
            let Some((name, kind)) = p.steam_slot.and_then(|n| steam_identity(&slots, n)) else {
                continue;
            };
            p.info.name = name;
            p.info.kind = kind;
            if let Some((shown, hint_kind)) = &mut self.hint {
                if *shown == p.ctrl.instance_id() {
                    *hint_kind = kind;
                }
            }
        }
    }

    /// At most once a second: re-reads Steam's info when the file's modification time changed.
    fn watch_steam_info(&mut self, dt: Duration) {
        let Some(path) = &self.steam_info else { return };
        self.steam_checked += dt;
        if self.steam_checked < Duration::from_secs(1) {
            return;
        }
        self.steam_checked = Duration::ZERO;
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if mtime != self.steam_mtime {
            self.refresh_steam_pads();
        }
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

    /// The system of the game being played (NES until set): `InputFrame::pads` carries its pad
    /// bits from the next poll on. Navigation, hotkeys and seats do not change.
    pub fn set_system(&mut self, system: System) {
        self.system = system;
    }

    /// Lets gamepads take seats: on while a game runs with the menu hidden.
    pub fn set_seating(&mut self, on: bool) {
        self.seating = on;
    }

    /// Every gamepad and the keyboard lose their seats (a game starts); the next one to press
    /// a button is player 1.
    pub fn reset_seats(&mut self) {
        self.seats.clear();
        self.keyboard_at = None;
        for p in &mut self.pads {
            p.info.slot = None;
            show_seat(&mut p.ctrl, None);
        }
    }

    /// Whether the keyboard is a player's device of its own (two or more play at this device
    /// online) or plays seat 1 together with a gamepad (`keyboard_seat`).
    pub fn set_keyboard_sits(&mut self, sits: bool) {
        if !sits && self.keyboard_at.take().is_some() {
            self.seats.release(KEYBOARD);
        }
        self.keyboard_sits = sits;
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
                // Only these events tell about focus: headless runs never get one and keep running.
                Event::Window {
                    win_event: WindowEvent::FocusLost,
                    ..
                } => f.focus = Some(false),
                Event::Window {
                    win_event: WindowEvent::FocusGained,
                    ..
                } => f.focus = Some(true),
                Event::DropFile { filename, .. } => f.dropped_file = Some(PathBuf::from(filename)),
                Event::TextInput { text, .. } if self.text_on => f.text.push_str(&text),
                Event::ControllerDeviceAdded { which, .. } => self.open(which),
                Event::ControllerDeviceRemoved { which, .. } => self.close(which),
                Event::ControllerButtonDown { which, button, .. } => {
                    f.pad.used = true;
                    match button {
                        Button::A | Button::B | Button::X | Button::Y | Button::LeftShoulder | Button::RightShoulder | Button::Start => {
                            let back_held = self.pads.iter().any(|p| p.id() == which && p.ctrl.button(Button::Back));
                            nav_button(&mut f.nav, button, back_held);
                        }
                        _ => {}
                    }
                    match button {
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
                    let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
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
                        Scancode::C if (cmd || ctrl) && !repeat => f.copy = true,
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
                        Scancode::Tab | Scancode::Space | Scancode::Application if !repeat => nav_key(&mut f.nav, sc, shift),
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
                Event::MouseButtonDown { mouse_btn: MouseButton::Right, x, y, .. } => {
                    f.mouse_moved = true;
                    f.pointer.push(PointerEvent::Secondary(to_canvas(WindowPos::Point(x as f32, y as f32))));
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

        f.turbo_on = self.turbo.tick();
        let ks = events.keyboard_state();
        let (keys, key_turbo) = key_bits(self.system, |sc| ks.is_scancode_pressed(sc));
        let pressed = keys != 0 || key_turbo != 0;
        if let Some(seat) = keyboard_seat(&mut self.seats, self.keyboard_sits, &mut self.keyboard_at, self.seating, pressed) {
            (f.pads[seat], f.turbo[seat]) = (keys, key_turbo);
        }
        f.rewind = ks.is_scancode_pressed(Scancode::Backspace) && !self.text_on;
        f.fast_forward = ks.is_scancode_pressed(Scancode::Tab);
        let mut dirs = [
            ks.is_scancode_pressed(Scancode::Up),
            ks.is_scancode_pressed(Scancode::Down),
            ks.is_scancode_pressed(Scancode::Left),
            ks.is_scancode_pressed(Scancode::Right),
        ];

        self.watch_steam_info(dt);

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
            // Both combos hide plain buttons only; turbo bits are A/B and pass through.
            let (held, turbo) = game_bits(self.system, &mask_shot_combo(&b), p.info.kind);
            let held = mask_menu_combo(self.system, held);
            if p.info.slot.is_none() && self.seating && (held != 0 || turbo != 0) {
                if let Some(slot) = self.seats.sit(p.ctrl.instance_id(), &p.key) {
                    p.info.slot = Some(slot);
                    show_seat(&mut p.ctrl, Some(slot));
                    self.events.push(PadEvent::Seated(p.info.clone()));
                }
            }
            if let Some(slot) = p.info.slot {
                f.pads[slot] |= held;
                f.turbo[slot] |= turbo;
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

    /// Puts `text` on the system clipboard (Cmd+C / Ctrl+C on a screen with something to copy).
    pub fn copy_text(&mut self, text: &str) {
        if let Err(err) = self.clipboard.set_clipboard_text(text) {
            eprintln!("ebobo: clipboard: {err}");
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

        // Bottom and top = A, left and right = B.
        for kind in [PadKind::Xbox, PadKind::PlayStation, PadKind::SteamDeck, PadKind::Generic] {
            assert_eq!(pad_bits(&south, kind), (pad::A, 0));
            assert_eq!(pad_bits(&north, kind), (pad::A, 0));
            assert_eq!(pad_bits(&west, kind), (pad::B, 0));
            assert_eq!(pad_bits(&east, kind), (pad::B, 0));
        }
        // Nintendo: the printed A (right) and Y (left) = A, B (bottom) and X (top) = B.
        assert_eq!(pad_bits(&east, PadKind::Nintendo), (pad::A, 0));
        assert_eq!(pad_bits(&west, PadKind::Nintendo), (pad::A, 0));
        assert_eq!(pad_bits(&south, PadKind::Nintendo), (pad::B, 0));
        assert_eq!(pad_bits(&north, PadKind::Nintendo), (pad::B, 0));

        // Shoulders are turbo: they land in the turbo bits and never in the held ones.
        let (l1, r1) = (one(|b| b.l1 = true), one(|b| b.r1 = true));
        for kind in [PadKind::Xbox, PadKind::Nintendo] {
            assert_eq!(pad_bits(&l1, kind), (0, pad::B));
            assert_eq!(pad_bits(&r1, kind), (0, pad::A));
            // Held and turbo of the same button stay apart.
            let b = PadButtons { l1: true, r1: true, back: true, ..Default::default() };
            assert_eq!(pad_bits(&b, kind), (pad::SELECT, pad::A | pad::B));

            let b = PadButtons { back: true, start: true, ..Default::default() };
            assert_eq!(pad_bits(&b, kind), (pad::SELECT | pad::START, 0));
            let b = PadButtons { lx: -0.6, ly: 0.4, up: true, ..Default::default() };
            assert_eq!(pad_bits(&b, kind), (pad::LEFT | pad::UP, 0));
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
        assert_eq!(pad_bits(&mask_shot_combo(&b), kind), (pad::B, 0));
        // Without Back, North is A.
        let b = PadButtons { north: true, west: true, ..Default::default() };
        assert_eq!(pad_bits(&mask_shot_combo(&b), kind), (pad::A | pad::B, 0));
        let b = PadButtons { back: true, ..Default::default() };
        assert_eq!(pad_bits(&mask_shot_combo(&b), kind), (pad::SELECT, 0));
        // Triggers never reach the NES pad.
        let b = PadButtons { lt: 1.0, rt: 1.0, ..Default::default() };
        assert_eq!(pad_bits(&b, kind), (0, 0));
    }

    #[test]
    fn input_keyboard_map() {
        let only = |k: Scancode| move |sc: Scancode| sc == k;
        assert_eq!(keyboard_bits(only(Scancode::X)), (pad::A, 0));
        assert_eq!(keyboard_bits(only(Scancode::Z)), (pad::B, 0));
        assert_eq!(keyboard_bits(only(Scancode::Return)), (pad::START, 0));
        assert_eq!(keyboard_bits(only(Scancode::RShift)), (pad::SELECT, 0));
        assert_eq!(keyboard_bits(only(Scancode::Up)), (pad::UP, 0));
        assert_eq!(keyboard_bits(only(Scancode::Down)), (pad::DOWN, 0));
        assert_eq!(keyboard_bits(only(Scancode::Left)), (pad::LEFT, 0));
        assert_eq!(keyboard_bits(only(Scancode::Right)), (pad::RIGHT, 0));
        // Turbo keys land in the turbo bits and never in the held ones.
        assert_eq!(keyboard_bits(only(Scancode::S)), (0, pad::A));
        assert_eq!(keyboard_bits(only(Scancode::A)), (0, pad::B));
        assert_eq!(keyboard_bits(|sc| matches!(sc, Scancode::S | Scancode::A | Scancode::Left)), (pad::LEFT, pad::A | pad::B));
    }

    #[test]
    fn input_solo_pads_expand_turbo_on_the_local_phase() {
        let w = |bits: u8| u16::from(bits);
        let mut f = InputFrame { pads: [w(pad::LEFT), w(pad::A), 0, 0], turbo: [pad::A, pad::B, 0, pad::A | pad::B], ..Default::default() };
        assert_eq!(f.solo_pads(), [w(pad::LEFT), w(pad::A), 0, 0]);
        f.turbo_on = true;
        assert_eq!(f.solo_pads(), [w(pad::LEFT | pad::A), w(pad::A | pad::B), 0, w(pad::A | pad::B)]);
        // The frame itself keeps held and turbo apart for netplay.
        assert_eq!(f.pads, [w(pad::LEFT), w(pad::A), 0, 0]);
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
            mask_combo(u16::from(pad::START | pad::SELECT | pad::A)),
            u16::from(pad::A)
        );
        assert_eq!(mask_combo(u16::from(pad::START)), u16::from(pad::START));
        let b = PadButtons { back: true, start: true, south: true, ..Default::default() };
        assert_eq!(mask_combo(u16::from(pad_bits(&b, PadKind::Xbox).0)), u16::from(pad::A));
    }

    #[test]
    fn input_keyboard_takes_its_own_seat_only_where_several_play() {
        // One player at the device: the keyboard is seat 1 whoever else sits there.
        let mut s = Seats::default();
        let mut at = None;
        assert_eq!(s.sit(10, "pad"), Some(0));
        assert_eq!(keyboard_seat(&mut s, false, &mut at, true, true), Some(0));
        assert_eq!(s.sit(11, "pad2"), Some(1), "it took no seat");

        // Several players: no seat before its first game key, and none taken with the menu up.
        let mut s = Seats::default();
        let mut at = None;
        assert_eq!(keyboard_seat(&mut s, true, &mut at, true, false), None);
        assert_eq!(keyboard_seat(&mut s, true, &mut at, false, true), None);
        // A gamepad pressed first is player 1, the keyboard player 2, and it keeps that seat.
        assert_eq!(s.sit(10, "pad"), Some(0));
        assert_eq!(keyboard_seat(&mut s, true, &mut at, true, true), Some(1));
        assert_eq!(keyboard_seat(&mut s, true, &mut at, true, false), Some(1));
        assert_eq!(s.sit(11, "pad2"), Some(2));

        // The keyboard pressed first is player 1 and the gamepad after it player 2.
        let mut s = Seats::default();
        let mut at = None;
        assert_eq!(keyboard_seat(&mut s, true, &mut at, true, true), Some(0));
        assert_eq!(s.sit(10, "pad"), Some(1));
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
        assert_eq!((f.pads[0], f.seated), (u16::from(pad::A), 1));
        assert!(f.pad_events.iter().any(|e| matches!(e, PadEvent::Seated(p) if p.slot == Some(0))));

        press(first, Button::X, true);
        let f = poll(&mut input);
        assert_ne!(f.pads[1] & u16::from(pad::B), 0);
        assert_eq!(f.seated, 2);

        press(first, Button::X, false);
        press(second, Button::A, false);
        poll(&mut input);
        input.reset_seats();
        assert!(input.pads().iter().all(|p| p.slot.is_none()));
        press(first, Button::A, true);
        let f = poll(&mut input);
        assert_eq!((f.pads[0], f.seated), (u16::from(pad::A), 1));

        // A held turbo button seats a pad too, and shows up in `turbo` only, on every poll.
        press(second, Button::RightShoulder, true);
        for _ in 0..4 {
            let f = poll(&mut input);
            assert_eq!((f.pads[1], f.turbo[1], f.seated), (0, pad::A, 2));
            assert_eq!(f.solo_pads()[1], if f.turbo_on { u16::from(pad::A) } else { 0 });
        }
        press(second, Button::RightShoulder, false);

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
    fn input_mega_drive_pad_mapping() {
        let cases: [(fn(&mut PadButtons), u16); 14] = [
            (|b| b.up = true, md_pad::UP),
            (|b| b.down = true, md_pad::DOWN),
            (|b| b.left = true, md_pad::LEFT),
            (|b| b.right = true, md_pad::RIGHT),
            (|b| b.west = true, md_pad::A),
            (|b| b.south = true, md_pad::B),
            (|b| b.east = true, md_pad::C),
            (|b| b.l1 = true, md_pad::X),
            (|b| b.north = true, md_pad::Y),
            (|b| b.r1 = true, md_pad::Z),
            (|b| b.start = true, md_pad::START),
            (|b| b.back = true, md_pad::MODE),
            (|b| (b.lx, b.ly) = (-0.6, 0.7), md_pad::LEFT | md_pad::DOWN),
            (|b| (b.lt, b.rt) = (1.0, 1.0), 0),
        ];
        // By position on every pad, Nintendo ones included; never any turbo.
        for kind in [PadKind::Xbox, PadKind::Nintendo, PadKind::PlayStation, PadKind::SteamDeck, PadKind::Generic] {
            for (i, (press, bits)) in cases.iter().enumerate() {
                let mut b = PadButtons::default();
                press(&mut b);
                assert_eq!(game_bits(System::MegaDrive, &b, kind), (*bits, 0), "case {i} {kind:?}");
            }
            let all = PadButtons { west: true, south: true, east: true, north: true, l1: true, r1: true, ..Default::default() };
            let six = md_pad::A | md_pad::B | md_pad::C | md_pad::X | md_pad::Y | md_pad::Z;
            assert_eq!(game_bits(System::MegaDrive, &all, kind), (six, 0));
        }
        // The screenshot combo hides Mode and Y; triggers stay hotkeys.
        let b = PadButtons { back: true, north: true, west: true, lt: 1.0, ..Default::default() };
        assert_eq!(game_bits(System::MegaDrive, &mask_shot_combo(&b), PadKind::Xbox), (md_pad::A, 0));
        assert_eq!(pad_hotkeys(&b), (true, false, true));
    }

    #[test]
    fn input_nes_bits_do_not_change_with_the_dispatch() {
        let w = |bits: u8| u16::from(bits);
        let b = PadButtons { south: true, west: true, l1: true, r1: true, back: true, up: true, ..Default::default() };
        assert_eq!(game_bits(System::Nes, &b, PadKind::Xbox), (w(pad::A | pad::B | pad::SELECT | pad::UP), pad::A | pad::B));
        let b = PadButtons { east: true, ..Default::default() };
        assert_eq!(game_bits(System::Nes, &b, PadKind::Nintendo), (w(pad::A), 0));
        assert_eq!(game_bits(System::Nes, &b, PadKind::Xbox), (w(pad::B), 0));
        let keys = |sc: Scancode| matches!(sc, Scancode::X | Scancode::S | Scancode::A | Scancode::Return);
        assert_eq!(key_bits(System::Nes, keys), (w(pad::A | pad::START), pad::A | pad::B));
        // The same keys are B, X, Y and Start on the Mega Drive.
        assert_eq!(key_bits(System::MegaDrive, keys), (md_pad::B | md_pad::X | md_pad::Y | md_pad::START, 0));
    }

    #[test]
    fn input_mega_drive_keyboard_map() {
        for (key, bit) in [
            (Scancode::Up, md_pad::UP),
            (Scancode::Down, md_pad::DOWN),
            (Scancode::Left, md_pad::LEFT),
            (Scancode::Right, md_pad::RIGHT),
            (Scancode::Z, md_pad::A),
            (Scancode::X, md_pad::B),
            (Scancode::C, md_pad::C),
            (Scancode::A, md_pad::X),
            (Scancode::S, md_pad::Y),
            (Scancode::D, md_pad::Z),
            (Scancode::Return, md_pad::START),
            (Scancode::RShift, md_pad::MODE),
            (Scancode::LShift, 0),
            (Scancode::Tab, 0),
        ] {
            assert_eq!(key_bits(System::MegaDrive, |sc| sc == key), (bit, 0), "{key:?}");
        }
    }

    #[test]
    fn input_mode_start_is_masked_on_the_mega_drive() {
        let md = System::MegaDrive;
        let b = PadButtons { back: true, start: true, south: true, ..Default::default() };
        let (held, _) = game_bits(md, &b, PadKind::Xbox);
        assert_eq!(held, md_pad::MODE | md_pad::START | md_pad::B);
        assert_eq!(mask_menu_combo(md, held), md_pad::B);
        assert_eq!(mask_menu_combo(md, md_pad::START | md_pad::C), md_pad::START | md_pad::C);
        assert_eq!(mask_menu_combo(md, md_pad::MODE), md_pad::MODE);
        // Mega Drive bits that equal NES Select+Start (LEFT | RIGHT here) are left alone, and
        // the NES pair is masked as before.
        let nes_combo = u16::from(pad::START | pad::SELECT);
        assert_eq!(mask_menu_combo(md, nes_combo), nes_combo);
        assert_eq!(mask_menu_combo(System::Nes, nes_combo | u16::from(pad::A)), u16::from(pad::A));
        assert_eq!(mask_menu_combo(System::Nes, md_pad::MODE | md_pad::START), md_pad::MODE | md_pad::START);
    }

    #[test]
    fn input_nav_presses_from_buttons_and_keys() {
        let button = |b: Button, back_held: bool| {
            let mut n = Nav::default();
            nav_button(&mut n, b, back_held);
            n
        };
        assert_eq!(button(Button::A, false), Nav { confirm: true, ..Default::default() });
        assert_eq!(button(Button::B, false), Nav { back: true, ..Default::default() });
        assert_eq!(button(Button::Y, false), Nav { fav: true, ..Default::default() });
        assert_eq!(button(Button::X, false), Nav { search: true, ..Default::default() });
        assert_eq!(button(Button::LeftShoulder, false), Nav { prev_tab: true, ..Default::default() });
        assert_eq!(button(Button::RightShoulder, true), Nav { next_tab: true, ..Default::default() });
        // Back+North is the screenshot, not a favourite.
        assert_eq!(button(Button::Y, true), Nav::default());
        assert_eq!(button(Button::Start, false), Nav { options: true, ..Default::default() });
        // Back+Start is the menu hold, not the options press.
        assert_eq!(button(Button::Start, true), Nav::default());
        assert_eq!(button(Button::DPadUp, false), Nav::default());

        let key = |sc: Scancode, shift: bool| {
            let mut n = Nav::default();
            nav_key(&mut n, sc, shift);
            n
        };
        assert_eq!(key(Scancode::Tab, false), Nav { next_tab: true, ..Default::default() });
        assert_eq!(key(Scancode::Tab, true), Nav { prev_tab: true, ..Default::default() });
        assert_eq!(key(Scancode::Space, false), Nav { options: true, ..Default::default() });
        assert_eq!(key(Scancode::Space, true), Nav { options: true, ..Default::default() });
        assert_eq!(key(Scancode::Application, false), Nav { options: true, ..Default::default() });
        assert_eq!(key(Scancode::F, false), Nav::default());
    }

    /// Needs SDL: run with `SDL_VIDEODRIVER=dummy SDL_AUDIODRIVER=dummy`, `--ignored` and
    /// `--test-threads=1` (SDL lives on one thread).
    #[test]
    #[ignore]
    fn input_virtual_pad_nav_presses_and_mega_drive_bits() {
        use sdl2::sys;
        let sdl = sdl2::init().unwrap();
        let mut input = Input::new(&sdl).unwrap();
        let mut pump = sdl.event_pump().unwrap();
        let id = unsafe {
            let index = sys::SDL_JoystickAttachVirtual(sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER, 6, 15, 0);
            assert!(index >= 0);
            sys::SDL_JoystickGetDeviceInstanceID(index)
        };
        let mut poll = |input: &mut Input| input.poll(&mut pump, MS(16), |_| None);
        let press = |button: Button, down: bool| unsafe {
            let joy = sys::SDL_JoystickFromInstanceID(id);
            assert!(!joy.is_null());
            assert_eq!(sys::SDL_JoystickSetVirtualButton(joy, button as i32, down as u8), 0);
        };
        for _ in 0..20 {
            if !input.pads.is_empty() {
                break;
            }
            poll(&mut input);
        }
        assert_eq!(input.pads.len(), 1);

        // One press = one frame of the edge, however long the button stays down. The on-screen
        // keyboard's `PadPress` is filled from the same presses.
        for (button, want) in [
            (Button::Y, Nav { fav: true, ..Default::default() }),
            (Button::X, Nav { search: true, ..Default::default() }),
            (Button::LeftShoulder, Nav { prev_tab: true, ..Default::default() }),
            (Button::RightShoulder, Nav { next_tab: true, ..Default::default() }),
            (Button::Start, Nav { options: true, ..Default::default() }),
        ] {
            press(button, true);
            let f = poll(&mut input);
            assert_eq!(f.nav, want, "{button:?}");
            assert_eq!((f.pad.shift, f.pad.layout), (button == Button::Y, button == Button::X));
            assert_eq!(poll(&mut input).nav, Nav::default());
            press(button, false);
            assert_eq!(poll(&mut input).nav, Nav::default());
        }
        // Back+North: a screenshot and no favourite.
        press(Button::Back, true);
        poll(&mut input);
        press(Button::Y, true);
        let f = poll(&mut input);
        assert!(f.screenshot && !f.nav.fav);
        press(Button::Y, false);
        // Back+Start is the menu hold: no options press.
        press(Button::Start, true);
        assert!(!poll(&mut input).nav.options);
        press(Button::Start, false);
        press(Button::Back, false);
        poll(&mut input);

        // Mega Drive: the same pad gives six-button bits, the shoulders are X and Z, no turbo.
        input.set_system(System::MegaDrive);
        input.set_seating(true);
        for (button, bit) in [
            (Button::X, md_pad::A),
            (Button::A, md_pad::B),
            (Button::B, md_pad::C),
            (Button::LeftShoulder, md_pad::X),
            (Button::Y, md_pad::Y),
            (Button::RightShoulder, md_pad::Z),
            (Button::Start, md_pad::START),
            (Button::Back, md_pad::MODE),
            (Button::DPadUp, md_pad::UP),
            (Button::DPadDown, md_pad::DOWN),
            (Button::DPadLeft, md_pad::LEFT),
            (Button::DPadRight, md_pad::RIGHT),
        ] {
            press(button, true);
            let f = poll(&mut input);
            assert_eq!((f.pads, f.turbo, f.seated), ([bit, 0, 0, 0], [0; 4], 1), "{button:?}");
            assert_eq!(f.solo_pads(), f.pads);
            press(button, false);
            poll(&mut input);
        }
        // Mode+Start: the menu after 0.6 s, and the game sees neither meanwhile.
        press(Button::Back, true);
        press(Button::Start, true);
        let frames: Vec<InputFrame> = (0..60).map(|_| poll(&mut input)).collect();
        assert_eq!(frames.iter().filter(|f| f.menu).count(), 1);
        assert!(frames.iter().all(|f| f.pads == [0; 4]));
        press(Button::Start, false);
        assert_eq!(poll(&mut input).pads[0], md_pad::MODE);
        // Back to the NES: Back alone is Select again.
        input.set_system(System::Nes);
        assert_eq!(poll(&mut input).pads[0], u16::from(pad::SELECT));
    }

    #[test]
    fn input_kind_steam_deck() {
        assert_eq!(kind_from("Steam Deck", Some(PadKind::Xbox), false), PadKind::SteamDeck);
        assert_eq!(kind_from("Steam Virtual Gamepad", Some(PadKind::Xbox), true), PadKind::SteamDeck);
        assert_eq!(kind_from("Steam Virtual Gamepad", Some(PadKind::Xbox), false), PadKind::Xbox);
        assert_eq!(kind_from("Foo", None, false), PadKind::Generic);
    }

    const DECK_INFO: &str = "[slot 0]\nname=Steam Deck Controller\nVID=0x28de\nPID=0x1205\nhandle=0x00000ff244308c2b\ntype=steam\n";

    #[test]
    fn input_steam_info_owner_sample() {
        let slots = parse_steam_info(DECK_INFO);
        assert_eq!(
            slots,
            [(0, SteamSlot { name: "Steam Deck Controller".into(), vid: 0x28de, pid: 0x1205, ty: "steam".into() })]
        );
        assert_eq!(steam_identity(&slots, 0), Some(("Steam Deck Controller".into(), PadKind::SteamDeck)));
    }

    #[test]
    fn input_steam_info_two_slots_with_ps4() {
        let text = format!(
            "{DECK_INFO}\r\n[slot 1]\r\nname=PS4 Controller\r\nVID=0x054c\r\nPID=0x09cc\r\nhandle=0x1\r\ntype=ps4\r\n"
        );
        let slots = parse_steam_info(&text);
        assert_eq!(slots.len(), 2);
        assert_eq!(
            slots[1],
            (1, SteamSlot { name: "PS4 Controller".into(), vid: 0x054c, pid: 0x09cc, ty: "ps4".into() })
        );
        assert_eq!(steam_identity(&slots, 1), Some(("PS4 Controller".into(), PadKind::PlayStation)));
        assert_eq!(steam_identity(&slots, 0), Some(("Steam Deck Controller".into(), PadKind::SteamDeck)));
        // The virtual pad in slot 1 shows the PS4; a real pad is never renamed.
        assert_eq!(
            pad_identity("Steam Virtual Gamepad", Some(PadKind::Xbox), true, steam_identity(&slots, 1)),
            ("PS4 Controller".to_owned(), PadKind::PlayStation)
        );
        assert_eq!(
            pad_identity("Xbox One Controller", Some(PadKind::Xbox), true, steam_identity(&slots, 1)),
            ("Xbox One Controller".to_owned(), PadKind::Xbox)
        );
    }

    #[test]
    fn input_steam_info_garbage_and_missing_slot() {
        let text = "name=before any section\nrubbish\n\n[slot x]\nname=bad section\n[other]\nname=nope\n\
                    [slot 2]\n = \nno equals here\ncolour=red\nVID=zz\nPID=2508\nname = Pro Controller \ntype=switchpro\n[slot 3]\nVID=0x045e\n";
        let slots = parse_steam_info(text);
        assert_eq!(
            slots,
            [
                (2, SteamSlot { name: "Pro Controller".into(), vid: 0, pid: 2508, ty: "switchpro".into() }),
                (3, SteamSlot { vid: 0x045e, ..Default::default() }),
            ]
        );
        assert_eq!(steam_identity(&slots, 2), Some(("Pro Controller".into(), PadKind::Nintendo)));
        // Not listed, and listed without a name: nothing to show.
        assert_eq!(steam_identity(&slots, 0), None);
        assert_eq!(steam_identity(&slots, 3), None);
        assert_eq!(parse_steam_info(""), []);
        assert_eq!(steam_identity(&parse_steam_info(DECK_INFO), 1), None);
    }

    #[test]
    fn input_steam_slot_from_joystick_name() {
        assert_eq!(steam_slot_from_joystick_name("Microsoft X-Box 360 pad 0"), Some(0));
        assert_eq!(steam_slot_from_joystick_name("Microsoft X-Box 360 pad 12"), Some(12));
        assert_eq!(steam_slot_from_joystick_name("X-Box 360 pad 3 (virtual)"), Some(3));
        assert_eq!(steam_slot_from_joystick_name("Microsoft X-Box 360 pad"), None);
        assert_eq!(steam_slot_from_joystick_name("Xbox 360 Controller"), None);
        assert_eq!(steam_slot_from_joystick_name(""), None);
    }

    #[test]
    fn input_steam_slot_kind_table() {
        use PadKind::*;
        for (ty, vid, kind) in [
            ("ps3", 0, PlayStation),
            ("ps4", 0x054c, PlayStation),
            ("ps5", 0x28de, PlayStation),
            ("", 0x054c, PlayStation),
            ("xbox360", 0, Xbox),
            ("xboxone", 0x045e, Xbox),
            ("unknown", 0x045e, Xbox),
            ("switchpro", 0, Nintendo),
            ("joyconleft", 0, Nintendo),
            ("joyconright", 0, Nintendo),
            ("joyconpair", 0, Nintendo),
            ("", 0x057e, Nintendo),
            ("steam", 0, SteamDeck),
            ("", 0x28de, SteamDeck),
            ("", 0, Generic),
            ("wheel", 0x046d, Generic),
        ] {
            assert_eq!(steam_slot_kind(ty, vid), kind, "{ty} {vid:#06x}");
        }
    }

    #[test]
    fn input_kind_steam_deck_without_steam_info() {
        for (name, ty, on_deck, kind) in [
            ("Steam Deck", Some(PadKind::Xbox), false, PadKind::SteamDeck),
            ("Steam Virtual Gamepad", Some(PadKind::Xbox), true, PadKind::SteamDeck),
            ("Steam Virtual Gamepad", Some(PadKind::Xbox), false, PadKind::Xbox),
            ("Foo", None, false, PadKind::Generic),
        ] {
            assert_eq!(pad_identity(name, ty, on_deck, None), (name.to_owned(), kind));
        }
    }
}
