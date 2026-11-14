//! Menus, lobby screen, pause menu, on-screen display and toasts, drawn on the 256x240 canvas.
//! Gamepad-first (Steam Deck), keyboard works everywhere. Owner: ui slice (also owns i18n.rs).

use std::time::{Duration, SystemTime};

use ebobo_net::{
    normalize_code, LobbyStatus, NatKind, NetDiag, NetError, NetStats, RomState, RouteKind, Stage, CODE_ALPHABET, CODE_LEN,
};

use crate::canvas::{Canvas, Color, Font};
use crate::config::{is_steam_deck, Config, ScreenMode, SpriteLimit};
use crate::crt::Effect;
use crate::i18n::{Lang, S};
use crate::input::{InputFrame, Nav, PadEvent, PadInfo, PadKind, PointerEvent};
use crate::osk::{self, Charset, Osk, OskNav, OskOutput};
use crate::library::RomEntry;

/// What the user asked for; the app performs it.
#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    PlaySolo(RomEntry),
    HostRoom(RomEntry),
    /// Normalised room code.
    JoinRoom(String),
    /// Host by IP on `Config::direct_port`.
    HostDirect(RomEntry),
    /// `host:port` as typed (port defaults to 7777 when omitted).
    JoinDirect(String),
    /// Host pressed Start in the lobby.
    StartLobbyGame,
    LeaveLobby,
    Resume,
    Reset,
    SaveState,
    LoadState,
    /// Pause menu slot row changed the current save slot (0-based).
    SelectSlot(u8),
    LeaveGame,
    /// `UiCtx::config` was edited; the app saves it and applies scale/volume/fullscreen.
    ConfigChanged,
    Rescan,
    /// Run `ebobo_net::diagnose` in the background and hand the result to `Ui::set_net_diag`.
    RunNetCheck,
    Quit,
}

/// The running game, for the pause menu and the on-screen display.
pub struct GameView<'a> {
    pub rom_name: &'a str,
    pub netplay: bool,
    /// Player names by NES controller slot (netplay), empty in solo.
    pub names: &'a [String],
    pub stats: Option<&'a NetStats>,
    /// Netplay warning line, e.g. a peer's connection is interrupted (already localised by
    /// the app via `Ui::text`).
    pub notice: Option<String>,
    pub desync: bool,
    /// GGRS has not advanced a frame for over 0.5 s (waiting for a peer): the game is dimmed.
    pub stalled: bool,
    /// Solo: current save slot (0-based) and when it was written (`None` = empty).
    pub save_slot: Option<(u8, Option<SystemTime>)>,
}

pub struct UiCtx<'a> {
    pub library: &'a [RomEntry],
    pub scanning: bool,
    pub lobby: Option<&'a LobbyStatus>,
    pub game: Option<GameView<'a>>,
    pub pads: &'a [PadInfo],
    pub config: &'a mut Config,
    pub version: &'static str,
}

/// Localised strings the app needs outside the UI (OSD notices, window title bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Text {
    /// "{name}: connection lost, {secs} s" style notice; app formats with `format_notice`.
    PeerInterrupted,
    PeerLeft,
    Desync,
    StateSaved,
    StateLoaded,
    NoQuickSave,
    StateWrongRom,
    StateOldVersion,
    StateCorrupt,
    StateSaveFailed,
    /// Opening a ROM is refused while an online game runs.
    CantOpenRomOnline,
    /// The wall clock jumped during netplay (sleep / suspend).
    SleepGap,
    /// "Screenshot: {name}" with the saved file path.
    ScreenshotSaved,
    ScreenshotFailed,
    /// macOS self-update impossible from here (translocated/read-only).
    UpdateMoveToApps,
    /// Linux self-update impossible (install dir not writable).
    UpdateRerunInstaller,
    /// The Deck's buttons arrive as keyboard keys (Steam's Desktop Layout).
    DeckKeys,
    /// How to switch the Deck to its gamepad layout.
    DeckKeysFix,
}

// ---------------------------------------------------------------------------------------------
// Style

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    [r, g, b, 255]
}
const BG: Color = rgb(0x14, 0x13, 0x1A);
const PANEL: Color = rgb(0x22, 0x20, 0x2B);
const ACCENT: Color = rgb(0xE4, 0x3B, 0x2C);
const WHITE: Color = rgb(0xF4, 0xF2, 0xF7);
const SEC: Color = rgb(0x9C, 0x98, 0xA8);
const DIS: Color = rgb(0x5C, 0x59, 0x68);
const DIS_BAR: Color = rgb(0x3A, 0x37, 0x45);
const OK: Color = rgb(0x4C, 0xC6, 0x6A);
const WARN: Color = rgb(0xE8, 0xB8, 0x3A);
const ERR: Color = rgb(0xE4, 0x50, 0x50);
const SHADE: Color = [0, 0, 0, 170];
const SLOT: [Color; 4] = [rgb(0x3B, 0x7B, 0xE4), rgb(0xE4, 0x3B, 0x2C), rgb(0x4C, 0xC6, 0x6A), rgb(0xE4, 0x6A, 0xB4)];

const PICKER_ROWS: usize = 13;
/// ROM picker row pitch: 14 canvas px ≈ 44 pt on the Deck's 1280x800 screen.
const PICKER_STEP: i32 = 14;
/// Smallest tap target height, canvas px (≈ 44 pt on the Deck).
const MIN_TAP: i32 = 14;
/// A press that moves this far (canvas px) on a list becomes a scroll drag, not a tap.
const DRAG_START: i32 = 6;
const TOAST_SECS: f32 = 2.5;
const NICK_MAX: usize = 16;

fn ping_color(ms: u32) -> Color {
    if ms < 80 {
        OK
    } else if ms < 150 {
        WARN
    } else {
        ERR
    }
}

/// Cuts `s` to `cols` characters, ending with ".." when cut.
fn trunc(s: &str, cols: usize) -> String {
    if s.chars().count() <= cols {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(cols.saturating_sub(2)).collect();
        out.push_str("..");
        out
    }
}

/// Word wrap to `cols` characters per line.
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let len = line.chars().count();
        if len > 0 && len + 1 + word.chars().count() > cols {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&trunc(word, cols));
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Full-width hit region of a row whose text sits at `y`, `step` px pitch, at least `MIN_TAP`
/// tall and centred on the text like the accent bar.
fn row_area(y: i32, step: i32) -> Area {
    let h = step.max(MIN_TAP);
    Area { x: 0, y: y + 4 - h / 2, w: 256, h }
}

// ---------------------------------------------------------------------------------------------
// State

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    Solo,
    Host,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Addr,
    Nick,
    Server,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Main,
    Picker(Purpose),
    Code,
    ByIp,
    Field(Field),
    Settings,
    /// Settings ▸ Advanced: server address, always-relay, network check.
    Advanced,
    /// Settings ▸ Advanced ▸ Network check.
    NetCheck,
    Lobby,
    Pause,
}

impl Screen {
    /// Index into the per-screen selection memory.
    fn slot(self) -> usize {
        match self {
            Screen::Main => 0,
            Screen::Picker(_) => 1,
            Screen::Code => 2,
            Screen::ByIp => 3,
            Screen::Field(_) => 4,
            Screen::Settings => 5,
            Screen::Lobby => 6,
            Screen::Pause => 7,
            Screen::Advanced => 8,
            Screen::NetCheck => 9,
        }
    }
}

struct Modal {
    title: String,
    lines: Vec<String>,
    /// Two-choice confirmation (Leave / Stay) performing this action on Leave, instead of an
    /// OK box.
    confirm: Option<UiAction>,
    sel: usize,
}

/// One menu row.
struct Item {
    label: String,
    value: Option<String>,
    /// Left/right change the value (draws ◀ ▶ on the selected row).
    arrows: bool,
    disabled: bool,
}

impl Item {
    fn new(label: &str) -> Item {
        Item { label: label.to_string(), value: None, arrows: false, disabled: false }
    }
    fn value(label: &str, value: String, arrows: bool) -> Item {
        Item { label: label.to_string(), value: Some(value), arrows, disabled: false }
    }
}

const MAIN_ITEMS: [S; 6] = [S::CreateRoom, S::JoinByCode, S::PlaySolo, S::ByIp, S::Settings, S::Quit];
const PAUSE_SOLO: [S; 6] = [S::Resume, S::Slot, S::SaveState, S::LoadState, S::Reset, S::ExitToMenu];
const PAUSE_NET: [S; 3] = [S::Resume, S::ResetAll, S::ExitGame];
const DELAYS: [Option<u8>; 7] = [None, Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)];

/// Hint bar glyphs.
#[derive(Clone, Copy)]
enum Glyph {
    Confirm,
    Back,
    LeftRight,
}

/// A rectangle on the canvas that takes taps and clicks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Area {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// A button of the gamepad/keyboard a tap stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Confirm,
    Back,
    Left,
    Right,
    Up,
    Down,
}

impl Key {
    fn nav(self) -> Nav {
        let mut n = Nav::default();
        match self {
            Key::Confirm => n.confirm = true,
            Key::Back => n.back = true,
            Key::Left => n.left = true,
            Key::Right => n.right = true,
            Key::Up => n.up = true,
            Key::Down => n.down = true,
        }
        n
    }
}

/// What a hit region does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    /// Row `i` of the top screen's list (picker: library index). Press selects it, release
    /// confirms; on a row that cycles a value, a release in `back` (x from..to, the left third of
    /// the value area) goes left, anywhere else right.
    Row { i: usize, back: Option<(i32, i32)>, enabled: bool },
    /// Modal button (0 = OK / Leave, 1 = Stay).
    Modal(usize),
    /// Room-code cell: press moves the cursor to it.
    Cell(usize),
    /// Hint-bar button or arrow: release acts as that button.
    Key(Key),
    /// On-screen keyboard key: press holds it, release on it types.
    Osk(usize),
}

/// A press in progress (mouse button or finger down).
#[derive(Clone, Copy, Debug)]
struct Press {
    hit: Hit,
    y0: i32,
    /// Moved far enough on the ROM list to scroll it; the release then does not activate.
    dragging: bool,
    /// Rows already scrolled by this drag.
    scrolled: i32,
}

pub struct Ui {
    lang: Lang,
    stack: Vec<Screen>,
    sel: [usize; 10],
    picker_top: usize,
    code: [Option<u8>; CODE_LEN],
    code_cur: usize,
    field: String,
    /// Last room code sent to join; pre-filled on the next code entry (kept after a failure).
    last_code: Option<String>,
    modal: Option<Modal>,
    toasts: Vec<(String, f32)>,
    clock: f32,
    /// Last network check result (`None` before the first one finishes).
    diag: Option<NetDiag>,
    /// A network check runs in the background.
    diag_running: bool,
    /// "vX.Y.Z" being installed by the self-updater (main-menu status line).
    update: Option<String>,
    /// Hit regions of the last drawn frame (topmost last).
    regions: Vec<(Area, Hit)>,
    press: Option<Press>,
    /// Built-in keyboard of the open text field; `None` = hidden.
    osk: Option<Osk>,
    /// Keyboard key released under the finger, typed by the next `update`.
    osk_tap: Option<usize>,
    /// The last confirm came from a gamepad or a tap: the next field opens with the keyboard.
    osk_wanted: bool,
}

impl Ui {
    pub fn new(lang: Lang) -> Ui {
        Ui {
            lang,
            stack: Vec::new(),
            sel: [0; 10],
            picker_top: 0,
            code: [None; CODE_LEN],
            code_cur: 0,
            field: String::new(),
            last_code: None,
            modal: None,
            toasts: Vec::new(),
            clock: 0.0,
            diag: None,
            diag_running: false,
            update: None,
            regions: Vec::new(),
            press: None,
            osk: None,
            osk_tap: None,
            osk_wanted: false,
        }
    }

    pub fn set_lang(&mut self, lang: Lang) {
        self.lang = lang;
    }

    /// Main menu (no game running).
    pub fn show_main(&mut self) {
        self.stack = vec![Screen::Main];
        self.press = None;
    }

    /// Main menu on top (no game, lobby or submenu): the self-updater may relaunch now.
    pub fn on_main(&self) -> bool {
        self.stack == [Screen::Main]
    }

    /// Version the self-updater installs ("v0.2.3"), shown under the version in the main menu.
    pub fn set_update(&mut self, version: Option<String>) {
        self.update = version;
    }

    /// Lobby screen (room code, players, Start for the host).
    pub fn show_lobby(&mut self) {
        self.stack = vec![Screen::Lobby];
        self.sel[Screen::Lobby.slot()] = 0;
        self.press = None;
    }

    /// Pause menu over the running game (solo pauses; netplay keeps running underneath).
    pub fn show_pause(&mut self) {
        self.stack = vec![Screen::Pause];
        self.sel[Screen::Pause.slot()] = 0;
        self.press = None;
    }

    /// Hides menus (game view: only OSD and toasts are drawn).
    pub fn hide(&mut self) {
        self.stack.clear();
        self.press = None;
    }

    pub fn is_visible(&self) -> bool {
        !self.stack.is_empty() || self.modal.is_some()
    }

    /// A text field (room code, IP, nickname, server) has focus.
    pub fn wants_text_input(&self) -> bool {
        self.modal.is_none() && matches!(self.stack.last(), Some(Screen::Code | Screen::Field(_)))
    }

    /// Modal message box (dismissed with confirm/back).
    pub fn error(&mut self, message: &str) {
        self.modal = Some(Modal { title: self.t(S::Error).to_string(), lines: wrap(message, 27), confirm: None, sel: 0 });
    }

    /// Modal for a network failure, localised per variant with a hint (e.g. RomMissing →
    /// "ask the host to send the ROM / put it in a ROM folder").
    pub fn net_error(&mut self, err: &NetError) {
        let [title, cause, todo] = self.lang.net_error(err);
        let mut lines = wrap(cause, 27);
        lines.push(String::new());
        lines.extend(wrap(todo, 27));
        self.modal = Some(Modal { title: title.to_string(), lines, confirm: None, sel: 0 });
    }

    /// Short notice at the bottom for ~2.5 s, also during play.
    pub fn toast(&mut self, message: &str) {
        self.toast_secs(message, TOAST_SECS);
    }

    /// Like `toast`, shown for `secs` seconds.
    pub fn toast_secs(&mut self, message: &str, secs: f32) {
        self.toasts.push((message.to_string(), secs));
        if self.toasts.len() > 2 {
            self.toasts.remove(0);
        }
    }

    /// Localised string for the app.
    pub fn text(&self, text: Text) -> &'static str {
        self.t(match text {
            Text::PeerInterrupted => S::PeerInterrupted,
            Text::PeerLeft => S::PeerLeft,
            Text::Desync => S::Desync,
            Text::StateSaved => S::StateSaved,
            Text::StateLoaded => S::StateLoaded,
            Text::CantOpenRomOnline => S::NoRomOnline,
            Text::NoQuickSave => S::NoQuickSave,
            Text::StateWrongRom => S::StateWrongRom,
            Text::StateOldVersion => S::StateOldVersion,
            Text::StateCorrupt => S::StateCorrupt,
            Text::StateSaveFailed => S::StateSaveFailed,
            Text::SleepGap => S::SleepGap,
            Text::ScreenshotSaved => S::ScreenshotSaved,
            Text::ScreenshotFailed => S::ScreenshotFailed,
            Text::UpdateMoveToApps => S::UpdateMoveToApps,
            Text::UpdateRerunInstaller => S::UpdateRerunInstaller,
            Text::DeckKeys => S::DeckKeys,
            Text::DeckKeysFix => S::DeckKeysFix,
        })
    }

    /// Result of the network check started by `UiAction::RunNetCheck`.
    pub fn set_net_diag(&mut self, diag: NetDiag) {
        self.diag = Some(diag);
        self.diag_running = false;
    }

    pub fn update(&mut self, input: &InputFrame, ctx: &mut UiCtx, dt: Duration) -> Option<UiAction> {
        self.clock += dt.as_secs_f32();
        // A gamepad press or a tap opens the next text field with the built-in keyboard,
        // Enter on a physical keyboard without it.
        if input.pad.used || input.pointer.iter().any(|e| matches!(e, PointerEvent::Up(_))) {
            self.osk_wanted = true;
        } else if input.nav.confirm {
            self.osk_wanted = false;
        }
        // Taps and clicks become the button they stand for, so every screen handles them
        // exactly like the keyboard/gamepad.
        let tapped = self.pointer(input, ctx);
        let synth;
        let input = if tapped == Nav::default() {
            input
        } else {
            let n = input.nav;
            let nav = Nav {
                up: n.up || tapped.up,
                down: n.down || tapped.down,
                left: n.left || tapped.left,
                right: n.right || tapped.right,
                confirm: n.confirm || tapped.confirm,
                back: n.back || tapped.back,
            };
            synth = InputFrame { nav, ..input.clone() };
            &synth
        };
        let typed;
        let input = match self.osk_frame(input) {
            Some(f) => {
                typed = f;
                &typed
            }
            None => input,
        };
        let nav = input.nav;
        if let Some(modal) = &mut self.modal {
            if modal.confirm.is_some() {
                if nav.up || nav.down {
                    modal.sel ^= 1;
                }
                if nav.back {
                    self.modal = None;
                } else if nav.confirm {
                    let modal = self.modal.take()?;
                    if modal.sel == 0 {
                        return modal.confirm;
                    }
                }
            } else if nav.confirm || nav.back {
                self.modal = None;
            }
            return None;
        }

        let top = *self.stack.last()?;
        match top {
            Screen::Main => {
                let i = self.menu_step(top, MAIN_ITEMS.len(), &nav)?;
                match MAIN_ITEMS[i] {
                    S::CreateRoom => self.push(Screen::Picker(Purpose::Host)),
                    S::JoinByCode => self.open_code(),
                    S::PlaySolo => self.push(Screen::Picker(Purpose::Solo)),
                    S::ByIp => self.push(Screen::ByIp),
                    S::Settings => self.push(Screen::Settings),
                    _ => return Some(UiAction::Quit),
                }
                None
            }
            Screen::Picker(purpose) => self.update_picker(purpose, input, ctx),
            Screen::Code => self.update_code(input),
            Screen::ByIp => {
                if nav.back {
                    self.pop();
                    return None;
                }
                match self.menu_step(top, 2, &nav)? {
                    0 => self.push(Screen::Picker(Purpose::Direct)),
                    _ => self.open_field(Field::Addr, ctx.config.last_join_addr.clone()),
                }
                None
            }
            Screen::Field(field) => self.update_field(field, input, ctx),
            Screen::Settings => self.update_settings(&nav, ctx),
            Screen::Advanced => self.update_advanced(&nav, ctx),
            Screen::NetCheck => self.update_net_check(&nav),
            Screen::Lobby => {
                if nav.back {
                    self.ask_leave(S::LeaveRoom, UiAction::LeaveLobby);
                    return None;
                }
                let lobby = ctx.lobby?;
                (nav.confirm && lobby.is_host && lobby.stage == Stage::Lobby && lobby.can_start).then_some(UiAction::StartLobbyGame)
            }
            Screen::Pause => {
                if nav.back || input.menu {
                    self.hide();
                    return Some(UiAction::Resume);
                }
                let netplay = ctx.game.as_ref().is_some_and(|g| g.netplay);
                let items: &[S] = if netplay { &PAUSE_NET } else { &PAUSE_SOLO };
                let sel = self.sel[top.slot()].min(items.len() - 1);
                if items[sel] == S::Slot && (nav.left || nav.right || nav.confirm) {
                    let (slot, _) = ctx.game.as_ref().and_then(|g| g.save_slot)?;
                    let n = crate::app::SAVE_SLOTS;
                    return Some(UiAction::SelectSlot(if nav.left { (slot + n - 1) % n } else { (slot + 1) % n }));
                }
                let action = match items[self.menu_step(top, items.len(), &nav)?] {
                    S::Resume => {
                        self.hide();
                        UiAction::Resume
                    }
                    S::SaveState => UiAction::SaveState,
                    S::LoadState => UiAction::LoadState,
                    S::Reset | S::ResetAll => UiAction::Reset,
                    _ if netplay => {
                        self.ask_leave(S::LeaveGameQ, UiAction::LeaveGame);
                        return None;
                    }
                    _ => UiAction::LeaveGame,
                };
                Some(action)
            }
        }
    }

    /// Draws visible menus, the OSD (netplay ping/slots, notices, Back+Start hold ring) and
    /// toasts on top of whatever is already on `canvas`.
    pub fn draw(&mut self, canvas: &mut Canvas, font: &mut Font, ctx: &UiCtx, input: &InputFrame) {
        let top = self.stack.last().copied();
        let visible = self.is_visible();
        if visible {
            if ctx.game.is_some() {
                canvas.dim(0.65);
            } else {
                canvas.clear(BG);
            }
        } else if ctx.game.as_ref().is_some_and(|g| g.stalled) {
            // Waiting for a peer: a light dim so the frozen picture reads as "paused by the net".
            canvas.dim(0.3);
        }
        // Rebuilt every frame (the buffer is reused); pointer input reads the last frame's.
        let mut h = std::mem::take(&mut self.regions);
        h.clear();
        match top {
            Some(Screen::Main) => self.draw_main(canvas, font, ctx, &mut h),
            Some(Screen::Picker(p)) => self.draw_picker(canvas, font, ctx, p, &mut h),
            Some(Screen::Code) => self.draw_code(canvas, font, &mut h),
            Some(Screen::ByIp) => self.draw_by_ip(canvas, font, ctx, &mut h),
            Some(Screen::Field(f)) => self.draw_field(canvas, font, f),
            Some(Screen::Settings) => self.draw_settings(canvas, font, ctx, input.hint_kind, &mut h),
            Some(Screen::Advanced) => self.draw_advanced(canvas, font, ctx, &mut h),
            Some(Screen::NetCheck) => self.draw_net_check(canvas, font),
            Some(Screen::Lobby) => self.draw_lobby(canvas, font, ctx, &mut h),
            Some(Screen::Pause) => self.draw_pause(canvas, font, ctx, input.hint_kind, &mut h),
            None => {
                if let Some(game) = &ctx.game {
                    self.draw_osd(canvas, font, game);
                }
                if input.menu_hold > 0.0 {
                    self.draw_hold(canvas, font, input.menu_hold);
                }
            }
        }
        let osk_shown = visible && self.osk_shown();
        if let (true, Some(k)) = (osk_shown, &self.osk) {
            k.draw(canvas, font);
            h.extend(k.key_rects().map(|(i, r)| (Area { x: r.x, y: r.y, w: r.w, h: r.h }, Hit::Osk(i))));
        }
        if let Some(modal) = &self.modal {
            // Only the modal takes taps while it is open.
            h.clear();
            self.draw_modal(canvas, font, modal, &mut h);
        }
        // The keyboard covers the hint bar (its keys say the same) and the toast line.
        self.draw_toasts(canvas, font, if osk_shown { osk::TOP - 16 } else if visible { 214 } else { 224 });
        if visible && !osk_shown {
            self.draw_hints(canvas, font, ctx, input.hint_kind, top, &mut h);
        }
        self.regions = h;
    }

    // -----------------------------------------------------------------------------------------
    // Update helpers

    fn t(&self, s: S) -> &'static str {
        self.lang.tr(s)
    }

    /// Age of a save for the pause menu ("5 min ago"); under a minute counts as 1 min.
    fn save_age(&self, at: SystemTime, now: SystemTime) -> String {
        let mins = (now.duration_since(at).map_or(0, |d| d.as_secs()) / 60).max(1);
        let (s, n) = match mins {
            ..60 => (S::MinAgo, mins),
            60..1440 => (S::HoursAgo, mins / 60),
            _ => (S::DaysAgo, mins / 1440),
        };
        self.t(s).replace("{n}", &n.to_string())
    }

    fn push(&mut self, screen: Screen) {
        self.stack.push(screen);
        self.press = None;
    }

    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.press = None;
        }
    }

    /// Topmost hit region of the last drawn frame under (x, y).
    fn hit_at(&self, x: i32, y: i32) -> Option<Hit> {
        self.regions.iter().rev().find(|(a, _)| a.contains(x, y)).map(|(_, hit)| *hit)
    }

    /// Mouse/touch on the regions of the last drawn frame: a press selects what it lands on,
    /// a release on the same region returns the button it stands for, a release elsewhere
    /// cancels. On the ROM list a drag or the wheel scrolls.
    fn pointer(&mut self, input: &InputFrame, ctx: &UiCtx) -> Nav {
        let picker = match self.stack.last() {
            Some(&Screen::Picker(p)) if self.modal.is_none() => Some(p),
            _ => None,
        };
        let mut scroll = 0;
        let mut out = Nav::default();
        for ev in &input.pointer {
            match *ev {
                PointerEvent::Down(Some((x, y))) => {
                    self.press = self.hit_at(x, y).map(|hit| Press { hit, y0: y, dragging: false, scrolled: 0 });
                    if let Some(p) = self.press {
                        self.select(p.hit);
                    }
                }
                PointerEvent::Down(None) => {
                    self.press = None;
                    self.release_osk();
                }
                PointerEvent::Move(Some((_, y))) => {
                    let Some(p) = self.press.as_mut().filter(|_| picker.is_some()) else { continue };
                    if !p.dragging && (y - p.y0).abs() >= DRAG_START {
                        p.dragging = true;
                    }
                    if p.dragging {
                        // Content follows the finger: dragging up shows later rows.
                        let rows = (p.y0 - y) / PICKER_STEP;
                        scroll += rows - p.scrolled;
                        p.scrolled = rows;
                    }
                }
                PointerEvent::Move(None) => {}
                PointerEvent::Up(pos) => {
                    self.release_osk();
                    let Some(p) = self.press.take() else { continue };
                    let Some((x, y)) = pos.filter(|_| !p.dragging) else { continue };
                    if self.hit_at(x, y) == Some(p.hit) {
                        if let Hit::Osk(i) = p.hit {
                            self.osk_tap = Some(i);
                        }
                        out = self.activate(p.hit, x);
                        // The screen may change; later events would hit stale regions.
                        break;
                    }
                }
                PointerEvent::Wheel(dy) => scroll -= dy,
            }
        }
        if let (Some(purpose), true) = (picker, scroll != 0) {
            self.scroll_picker(purpose, scroll, ctx.library.len());
        }
        out
    }

    /// Press feedback: the pressed row or cell becomes the selection right away.
    fn select(&mut self, hit: Hit) {
        match hit {
            Hit::Row { i, enabled: true, .. } => {
                if let Some(top) = self.stack.last() {
                    self.sel[top.slot()] = i;
                }
            }
            Hit::Modal(i) => {
                if let Some(m) = &mut self.modal {
                    m.sel = i;
                }
            }
            Hit::Cell(i) => self.code_cur = i,
            Hit::Osk(i) => {
                if let Some(k) = &mut self.osk {
                    k.set_pressed(Some(i));
                }
            }
            Hit::Row { .. } | Hit::Key(_) => {}
        }
    }

    /// The button a completed tap at column `x` stands for.
    fn activate(&self, hit: Hit, x: i32) -> Nav {
        match hit {
            Hit::Row { enabled: false, .. } | Hit::Cell(_) | Hit::Osk(_) => Nav::default(),
            Hit::Row { back: Some((from, to)), .. } => (if (from..to).contains(&x) { Key::Left } else { Key::Right }).nav(),
            Hit::Row { .. } | Hit::Modal(_) => Key::Confirm.nav(),
            Hit::Key(k) => k.nav(),
        }
    }

    /// Scrolls the ROM list by `rows` (positive = later rows), keeping the selection on screen.
    fn scroll_picker(&mut self, purpose: Purpose, rows: i32, n: usize) {
        let max_top = (n + 1).saturating_sub(PICKER_ROWS) as i32;
        self.picker_top = (self.picker_top as i32 + rows).clamp(0, max_top) as usize;
        let sel = &mut self.sel[Screen::Picker(purpose).slot()];
        *sel = (*sel).clamp(self.picker_top, self.picker_top + PICKER_ROWS - 1).min(n);
    }

    /// Up/down with wrap on the remembered selection; `Some(index)` on confirm.
    fn menu_step(&mut self, screen: Screen, len: usize, nav: &Nav) -> Option<usize> {
        let sel = &mut self.sel[screen.slot()];
        *sel = (*sel).min(len.saturating_sub(1));
        if nav.up {
            *sel = (*sel + len - 1) % len;
        }
        if nav.down {
            *sel = (*sel + 1) % len;
        }
        nav.confirm.then_some(*sel)
    }

    /// Leave / Stay confirmation; "Stay" is preselected so a stray confirm is harmless.
    fn ask_leave(&mut self, title: S, action: UiAction) {
        self.modal = Some(Modal { title: self.t(title).to_string(), lines: Vec::new(), confirm: Some(action), sel: 1 });
    }

    fn open_code(&mut self) {
        self.code = [None; CODE_LEN];
        self.code_cur = 0;
        if let Some(last) = &self.last_code {
            for (cell, ch) in self.code.iter_mut().zip(last.bytes()) {
                *cell = CODE_ALPHABET.iter().position(|&b| b == ch).map(|i| i as u8);
            }
            self.code_cur = CODE_LEN - 1;
        }
        self.push(Screen::Code);
        self.open_osk(Charset::RoomCode);
    }

    fn open_field(&mut self, field: Field, prefill: String) {
        self.field = prefill;
        self.push(Screen::Field(field));
        self.open_osk(match field {
            Field::Addr => Charset::Address,
            Field::Nick => Charset::Nickname,
            Field::Server => Charset::Url,
        });
    }

    /// The built-in keyboard shows on the Deck always, elsewhere when the field was opened
    /// with a gamepad or a tap.
    fn open_osk(&mut self, charset: Charset) {
        self.osk_tap = None;
        self.osk = (is_steam_deck() || self.osk_wanted).then(|| Osk::new(charset));
    }

    fn osk_shown(&self) -> bool {
        self.osk.is_some() && self.wants_text_input()
    }

    fn release_osk(&mut self) {
        if let Some(k) = &mut self.osk {
            k.set_pressed(None);
        }
    }

    /// While the keyboard shows, turns gamepad and tap input into the text-field input the
    /// field screens already take: d-pad moves on the keyboard, A presses, B deletes (leaves
    /// an empty field), Y shift, X layout, Start OK. Physical keys keep working; typing on a
    /// physical keyboard hides the built-in one for this field.
    fn osk_frame(&mut self, input: &InputFrame) -> Option<InputFrame> {
        let tap = self.osk_tap.take();
        if !self.osk_shown() || self.modal.is_some() {
            return None;
        }
        if !input.text.is_empty() && !input.pad.used {
            self.osk = None;
            return None;
        }
        let empty = match self.stack.last() {
            Some(Screen::Code) => self.code.iter().all(Option::is_none),
            _ => self.field.is_empty(),
        };
        let k = self.osk.as_mut()?;
        let (n, pad) = (input.nav, input.pad);
        let mut out = Vec::new();
        let moves = [
            (n.up, OskNav::Up),
            (n.down, OskNav::Down),
            (n.left, OskNav::Left),
            (n.right, OskNav::Right),
            (pad.used && n.confirm, OskNav::Press),
            (pad.shift, OskNav::Shift),
            (pad.layout, OskNav::Layout),
            (pad.start, OskNav::Done),
        ];
        for (on, m) in moves {
            if on {
                out.extend(k.nav(m));
            }
        }
        if let Some(i) = tap {
            out.extend(k.press_key(i));
        }
        let mut f = InputFrame { nav: Nav { back: n.back, confirm: n.confirm && !pad.used, ..Nav::default() }, ..input.clone() };
        if pad.used && n.back && !input.backspace && !empty {
            f.nav.back = false;
            out.extend(k.nav(OskNav::Backspace));
        }
        for o in out {
            match o {
                OskOutput::Char(c) => f.text.push(c),
                OskOutput::Backspace => f.backspace = true,
                OskOutput::Done => f.nav.confirm = true,
            }
        }
        Some(f)
    }

    fn update_picker(&mut self, purpose: Purpose, input: &InputFrame, ctx: &UiCtx) -> Option<UiAction> {
        let nav = input.nav;
        if nav.back {
            self.pop();
            return None;
        }
        let lib = ctx.library;
        let n = lib.len();
        let total = n + 1; // last row: Rescan
        let selectable = |i: usize| i == n || lib[i].supported;
        let slot = Screen::Picker(purpose).slot();
        let mut sel = self.sel[slot].min(n);
        let step = |from: usize, fwd: bool| -> usize {
            let mut i = from;
            for _ in 0..total {
                i = if fwd { (i + 1) % total } else { (i + total - 1) % total };
                if selectable(i) {
                    return i;
                }
            }
            from
        };
        if !selectable(sel) {
            sel = step(sel, true);
        }
        if nav.up {
            sel = step(sel, false);
        }
        if nav.down {
            sel = step(sel, true);
        }
        if nav.left || nav.right {
            let target = if nav.left { sel.saturating_sub(PICKER_ROWS) } else { (sel + PICKER_ROWS).min(n) };
            sel = if selectable(target) { target } else { step(target, nav.right) };
        }
        self.sel[slot] = sel;
        if sel < self.picker_top {
            self.picker_top = sel;
        } else if sel >= self.picker_top + PICKER_ROWS {
            self.picker_top = sel + 1 - PICKER_ROWS;
        }
        self.picker_top = self.picker_top.min(total.saturating_sub(PICKER_ROWS));
        if !nav.confirm {
            return None;
        }
        if sel == n {
            return Some(UiAction::Rescan);
        }
        let entry = lib[sel].clone();
        Some(match purpose {
            Purpose::Solo => UiAction::PlaySolo(entry),
            Purpose::Host => UiAction::HostRoom(entry),
            Purpose::Direct => UiAction::HostDirect(entry),
        })
    }

    fn put_code_char(&mut self, c: char) {
        let c = c.to_ascii_uppercase();
        if let Some(idx) = CODE_ALPHABET.iter().position(|&b| b as char == c) {
            self.code[self.code_cur] = Some(idx as u8);
            self.code_cur = (self.code_cur + 1).min(CODE_LEN - 1);
        }
    }

    fn update_code(&mut self, input: &InputFrame) -> Option<UiAction> {
        let nav = input.nav;
        if input.backspace {
            if self.code[self.code_cur].is_some() {
                self.code[self.code_cur] = None;
            } else if self.code_cur > 0 {
                self.code_cur -= 1;
                self.code[self.code_cur] = None;
            }
        } else if nav.back {
            self.pop();
            return None;
        }
        if let Some(paste) = &input.paste {
            match normalize_code(paste) {
                Some(code) => {
                    self.code_cur = 0;
                    code.chars().for_each(|c| self.put_code_char(c));
                }
                None => paste.chars().for_each(|c| self.put_code_char(c)),
            }
        }
        input.text.chars().for_each(|c| self.put_code_char(c));
        let len = CODE_ALPHABET.len() as u8;
        if nav.up || nav.down {
            let cur = &mut self.code[self.code_cur];
            *cur = Some(match (*cur, nav.up) {
                (None, true) => 0,
                (None, false) => len - 1,
                (Some(i), true) => (i + 1) % len,
                (Some(i), false) => (i + len - 1) % len,
            });
        }
        if nav.left {
            self.code_cur = self.code_cur.saturating_sub(1);
        }
        if nav.right {
            self.code_cur = (self.code_cur + 1).min(CODE_LEN - 1);
        }
        if nav.confirm && self.code.iter().all(Option::is_some) {
            let code = self.code_string();
            self.last_code = Some(code.clone());
            return Some(UiAction::JoinRoom(code));
        }
        None
    }

    fn code_string(&self) -> String {
        self.code.iter().map(|c| c.map_or('_', |i| CODE_ALPHABET[i as usize] as char)).collect()
    }

    fn update_field(&mut self, field: Field, input: &InputFrame, ctx: &mut UiCtx) -> Option<UiAction> {
        let nav = input.nav;
        if input.backspace {
            self.field.pop();
        } else if nav.back {
            self.pop();
            return None;
        }
        let max = if field == Field::Nick { NICK_MAX } else { 200 };
        for c in input.paste.iter().flat_map(|p| p.chars()).chain(input.text.chars()) {
            if !c.is_control() && self.field.chars().count() < max {
                self.field.push(c);
            }
        }
        if !nav.confirm {
            return None;
        }
        let value = self.field.trim().to_string();
        match field {
            Field::Addr => {
                if value.is_empty() {
                    return None;
                }
                ctx.config.last_join_addr = value.clone();
                Some(UiAction::JoinDirect(value))
            }
            Field::Nick | Field::Server => {
                if !value.is_empty() {
                    if field == Field::Nick {
                        ctx.config.nickname = value;
                    } else {
                        ctx.config.server = value;
                    }
                }
                self.pop();
                Some(UiAction::ConfigChanged)
            }
        }
    }

    fn settings_items(&self, cfg: &Config) -> Vec<Item> {
        let onoff = |b: bool| self.t(if b { S::On } else { S::Off }).to_string();
        vec![
            Item::value(self.t(S::Nick), trunc(&cfg.nickname, 11), false),
            Item::value(self.t(S::Fullscreen), onoff(cfg.fullscreen), true),
            Item::value(self.t(S::Scale), format!("{}x", cfg.scale), true),
            Item::value(
                self.t(S::ScreenMode),
                self.t(match cfg.screen {
                    ScreenMode::Stretch => S::ScreenStretch,
                    ScreenMode::Fit => S::ScreenFit,
                    ScreenMode::Pixel => S::ScreenPixel,
                })
                .to_string(),
                true,
            ),
            Item::value(
                self.t(S::Effect),
                self.t(match cfg.effect {
                    Effect::Off => S::EffectOff,
                    Effect::CrtTv => S::EffectCrtTv,
                    Effect::Pvm => S::EffectPvm,
                    Effect::Lcd => S::EffectLcd,
                })
                .to_string(),
                true,
            ),
            Item::value(self.t(S::Volume), format!("{}%", (cfg.volume * 100.0).round() as u32), true),
            Item::value(self.t(S::Language), if self.lang == Lang::Ru { "Русский" } else { "English" }.to_string(), true),
            Item::new(self.t(S::Advanced)),
        ]
    }

    fn advanced_items(&self, cfg: &Config) -> Vec<Item> {
        let server = cfg.server.trim_start_matches("https://").trim_start_matches("http://");
        let delay = match cfg.input_delay {
            None => self.t(S::Auto).to_string(),
            Some(d) => d.to_string(),
        };
        vec![
            Item::value(self.t(S::Server), trunc(server, 11), false),
            Item::value(self.t(S::ForceRelay), self.t(if cfg.force_relay { S::On } else { S::Off }).to_string(), true),
            Item::new(self.t(S::NetCheck)),
            Item::value(self.t(S::FastForward), self.t(if cfg.ff_toggle { S::FfToggle } else { S::FfHold }).to_string(), true),
            Item::value(
                self.t(S::SpriteLimit),
                self.t(match cfg.sprite_limit {
                    SpriteLimit::Auto => S::Auto,
                    SpriteLimit::Hardware => S::SpriteLimitNes,
                    SpriteLimit::Off => S::SpriteLimitRemove,
                })
                .to_string(),
                true,
            ),
            Item::value(self.t(S::AutoUpdate), self.t(if cfg.auto_update { S::On } else { S::Off }).to_string(), true),
            Item::value(self.t(S::InputDelay), delay, true),
        ]
    }

    /// Selected row is a text field (opens an editor on confirm).
    fn on_text_row(&self, screen: Screen) -> bool {
        let row = self.sel[screen.slot()];
        match screen {
            Screen::Settings => row == 0,
            Screen::Advanced => row == 0,
            _ => false,
        }
    }

    fn update_settings(&mut self, nav: &Nav, ctx: &mut UiCtx) -> Option<UiAction> {
        if nav.back {
            self.pop();
            return None;
        }
        let confirm = self.menu_step(Screen::Settings, 8, nav);
        let row = self.sel[Screen::Settings.slot()];
        let delta: i32 = if nav.right || confirm.is_some() { 1 } else if nav.left { -1 } else { 0 };
        let cfg = &mut *ctx.config;
        match row {
            0 if confirm.is_some() => {
                let nick = cfg.nickname.clone();
                self.open_field(Field::Nick, nick);
                None
            }
            7 if confirm.is_some() => {
                self.sel[Screen::Advanced.slot()] = 0;
                self.push(Screen::Advanced);
                None
            }
            0 | 7 => None,
            _ if delta == 0 => None,
            1 => {
                cfg.fullscreen = !cfg.fullscreen;
                Some(UiAction::ConfigChanged)
            }
            2 => {
                let scale = (cfg.scale as i32 + delta).clamp(1, 6) as u32;
                let changed = scale != cfg.scale;
                cfg.scale = scale;
                changed.then_some(UiAction::ConfigChanged)
            }
            3 => {
                const ORDER: [ScreenMode; 3] = [ScreenMode::Stretch, ScreenMode::Fit, ScreenMode::Pixel];
                let i = ORDER.iter().position(|m| *m == cfg.screen).unwrap_or(0) as i32;
                cfg.screen = ORDER[(i + delta).rem_euclid(3) as usize];
                Some(UiAction::ConfigChanged)
            }
            4 => {
                cfg.effect = if delta > 0 { cfg.effect.next() } else { cfg.effect.prev() };
                Some(UiAction::ConfigChanged)
            }
            5 => {
                let steps = ((cfg.volume * 10.0).round() as i32 + delta).clamp(0, 10);
                let volume = steps as f32 / 10.0;
                let changed = volume != cfg.volume;
                cfg.volume = volume;
                changed.then_some(UiAction::ConfigChanged)
            }
            _ => {
                self.lang = if self.lang == Lang::Ru { Lang::En } else { Lang::Ru };
                cfg.lang = Some(self.lang.code().to_string());
                Some(UiAction::ConfigChanged)
            }
        }
    }

    fn update_advanced(&mut self, nav: &Nav, ctx: &mut UiCtx) -> Option<UiAction> {
        if nav.back {
            self.pop();
            return None;
        }
        let confirm = self.menu_step(Screen::Advanced, 7, nav);
        let cfg = &mut *ctx.config;
        match self.sel[Screen::Advanced.slot()] {
            0 => {
                if confirm.is_some() {
                    let server = cfg.server.clone();
                    self.open_field(Field::Server, server);
                }
                None
            }
            1 if confirm.is_some() || nav.left || nav.right => {
                cfg.force_relay = !cfg.force_relay;
                Some(UiAction::ConfigChanged)
            }
            2 if confirm.is_some() => {
                self.push(Screen::NetCheck);
                self.start_net_check()
            }
            3 if confirm.is_some() || nav.left || nav.right => {
                cfg.ff_toggle = !cfg.ff_toggle;
                Some(UiAction::ConfigChanged)
            }
            4 if confirm.is_some() || nav.left || nav.right => {
                const ORDER: [SpriteLimit; 3] = [SpriteLimit::Auto, SpriteLimit::Hardware, SpriteLimit::Off];
                let i = ORDER.iter().position(|v| *v == cfg.sprite_limit).unwrap_or(0) as i32;
                let delta = if nav.left { -1 } else { 1 };
                cfg.sprite_limit = ORDER[(i + delta).rem_euclid(3) as usize];
                Some(UiAction::ConfigChanged)
            }
            5 if confirm.is_some() || nav.left || nav.right => {
                cfg.auto_update = !cfg.auto_update;
                Some(UiAction::ConfigChanged)
            }
            6 if confirm.is_some() || nav.left || nav.right => {
                let i = DELAYS.iter().position(|d| *d == cfg.input_delay).unwrap_or(0) as i32;
                let delta = if nav.left { -1 } else { 1 };
                cfg.input_delay = DELAYS[(i + delta).rem_euclid(DELAYS.len() as i32) as usize];
                Some(UiAction::ConfigChanged)
            }
            _ => None,
        }
    }

    /// Marks a check as running; `None` when one already runs (its result will show).
    fn start_net_check(&mut self) -> Option<UiAction> {
        if self.diag_running {
            return None;
        }
        self.diag_running = true;
        Some(UiAction::RunNetCheck)
    }

    fn update_net_check(&mut self, nav: &Nav) -> Option<UiAction> {
        if nav.back {
            self.pop();
            return None;
        }
        if nav.confirm {
            return self.start_net_check();
        }
        None
    }

    fn lobby_block(&self, lobby: &LobbyStatus) -> Option<S> {
        if lobby.can_start {
            None
        } else if lobby.players.len() < 2 {
            Some(S::NeedPlayer)
        } else if lobby.players.iter().any(|p| p.rom != RomState::Have) {
            Some(S::NeedRom)
        } else {
            Some(S::NeedReady)
        }
    }

    // -----------------------------------------------------------------------------------------
    // Drawing

    fn ticker(&self) -> &'static str {
        ["", ".", "..", "..."][(self.clock * 3.0) as usize % 4]
    }

    /// Menu rows from `y0`, `step` px apart; selected row gets the accent bar and `>`. Each row
    /// becomes a full-width hit region at least `MIN_TAP` tall.
    fn draw_rows(c: &mut Canvas, f: &mut Font, items: &[Item], sel: usize, y0: i32, step: i32, h: &mut Vec<(Area, Hit)>) {
        for (i, it) in items.iter().enumerate() {
            let y = y0 + i as i32 * step;
            let selected = i == sel;
            if selected {
                c.fill_rect(8, y - (step - 8) / 2, 240, step, if it.disabled { DIS_BAR } else { ACCENT });
                f.draw(c, 12, y, ">", WHITE, 1);
            }
            let col = match (it.disabled, selected) {
                (true, true) => SEC,
                (true, false) => DIS,
                _ => WHITE,
            };
            f.draw(c, 24, y, &it.label, col, 1);
            let mut back = None;
            if let Some(v) = &it.value {
                let vx = 236 - f.width(v, 1);
                f.draw(c, vx, y, v, if selected { WHITE } else { SEC }, 1);
                if it.arrows && selected {
                    tri(c, vx - 7, y, WHITE, false);
                    tri(c, 240, y, WHITE, true);
                }
                if it.arrows {
                    // Value area ◀ value ▶: its left third (at least MIN_TAP wide) steps back,
                    // the rest of the row forward.
                    let left = vx - 8;
                    back = Some((left, left + ((248 - left) / 3).max(MIN_TAP)));
                }
            }
            h.push((row_area(y, step), Hit::Row { i, back, enabled: !it.disabled }));
        }
    }

    fn draw_main(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, h: &mut Vec<(Area, Hit)>) {
        f.draw_centered(c, 32, "EBOBO", ACCENT, 2);
        f.draw_centered(c, 56, &format!("v{}", ctx.version), SEC, 1);
        if let Some(v) = &self.update {
            f.draw_centered(c, 70, &self.t(S::UpdatingTo).replace("{name}", v), ACCENT, 1);
        }
        let items: Vec<Item> = MAIN_ITEMS.iter().map(|s| Item::new(self.t(*s))).collect();
        Self::draw_rows(c, f, &items, self.sel[Screen::Main.slot()], 88, 16, h);
    }

    fn draw_picker(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, purpose: Purpose, h: &mut Vec<(Area, Hit)>) {
        let title = self.t(match purpose {
            Purpose::Solo => S::PickRomSolo,
            Purpose::Host => S::PickRomHost,
            Purpose::Direct => S::PickRomDirect,
        });
        f.draw(c, 8, 8, title, WHITE, 1);
        let lib = ctx.library;
        let n = lib.len();
        let count = format!("{}: {}", self.t(S::Found), n);
        f.draw(c, 248 - f.width(&count, 1), 8, &count, if ctx.scanning { WARN } else { SEC }, 1);
        c.fill_rect(8, 18, 240, 1, DIS_BAR);
        let sel = self.sel[Screen::Picker(purpose).slot()].min(n);
        if n == 0 {
            let lines = [
                (S::NoRoms, WHITE),
                (S::PutRomsIn, SEC),
                (S::RomsHome, SEC),
                (S::RomsDeck, SEC),
                (S::RomsDownloads, SEC),
            ];
            for (i, (s, col)) in lines.iter().enumerate() {
                f.draw(c, 16, 32 + i as i32 * 14 + if i > 1 { 4 } else { 0 }, self.t(*s), *col, 1);
            }
            if ctx.scanning {
                let base = self.t(S::Scanning);
                f.draw(c, 16, 112, &format!("{base}{}", self.ticker()), WARN, 1);
            }
            Self::draw_rows(c, f, &[Item::new(self.t(S::Rescan))], 0, 136, 16, h);
            return;
        }
        let top = self.picker_top.min(n);
        for row in 0..PICKER_ROWS {
            let i = top + row;
            if i > n {
                break;
            }
            let y = 26 + row as i32 * PICKER_STEP;
            let selected = i == sel;
            if selected {
                c.fill_rect(8, y - 3, 240, PICKER_STEP, ACCENT);
                f.draw(c, 10, y, ">", WHITE, 1);
            }
            let enabled = i == n || lib[i].supported;
            h.push((row_area(y, PICKER_STEP), Hit::Row { i, back: None, enabled }));
            if i == n {
                f.draw(c, 20, y, self.t(S::Rescan), if selected { WHITE } else { SEC }, 1);
                continue;
            }
            let e = &lib[i];
            let (tag, tag_col) = if !e.supported {
                (format!("{} {}", self.t(S::Unsupported), e.mapper), DIS)
            } else if e.region == ebobo_core::Region::Pal {
                ("PAL".to_string(), if selected { WHITE } else { WARN })
            } else {
                (String::new(), SEC)
            };
            let tag_w = f.width(&tag, 1);
            let cols = ((244 - tag_w - 20 - if tag_w > 0 { 8 } else { 0 }) / 8) as usize;
            let col = if !e.supported { DIS } else { WHITE };
            f.draw(c, 20, y, &trunc(&e.name, cols), col, 1);
            if tag_w > 0 {
                f.draw(c, 244 - tag_w, y, &tag, tag_col, 1);
            }
        }
        if n + 1 > PICKER_ROWS {
            // Scroll position, 1 px track on the right edge.
            let track = PICKER_ROWS as i32 * PICKER_STEP;
            let thumb = (track * PICKER_ROWS as i32 / (n as i32 + 1)).max(4);
            let y = 24 + (track - thumb) * top as i32 / (n + 1 - PICKER_ROWS) as i32;
            c.fill_rect(250, 23, 1, track, DIS_BAR);
            c.fill_rect(250, y - 1, 1, thumb, SEC);
        }
    }

    fn draw_code(&self, c: &mut Canvas, f: &mut Font, h: &mut Vec<(Area, Hit)>) {
        f.draw_centered(c, 32, self.t(S::EnterCode), WHITE, 1);
        f.draw_centered(c, 48, self.t(S::CodeHint), SEC, 1);
        let osk = self.osk_shown();
        let (bw, gap) = (28, 8);
        let x0 = (256 - (bw * CODE_LEN as i32 + gap * (CODE_LEN as i32 - 1))) / 2;
        // With the keyboard the strip moves up clear of it and the letter arrows go.
        let y = if osk { 76 } else { 84 };
        for i in 0..CODE_LEN {
            let x = x0 + i as i32 * (bw + gap);
            // Cells grow into the gaps so the whole strip takes taps.
            h.push((Area { x: x - gap / 2, y, w: bw + gap, h: 32 }, Hit::Cell(i)));
            c.fill_rect(x, y, bw, 32, PANEL);
            if i == self.code_cur {
                c.rect(x, y, bw, 32, ACCENT);
                c.rect(x + 1, y + 1, bw - 2, 30, ACCENT);
                if !osk {
                    up_tri(c, x + bw / 2 - 3, y - 8, SEC, true);
                    up_tri(c, x + bw / 2 - 3, y + 36, SEC, false);
                    h.push((Area { x: x - gap / 2, y: y - 16, w: bw + gap, h: 16 }, Hit::Key(Key::Up)));
                    h.push((Area { x: x - gap / 2, y: y + 32, w: bw + gap, h: 16 }, Hit::Key(Key::Down)));
                }
            } else {
                c.rect(x, y, bw, 32, DIS);
            }
            if let Some(ch) = self.code[i] {
                let s = (CODE_ALPHABET[ch as usize] as char).to_string();
                f.draw(c, x + (bw - 16) / 2, y + 8, &s, WHITE, 2);
            }
        }
        if !osk {
            let complete = self.code.iter().all(Option::is_some);
            let mut join = Item::new(self.t(S::Join));
            join.disabled = !complete;
            Self::draw_rows(c, f, &[join], 0, 144, 16, h);
        }
    }

    fn draw_by_ip(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, h: &mut Vec<(Area, Hit)>) {
        f.draw_centered(c, 32, self.t(S::ByIp), WHITE, 1);
        let items = [
            Item::new(&format!("{} ({})", self.t(S::WaitForPlayers), ctx.config.direct_port)),
            Item::new(self.t(S::ConnectTo)),
        ];
        Self::draw_rows(c, f, &items, self.sel[Screen::ByIp.slot()], 72, 16, h);
        for (i, line) in wrap(self.t(S::DirectHint), 28).iter().enumerate() {
            f.draw_centered(c, 120 + i as i32 * 10, line, SEC, 1);
        }
    }

    fn draw_field(&self, c: &mut Canvas, f: &mut Font, field: Field) {
        let title = self.t(match field {
            Field::Addr => S::AddrTitle,
            Field::Nick => S::NickTitle,
            Field::Server => S::ServerTitle,
        });
        f.draw_centered(c, 48, title, WHITE, 1);
        c.fill_rect(8, 70, 240, 20, PANEL);
        c.rect(8, 70, 240, 20, ACCENT);
        // Show the tail when the text is longer than the box (28 columns incl. the caret).
        let chars: Vec<char> = self.field.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(27)..].iter().collect();
        f.draw(c, 16, 76, &shown, WHITE, 1);
        f.draw(c, 16 + f.width(&shown, 1), 76, "_", ACCENT, 1);
    }

    fn draw_settings(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, kind: Option<PadKind>, h: &mut Vec<(Area, Hit)>) {
        f.draw_centered(c, 8, self.t(S::Settings), WHITE, 1);
        let items = self.settings_items(ctx.config);
        Self::draw_rows(c, f, &items, self.sel[Screen::Settings.slot()], 28, 14, h);
        f.draw(c, 16, 146, self.t(S::Gamepads), SEC, 1);
        if ctx.pads.is_empty() {
            f.draw(c, 24, 157, self.t(S::NoPads), DIS, 1);
            return;
        }
        let shown = ctx.pads.len().min(4);
        for (i, pad) in ctx.pads.iter().take(shown).enumerate() {
            let y = 157 + i as i32 * 10;
            match pad.slot {
                Some(s) => f.draw(c, 24, y, &(s + 1).to_string(), SLOT[s.min(3)], 1),
                None => f.draw(c, 24, y, "-", DIS, 1),
            }
            f.draw(c, 40, y, &trunc(&pad.name, 25), WHITE, 1);
        }
        let mut y = 157 + shown as i32 * 10 + 3;
        self.draw_pad_legend(c, f, kind.unwrap_or(ctx.pads[0].kind), &mut y);
        if ctx.pads.len() >= 2 {
            f.draw(c, 16, y, self.t(S::PlayerOrder), SEC, 1);
        }
    }

    /// The fixed gamepad layout in `kind`'s labels at (16, `*y`): face buttons, then
    /// shoulders (turbo) on the same line when they fit, else on the next. Advances `*y`
    /// past the drawn lines.
    fn draw_pad_legend(&self, c: &mut Canvas, f: &mut Font, kind: PadKind, y: &mut i32) {
        let gap = f.width("   ", 1);
        let mut x = 16;
        if kind == PadKind::PlayStation {
            for (a, b, col_a, col_b, label) in [(&PS_CROSS, &PS_TRIANGLE, PS_BLUE, PS_GREEN, " = A"), (&PS_CIRCLE, &PS_SQUARE, PS_RED, PS_PINK, " = B")] {
                bitmap(c, x, *y, a, 8, col_a);
                bitmap(c, x + 10, *y, b, 8, col_b);
                x += 18;
                f.draw(c, x, *y, label, SEC, 1);
                x += f.width(label, 1) + gap;
            }
        } else {
            let face = "A, Y = A   B, X = B";
            f.draw(c, x, *y, face, SEC, 1);
            x += f.width(face, 1) + gap;
        }
        let shoulders = match kind {
            PadKind::Xbox => "LB RB",
            PadKind::Nintendo => "L R",
            _ => "L1 R1",
        };
        let turbo = format!("{shoulders} = {}", self.t(S::Turbo));
        if x + f.width(&turbo, 1) > 248 {
            x = 16;
            *y += 10;
        }
        f.draw(c, x, *y, &turbo, SEC, 1);
        *y += 10;
    }

    fn draw_advanced(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, h: &mut Vec<(Area, Hit)>) {
        f.draw_centered(c, 8, self.t(S::Advanced), WHITE, 1);
        let items = self.advanced_items(ctx.config);
        Self::draw_rows(c, f, &items, self.sel[Screen::Advanced.slot()], 28, 14, h);
    }

    fn draw_lobby(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, h: &mut Vec<(Area, Hit)>) {
        let lobby = ctx.lobby.filter(|l| l.stage != Stage::Connecting);
        let Some(lobby) = lobby else {
            let base = self.t(S::Connecting);
            let x = (256 - f.width(base, 1) - 24) / 2;
            f.draw(c, x, 108, base, WHITE, 1);
            f.draw(c, x + f.width(base, 1), 108, self.ticker(), WHITE, 1);
            return;
        };
        f.draw_centered(c, 8, self.t(S::Room), SEC, 1);
        match &lobby.code {
            Some(code) => {
                f.draw_centered(c, 20, code, WHITE, 2);
                f.draw_centered(c, 40, self.t(S::TellCode), SEC, 1);
            }
            None => f.draw_centered(c, 20, "IP", WHITE, 2),
        }
        for i in 0..4 {
            let y = 58 + i as i32 * 22;
            c.fill_rect(8, y - 4, 240, 20, PANEL);
            f.draw(c, 12, y, &(i + 1).to_string(), SLOT[i], 1);
            let Some(p) = lobby.players.get(i) else {
                f.draw(c, 28, y, self.t(S::Waiting), DIS, 1);
                continue;
            };
            let name = if p.is_self { format!("{} ({})", trunc(&p.name, 16), self.t(S::You)) } else { trunc(&p.name, 22) };
            f.draw(c, 28, y, &name, if p.away { SEC } else { WHITE }, 1);
            if p.ready {
                tick(c, 236, y, OK);
            }
            let y2 = y + 9;
            if p.away {
                f.draw(c, 28, y2, self.t(S::Reconnecting), WARN, 1);
                continue;
            }
            if let Some(route) = p.route {
                f.draw(c, 28, y2, if route == RouteKind::Direct { "P2P" } else { "relay" }, SEC, 1);
            }
            if let Some(ms) = p.rtt_ms {
                f.draw(c, 76, y2, &format!("{ms} ms"), ping_color(ms), 1);
            }
            let (rom, col) = match p.rom {
                RomState::Have => ("ROM".to_string(), OK),
                RomState::Receiving { percent } => (format!("ROM {percent}%"), WARN),
                RomState::Missing => ("ROM -".to_string(), ERR),
                RomState::Unknown => ("ROM ?".to_string(), DIS),
            };
            f.draw(c, 148, y2, &rom, col, 1);
            if p.rom == RomState::Have {
                tick(c, 176, y2, OK);
            }
        }
        if let Some(rom) = &lobby.rom_name {
            f.draw_centered(c, 150, &trunc(rom, 30), SEC, 1);
        }
        if lobby.stage == Stage::Starting {
            f.draw_centered(c, 172, &format!("{}...", self.t(S::Starting)), WHITE, 1);
        } else if lobby.is_host {
            let block = self.lobby_block(lobby);
            let mut start = Item::new(self.t(S::Start));
            start.disabled = block.is_some();
            Self::draw_rows(c, f, &[start], 0, 172, 16, h);
            if let Some(reason) = block {
                f.draw_centered(c, 192, self.t(reason), WARN, 1);
            }
        } else {
            f.draw_centered(c, 172, self.t(S::WaitHost), SEC, 1);
        }
    }

    fn draw_pause(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, kind: Option<PadKind>, h: &mut Vec<(Area, Hit)>) {
        f.draw_centered(c, 32, self.t(S::Paused), WHITE, 2);
        let netplay = ctx.game.as_ref().is_some_and(|g| g.netplay);
        if let Some(g) = &ctx.game {
            f.draw_centered(c, 56, &trunc(g.rom_name, 30), SEC, 1);
        }
        if netplay {
            f.draw_centered(c, 72, self.t(S::NetRunning), WARN, 1);
        }
        let list: &[S] = if netplay { &PAUSE_NET } else { &PAUSE_SOLO };
        let items: Vec<Item> = list
            .iter()
            .map(|s| match (s, ctx.game.as_ref().and_then(|g| g.save_slot)) {
                (S::Slot, Some((slot, at))) => {
                    let when = at.map_or_else(|| self.t(S::SlotEmpty).to_string(), |at| self.save_age(at, SystemTime::now()));
                    Item::value(&format!("{} {}", self.t(S::Slot), slot + 1), when, true)
                }
                _ => Item::new(self.t(*s)),
            })
            .collect();
        let y0 = if netplay { 96 } else { 84 };
        Self::draw_rows(c, f, &items, self.sel[Screen::Pause.slot()].min(items.len() - 1), y0, 16, h);
        if !netplay {
            let (lt, rt, shot) = match kind {
                Some(PadKind::PlayStation) => ("L2", "R2", "Share+Triangle"),
                Some(PadKind::Nintendo) => ("ZL", "ZR", "-+X"),
                Some(PadKind::SteamDeck) => ("L2", "R2", "View+Y"),
                Some(_) => ("LT", "RT", "Back+Y"),
                None => ("", "", ""),
            };
            let keys = |key: &str, pad: &str| if pad.is_empty() { key.to_string() } else { format!("{key} / {pad}") };
            let lines = [
                (S::HintRewind, keys("Backspace", lt)),
                (S::HintFastForward, keys("Tab", rt)),
                (S::HintScreenshot, keys("F12", shot)),
            ];
            for (i, (s, k)) in lines.iter().enumerate() {
                f.draw_centered(c, 184 + i as i32 * 12, &self.t(*s).replace("{keys}", k), SEC, 1);
            }
        }
    }

    fn draw_osd(&self, c: &mut Canvas, f: &mut Font, game: &GameView) {
        if let Some(stats) = game.stats.filter(|_| game.netplay) {
            for (i, p) in stats.peers.iter().enumerate() {
                let name = game.names.get(p.slot).map_or(String::new(), |n| trunc(n, 6));
                let slot = (p.slot + 1).to_string();
                let ping = p.ping_ms.to_string();
                let w = f.width(&format!("{slot} {name} {ping}"), 1);
                let (x, y) = (250 - w, 4 + i as i32 * 10);
                c.fill_rect(x - 2, y - 1, w + 4, 10, SHADE);
                f.draw(c, x, y, &slot, SLOT[p.slot.min(3)], 1);
                f.draw(c, x + 16, y, &name, WHITE, 1);
                f.draw(c, x + w - f.width(&ping, 1), y, &ping, ping_color(p.ping_ms), 1);
                if i == 0 {
                    let col = if stats.last_rollback > 0 { WARN } else { OK };
                    c.fill_rect(x - 8, y + 2, 3, 3, col);
                }
            }
        }
        // Waiting for a peer: big and centred near the top so nobody thinks the game froze.
        let mut y = 48;
        if let Some(line) = &game.notice {
            let scale = if f.width(line, 2) <= 232 { 2 } else { 1 };
            let line = trunc(line, 29);
            let w = f.width(&line, scale);
            let h = 8 * scale as i32 + 12;
            c.fill_rect((256 - w) / 2 - 6, y - 6, w + 12, h, PANEL);
            c.rect((256 - w) / 2 - 6, y - 6, w + 12, h, WARN);
            f.draw_centered(c, y, &line, WARN, scale);
            y += h + 6;
        }
        if game.desync {
            let line = trunc(self.t(S::Desync), 30);
            let w = f.width(&line, 1);
            c.fill_rect((256 - w) / 2 - 4, y - 3, w + 8, 14, SHADE);
            f.draw_centered(c, y, &line, ERR, 1);
        }
    }

    /// Settings ▸ Advanced ▸ Network check: five fixed rows (filled in when the check ends, so
    /// nothing moves), the first failure's raw text, and the README's advice for the result.
    fn draw_net_check(&self, c: &mut Canvas, f: &mut Font) {
        f.draw_centered(c, 8, self.t(S::NetCheck), WHITE, 1);
        let done = self.diag.as_ref().filter(|_| !self.diag_running);
        let labels = [S::CheckServer, S::CheckRooms, S::CheckPublic, S::CheckLocal, S::CheckNat];
        let rows = done.map(|d| self.net_check_rows(d));
        for (i, label) in labels.iter().enumerate() {
            let y = 28 + i as i32 * 14;
            f.draw(c, 28, y, self.t(*label), WHITE, 1);
            let Some(rows) = &rows else {
                let dots = self.ticker();
                f.draw(c, 248 - f.width("...", 1), y, dots, DIS, 1);
                continue;
            };
            let (value, col) = &rows[i];
            let value = trunc(value, 20);
            f.draw(c, 248 - f.width(&value, 1), y, &value, *col, 1);
            match *col {
                OK => tick(c, 12, y, OK),
                WARN => f.draw(c, 12, y, "!", WARN, 1),
                _ => cross(c, 12, y, ERR),
            }
        }
        let Some(d) = done else {
            let base = self.t(S::Checking);
            let x = (256 - f.width(base, 1) - 24) / 2;
            f.draw(c, x, 116, base, SEC, 1);
            f.draw(c, x + f.width(base, 1), 116, self.ticker(), SEC, 1);
            return;
        };
        if let Some(err) = d.server.as_ref().err().or(d.websocket.as_ref().err()) {
            f.draw_centered(c, 100, &trunc(err, 30), DIS, 1);
        }
        f.draw(c, 16, 118, self.t(S::WhatToDo), SEC, 1);
        for (i, line) in wrap(self.t(net_advice(d)), 29).iter().take(8).enumerate() {
            f.draw(c, 16, 132 + i as i32 * 10, line, WHITE, 1);
        }
    }

    /// Value text and colour (OK / WARN / ERR) per row of the network check.
    fn net_check_rows(&self, d: &NetDiag) -> [(String, Color); 5] {
        let dash = || ("-".to_string(), ERR);
        [
            match &d.server {
                Ok(rtt) => (format!("{} ms", rtt.as_millis()), OK),
                Err(_) => (self.t(S::Unreachable).to_string(), ERR),
            },
            match &d.websocket {
                Ok(()) => (self.t(S::Works).to_string(), OK),
                Err(_) => (self.t(S::NotWorking).to_string(), ERR),
            },
            d.public_addr.map_or_else(dash, |a| (a.to_string(), OK)),
            d.local_ip.map_or_else(dash, |ip| (ip.to_string(), OK)),
            match d.nat {
                NatKind::Cone => (self.t(S::NatCone).to_string(), OK),
                NatKind::Symmetric => (self.t(S::NatSymmetric).to_string(), WARN),
                NatKind::UdpBlocked => (self.t(S::NatBlocked).to_string(), ERR),
            },
        ]
    }

    fn draw_hold(&self, c: &mut Canvas, f: &mut Font, progress: f32) {
        let (w, x, y) = (96, 80, 112);
        c.fill_rect(x - 4, y - 16, w + 8, 30, SHADE);
        f.draw_centered(c, y - 12, self.t(S::Menu), WHITE, 1);
        c.rect(x, y, w, 8, WHITE);
        c.fill_rect(x + 1, y + 1, ((w - 2) as f32 * progress.clamp(0.0, 1.0)) as i32, 6, ACCENT);
    }

    fn draw_modal(&self, c: &mut Canvas, f: &mut Font, m: &Modal, hits: &mut Vec<(Area, Hit)>) {
        let leave = m.confirm.is_some();
        let buttons = if leave { 2 } else { 1 };
        let h = 16 + 10 + m.lines.len() as i32 * 10 + 8 + buttons * 16 + 4;
        let y0 = ((232 - h) / 2).max(4);
        c.fill_rect(12, y0, 232, h, PANEL);
        c.rect(12, y0, 232, h, if leave { SEC } else { ERR });
        f.draw_centered(c, y0 + 8, &trunc(&m.title, 27), if leave { WHITE } else { ERR }, 1);
        for (i, line) in m.lines.iter().enumerate() {
            f.draw(c, 20, y0 + 26 + i as i32 * 10, line, WHITE, 1);
        }
        let by = y0 + 26 + m.lines.len() as i32 * 10 + 12;
        let items: Vec<Item> = if leave {
            vec![Item::new(self.t(S::Leave)), Item::new(self.t(S::Stay))]
        } else {
            vec![Item::new(self.t(S::Ok))]
        };
        for (i, it) in items.iter().enumerate() {
            let y = by + i as i32 * 16;
            if i == m.sel {
                c.fill_rect(16, y - 4, 224, 16, ACCENT);
                f.draw(c, 20, y, ">", WHITE, 1);
            }
            f.draw(c, 32, y, &it.label, WHITE, 1);
            hits.push((row_area(y, 16), Hit::Modal(i)));
        }
    }

    fn draw_toasts(&self, c: &mut Canvas, f: &mut Font, base: i32) {
        for (i, (msg, _)) in self.toasts.iter().rev().enumerate() {
            let line = trunc(msg, 30);
            let w = f.width(&line, 1);
            let y = base - i as i32 * 14;
            c.fill_rect((256 - w) / 2 - 4, y - 3, w + 8, 14, SHADE);
            f.draw_centered(c, y, &line, WHITE, 1);
        }
    }

    fn draw_hints(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, kind: Option<PadKind>, top: Option<Screen>, h: &mut Vec<(Area, Hit)>) {
        let entries: Vec<(Glyph, S)> = if self.modal.is_some() {
            vec![(Glyph::Confirm, S::Ok)]
        } else {
            match top {
                None => return,
                Some(Screen::Main) => vec![(Glyph::Confirm, S::Select)],
                Some(Screen::Picker(_)) => vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Back), (Glyph::LeftRight, S::Page)],
                Some(Screen::Code) => vec![(Glyph::Confirm, S::Join), (Glyph::Back, S::Back)],
                Some(Screen::Field(_)) => vec![(Glyph::Confirm, S::Ok), (Glyph::Back, S::Back)],
                Some(Screen::Settings) if self.sel[Screen::Settings.slot()] == 7 => {
                    vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Back)]
                }
                Some(Screen::Advanced) if self.sel[Screen::Advanced.slot()] == 2 => {
                    vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Back)]
                }
                Some(screen @ (Screen::Settings | Screen::Advanced)) if self.on_text_row(screen) => {
                    vec![(Glyph::Confirm, S::Edit), (Glyph::Back, S::Back)]
                }
                Some(Screen::Settings | Screen::Advanced) => vec![(Glyph::LeftRight, S::Change), (Glyph::Back, S::Back)],
                Some(Screen::NetCheck) if self.diag_running => vec![(Glyph::Back, S::Back)],
                Some(Screen::NetCheck) => vec![(Glyph::Confirm, S::Recheck), (Glyph::Back, S::Back)],
                Some(Screen::Lobby) => {
                    let host = ctx.lobby.is_some_and(|l| l.is_host && l.stage == Stage::Lobby);
                    let mut v = vec![(Glyph::Back, S::Leave)];
                    if host {
                        v.insert(0, (Glyph::Confirm, S::Start));
                    }
                    v
                }
                Some(Screen::Pause) => vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Resume)],
                Some(Screen::ByIp) => vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Back)],
            }
        };
        let y = 231;
        let mut x = 8;
        for (glyph, s) in entries {
            let label = self.t(s);
            let gw = glyph_width(f, kind, glyph);
            if x + gw + 4 + f.width(label, 1) > 250 {
                break;
            }
            // The whole entry (glyph + label, padded into the gaps, down to the bottom edge)
            // is a button; LeftRight splits into a left and a right half.
            let w = gw + 4 + f.width(label, 1) + 12;
            let area = Area { x: x - 6, y: 224, w, h: 16 };
            let keys: &[Key] = match glyph {
                Glyph::Confirm => &[Key::Confirm],
                Glyph::Back => &[Key::Back],
                Glyph::LeftRight => &[Key::Left, Key::Right],
            };
            let part = area.w / keys.len() as i32;
            for (k, key) in keys.iter().enumerate() {
                let a = Area { x: area.x + k as i32 * part, w: part, ..area };
                if self.press.is_some_and(|p| p.hit == Hit::Key(*key)) {
                    c.fill_rect(a.x, a.y, a.w, a.h, PANEL);
                }
                h.push((a, Hit::Key(*key)));
            }
            draw_glyph(c, f, x, y, kind, glyph);
            x += gw + 4;
            f.draw(c, x, y, label, SEC, 1);
            x += f.width(label, 1) + 12;
        }
    }

    /// Per-frame housekeeping in every app state, menu visible or not: toast aging and
    /// gamepad connect/disconnect notices.
    pub fn tick(&mut self, input: &InputFrame, dt: Duration) {
        let secs = dt.as_secs_f32();
        for t in &mut self.toasts {
            t.1 -= secs;
        }
        self.toasts.retain(|t| t.1 > 0.0);
        for ev in &input.pad_events {
            let msg = match ev {
                PadEvent::Connected(p) => match p.slot {
                    Some(seat) => self.seat_text(&p.name, seat),
                    None => format!("{} {} {}", self.t(S::PadConnected), trunc(&p.name, 12), self.t(S::PadPlugged)),
                },
                PadEvent::Seated(_) => continue,
                PadEvent::Disconnected(p) => {
                    format!("{} {} {}", self.t(S::PadConnected), trunc(&p.name, 12), self.t(S::PadDisconnected))
                }
            };
            self.toast(&msg);
        }
    }

    /// Toast that `pad` took a seat ("Gamepad NAME → player N"); nothing without a seat.
    pub fn toast_seated(&mut self, pad: &PadInfo) {
        if let Some(seat) = pad.slot {
            let msg = self.seat_text(&pad.name, seat);
            self.toast(&msg);
        }
    }

    fn seat_text(&self, name: &str, seat: usize) -> String {
        format!("{} {} → {} {}", self.t(S::PadConnected), trunc(name, 12), self.t(S::PlayerN), seat + 1)
    }
}

// ---------------------------------------------------------------------------------------------
// Icons

/// 4x7 triangle pointing left or right with its box at (x, y).
fn tri(c: &mut Canvas, x: i32, y: i32, col: Color, right: bool) {
    for k in 0..4 {
        let cx = if right { x + 3 - k } else { x + k };
        c.fill_rect(cx, y + k, 1, 7 - 2 * k, col);
    }
}

/// 7x4 triangle pointing up or down.
fn up_tri(c: &mut Canvas, x: i32, y: i32, col: Color, up: bool) {
    for k in 0..4 {
        let cy = if up { y + 3 - k } else { y + k };
        c.fill_rect(x + k, cy, 7 - 2 * k, 1, col);
    }
}

fn bitmap(c: &mut Canvas, x: i32, y: i32, rows: &[u8], width: i32, col: Color) {
    for (r, bits) in rows.iter().enumerate() {
        for b in 0..width {
            if bits >> (width - 1 - b) & 1 == 1 {
                c.fill_rect(x + b, y + r as i32, 1, 1, col);
            }
        }
    }
}

fn tick(c: &mut Canvas, x: i32, y: i32, col: Color) {
    bitmap(c, x, y, &[0b0000000, 0b0000001, 0b0000011, 0b1000110, 0b1101100, 0b0111000, 0b0010000], 7, col);
}

fn cross(c: &mut Canvas, x: i32, y: i32, col: Color) {
    bitmap(c, x, y, &[0b1000001, 0b0100010, 0b0010100, 0b0001000, 0b0010100, 0b0100010, 0b1000001], 7, col);
}

/// The README's connection-trouble advice for a finished network check (most blocking
/// problem first).
fn net_advice(d: &NetDiag) -> S {
    if d.server.is_err() {
        S::AdviceServer
    } else if d.websocket.is_err() {
        S::AdviceRooms
    } else {
        match d.nat {
            NatKind::UdpBlocked => S::AdviceBlocked,
            NatKind::Symmetric => S::AdviceSymmetric,
            NatKind::Cone => S::AdviceAllGood,
        }
    }
}

const PS_CROSS: [u8; 8] = [0x81, 0x42, 0x24, 0x18, 0x18, 0x24, 0x42, 0x81];
const PS_CIRCLE: [u8; 8] = [0x3C, 0x42, 0x81, 0x81, 0x81, 0x81, 0x42, 0x3C];
const PS_TRIANGLE: [u8; 8] = [0x18, 0x18, 0x24, 0x24, 0x42, 0x42, 0x81, 0xFF];
const PS_SQUARE: [u8; 8] = [0xFF, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0xFF];
const PS_BLUE: Color = rgb(0x7C, 0xA8, 0xF0);
const PS_RED: Color = rgb(0xF0, 0x6E, 0x6E);
const PS_GREEN: Color = rgb(0x4C, 0xC6, 0x9A);
const PS_PINK: Color = rgb(0xE8, 0x8C, 0xC8);
const LETTER_A: [u8; 5] = [0b010, 0b101, 0b111, 0b101, 0b101];
const LETTER_B: [u8; 5] = [0b110, 0b101, 0b110, 0b101, 0b110];

fn glyph_width(f: &Font, kind: Option<PadKind>, glyph: Glyph) -> i32 {
    match (kind, glyph) {
        (_, Glyph::LeftRight) => 12,
        (None, Glyph::Confirm) => f.width("Enter", 1) + 4,
        (None, Glyph::Back) => f.width("Esc", 1) + 4,
        (Some(PadKind::PlayStation), _) => 8,
        (Some(_), _) => 9,
    }
}

fn draw_glyph(c: &mut Canvas, f: &mut Font, x: i32, y: i32, kind: Option<PadKind>, glyph: Glyph) {
    let confirm = matches!(glyph, Glyph::Confirm);
    match (kind, glyph) {
        (_, Glyph::LeftRight) => {
            tri(c, x, y, SEC, false);
            tri(c, x + 7, y, SEC, true);
        }
        (None, _) => {
            let label = if confirm { "Enter" } else { "Esc" };
            c.fill_rect(x, y - 1, f.width(label, 1) + 4, 10, DIS_BAR);
            f.draw(c, x + 2, y, label, WHITE, 1);
        }
        (Some(PadKind::PlayStation), _) => {
            if confirm {
                bitmap(c, x, y, &PS_CROSS, 8, PS_BLUE);
            } else {
                bitmap(c, x, y, &PS_CIRCLE, 8, PS_RED);
            }
        }
        (Some(kind), _) => {
            // South confirms; Nintendo prints B there, everyone else A.
            let a = confirm != (kind == PadKind::Nintendo);
            let col = if a { OK } else { ERR };
            for dy in 0..9 {
                for dx in 0..9 {
                    let (px, py) = (dx * 2 - 8, dy * 2 - 8);
                    if px * px + py * py <= 81 {
                        c.fill_rect(x + dx, y - 1 + dy, 1, 1, col);
                    }
                }
            }
            bitmap(c, x + 3, y + 1, if a { &LETTER_A } else { &LETTER_B }, 3, BG);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ebobo_core::Region;
    use ebobo_net::{PeerId, PlayerInfo};
    use std::path::PathBuf;

    const DT: Duration = Duration::from_millis(16);

    fn entry(name: &str, supported: bool, region: Region) -> RomEntry {
        RomEntry { path: PathBuf::from(format!("/r/{name}.nes")), name: name.into(), crc32: 1, mapper: if supported { 1 } else { 90 }, supported, region }
    }

    fn nav(f: impl FnOnce(&mut Nav)) -> InputFrame {
        let mut i = InputFrame::default();
        f(&mut i.nav);
        i
    }

    fn ctx<'a>(cfg: &'a mut Config, lib: &'a [RomEntry], lobby: Option<&'a LobbyStatus>) -> UiCtx<'a> {
        UiCtx { library: lib, scanning: false, lobby, game: None, pads: &[], config: cfg, version: "0.1.0" }
    }

    fn lobby(n: usize, host: bool, can_start: bool) -> LobbyStatus {
        let names = ["Ваня", "Саша", "Petr"];
        let players = (0..n)
            .map(|i| PlayerInfo {
                id: PeerId(i as u8),
                name: names[i].into(),
                is_self: i == 0,
                route: (i > 0).then_some(if i == 1 { RouteKind::Direct } else { RouteKind::Relay }),
                rtt_ms: (i > 0).then_some([0, 34, 120][i]),
                rom: if i == 2 { RomState::Receiving { percent: 45 } } else { RomState::Have },
                ready: i != 2,
                away: i == 2,
            })
            .collect();
        LobbyStatus { stage: Stage::Lobby, code: Some("K7QPA".into()), me: Some(PeerId(0)), is_host: host, players, rom_name: Some("Super Mario Bros. 3 (USA)".into()), can_start, error: None }
    }

    fn pointer(events: &[PointerEvent]) -> InputFrame {
        InputFrame { pointer: events.to_vec(), ..InputFrame::default() }
    }

    fn tap(x: i32, y: i32) -> InputFrame {
        pointer(&[PointerEvent::Down(Some((x, y))), PointerEvent::Up(Some((x, y)))])
    }

    /// An idle frame whose last-used device is a gamepad of `kind` (hint glyphs follow it).
    fn hint(kind: PadKind) -> InputFrame {
        InputFrame { hint_kind: Some(kind), ..InputFrame::default() }
    }

    /// Draws a frame so the hit regions exist, as the app does every frame.
    fn redraw(ui: &mut Ui, c: &UiCtx) {
        ui.draw(&mut Canvas::new(), &mut Font::new(), c, &InputFrame::default());
    }

    #[test]
    fn ui_tap_activates_main_row() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        let mut c = ctx(&mut cfg, &[], None);
        redraw(&mut ui, &c);
        // Rows from y = 88, 16 px apart, full width: "Play solo" is row 2.
        assert_eq!(ui.update(&tap(4, 2 * 16 + 88 + 2), &mut c, DT), None);
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        ui.show_main();
        redraw(&mut ui, &c);
        // The Quit row's band starts 4 px above its text; its top edge at the far right fires.
        assert_eq!(ui.update(&tap(250, 5 * 16 + 88 - 4), &mut c, DT), Some(UiAction::Quit));
        // Every row is at least MIN_TAP tall.
        assert!(ui.regions.iter().all(|(a, _)| a.h >= MIN_TAP));
    }

    #[test]
    fn ui_release_outside_cancels() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        let mut c = ctx(&mut cfg, &[], None);
        redraw(&mut ui, &c);
        let quit = (128, 5 * 16 + 88);
        // Press selects the row at once; release on the title cancels.
        assert_eq!(ui.update(&pointer(&[PointerEvent::Down(Some(quit))]), &mut c, DT), None);
        assert_eq!(ui.sel[Screen::Main.slot()], 5);
        assert_eq!(ui.update(&pointer(&[PointerEvent::Up(Some((128, 20)))]), &mut c, DT), None);
        // Release in the letterbox (outside the picture) cancels too.
        let press_out = [PointerEvent::Down(Some(quit)), PointerEvent::Up(None)];
        assert_eq!(ui.update(&pointer(&press_out), &mut c, DT), None);
        // Release on another row cancels as well.
        let other = [PointerEvent::Down(Some(quit)), PointerEvent::Up(Some((128, 88)))];
        assert_eq!(ui.update(&pointer(&other), &mut c, DT), None);
        assert_eq!(ui.stack, vec![Screen::Main]);
        // A plain release with no press does nothing.
        assert_eq!(ui.update(&pointer(&[PointerEvent::Up(Some(quit))]), &mut c, DT), None);
    }

    #[test]
    fn ui_settings_cycle_by_tap() {
        let mut cfg = Config::default();
        cfg.volume = 0.5;
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Main, Screen::Settings];
        let volume_y = 28 + 5 * 14;
        let back = |ui: &Ui| {
            ui.regions
                .iter()
                .find_map(|(_, hit)| match hit {
                    Hit::Row { i: 5, back, .. } => *back,
                    _ => None,
                })
                .expect("volume row cycles")
        };
        let mut c = ctx(&mut cfg, &[], None);
        redraw(&mut ui, &c);
        let (from, to) = back(&ui);
        assert_eq!(ui.update(&tap(from, volume_y), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.volume, 0.4);
        assert_eq!(ui.sel[Screen::Settings.slot()], 5);
        redraw(&mut ui, &c);
        let (_, to2) = back(&ui);
        assert_eq!(ui.update(&tap(to2, volume_y), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.volume, 0.5);
        assert_eq!(ui.update(&tap(30, volume_y), &mut c, DT), Some(UiAction::ConfigChanged), "label area goes right");
        assert_eq!(c.config.volume, 0.6);
        assert!(to - from >= 8);
        // The Back button of the hint bar goes back.
        redraw(&mut ui, &c);
        let back = ui.regions.iter().find(|(_, hit)| *hit == Hit::Key(Key::Back)).map(|(a, _)| *a).expect("back hint");
        ui.update(&tap(back.x + 1, back.y + back.h - 1), &mut c, DT);
        assert_eq!(ui.stack, vec![Screen::Main]);
    }

    #[test]
    fn ui_picker_scrolls_by_drag_and_wheel() {
        let mut cfg = Config::default();
        let lib: Vec<RomEntry> = (0..40).map(|i| entry(&format!("Game {i}"), true, Region::Ntsc)).collect();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Main, Screen::Picker(Purpose::Solo)];
        let mut c = ctx(&mut cfg, &lib, None);
        redraw(&mut ui, &c);
        // Drag up by 5 rows: later games come into view, and the release starts nothing.
        let y = 26 + 8 * PICKER_STEP;
        let drag = [
            PointerEvent::Down(Some((128, y))),
            PointerEvent::Move(Some((128, y - 2 * PICKER_STEP))),
            PointerEvent::Move(Some((128, y - 5 * PICKER_STEP))),
            PointerEvent::Up(Some((128, y - 5 * PICKER_STEP))),
        ];
        assert_eq!(ui.update(&pointer(&drag), &mut c, DT), None);
        assert_eq!(ui.picker_top, 5);
        assert!(ui.sel[Screen::Picker(Purpose::Solo).slot()] >= 5);
        // Wheel towards the user scrolls further down; past the end it stops.
        ui.update(&pointer(&[PointerEvent::Wheel(-3)]), &mut c, DT);
        assert_eq!(ui.picker_top, 8);
        ui.update(&pointer(&[PointerEvent::Wheel(-100)]), &mut c, DT);
        assert_eq!(ui.picker_top, 41 - PICKER_ROWS);
        // A tap on the first visible row plays the game shown there.
        redraw(&mut ui, &c);
        let first = ui.picker_top;
        assert_eq!(ui.update(&tap(128, 26), &mut c, DT), Some(UiAction::PlaySolo(lib[first].clone())));
    }

    #[test]
    fn ui_join_by_typed_code() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        let mut c = ctx(&mut cfg, &[], None);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        assert!(ui.wants_text_input());
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "incomplete code");
        let mut typed = InputFrame::default();
        typed.text = "k7q-pa".into();
        ui.update(&typed, &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::JoinRoom("K7QPA".into())));
    }

    #[test]
    fn ui_picker_skips_unsupported() {
        let mut cfg = Config::default();
        let lib = [entry("Bad", false, Region::Ntsc), entry("Good", true, Region::Ntsc)];
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        let mut c = ctx(&mut cfg, &lib, None);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::PlaySolo(lib[1].clone())));
        // Moving up from Good wraps past Bad to Rescan.
        assert_eq!(ui.update(&nav(|n| { n.up = true; n.confirm = true }), &mut c, DT), Some(UiAction::Rescan));
    }

    #[test]
    fn ui_settings_volume_right() {
        let mut cfg = Config::default();
        let before = cfg.volume;
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        {
            let mut c = ctx(&mut cfg, &[], None);
            for _ in 0..4 {
                ui.update(&nav(|n| n.down = true), &mut c, DT);
            }
            ui.update(&nav(|n| n.confirm = true), &mut c, DT);
            for _ in 0..5 {
                ui.update(&nav(|n| n.down = true), &mut c, DT);
            }
            assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        }
        assert!((cfg.volume - (before + 0.1)).abs() < 1e-4, "{}", cfg.volume);
    }

    #[test]
    fn ui_settings_language_applies() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings];
        ui.sel[Screen::Settings.slot()] = 6;
        let mut c = ctx(&mut cfg, &[], None);
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(ui.lang, Lang::En);
        assert_eq!(cfg.lang.as_deref(), Some("en"));
    }

    #[test]
    fn ui_lobby_start_gated() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_lobby();
        let blocked = lobby(1, true, false);
        let mut c = ctx(&mut cfg, &[], Some(&blocked));
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        let ready = lobby(2, true, true);
        let mut c = ctx(&mut cfg, &[], Some(&ready));
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::StartLobbyGame));
        // Back asks first with "Stay" preselected; confirming stays, Leave must be chosen.
        assert_eq!(ui.update(&nav(|n| n.back = true), &mut c, DT), None);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "Stay is preselected");
        assert_eq!(ui.stack.last(), Some(&Screen::Lobby));
        ui.update(&nav(|n| n.back = true), &mut c, DT);
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::LeaveLobby));
    }

    #[test]
    fn ui_toast_expires_while_menu_hidden() {
        let mut ui = Ui::new(Lang::Ru);
        ui.hide();
        ui.toast("hello");
        let idle = InputFrame::default();
        let frame = Duration::from_millis(10);
        for _ in 0..240 {
            ui.tick(&idle, frame);
        }
        assert_eq!(ui.toasts.len(), 1, "still shown at 2.4 s");
        for _ in 0..11 {
            ui.tick(&idle, frame);
        }
        assert!(ui.toasts.is_empty(), "gone after 2.5 s");
    }

    #[test]
    fn ui_update_does_not_age_toasts() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        ui.toast("hello");
        let mut c = ctx(&mut cfg, &[], None);
        ui.update(&InputFrame::default(), &mut c, Duration::from_secs(3));
        assert_eq!(ui.toasts.len(), 1);
    }

    #[test]
    fn ui_pause_opens_on_continue() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_pause();
        let mut c = ctx(&mut cfg, &[], None);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.sel[Screen::Pause.slot()], 2);
        ui.hide();
        ui.show_pause();
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::Resume));
    }

    #[test]
    fn ui_pause_slot_row_cycles() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_pause();
        let solo = |slot| GameView { rom_name: "X", netplay: false, names: &[], stats: None, notice: None, desync: false, stalled: false, save_slot: Some((slot, None)) };
        let mut c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(solo(0)), pads: &[], config: &mut cfg, version: "0.1.0" };
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::SelectSlot(1)));
        assert_eq!(ui.update(&nav(|n| n.left = true), &mut c, DT), Some(UiAction::SelectSlot(3)));
        c.game = Some(solo(3));
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::SelectSlot(0)));
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::SaveState));
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::LoadState));
        let now = SystemTime::now();
        assert_eq!(ui.save_age(now - Duration::from_secs(5), now), "1 мин назад");
        assert_eq!(ui.save_age(now - Duration::from_secs(3 * 3600), now), "3 ч назад");
        assert_eq!(ui.save_age(now - Duration::from_secs(2 * 86_400), now), "2 дн назад");
    }

    #[test]
    fn ui_netplay_exit_asks_first() {
        let mut cfg = Config::default();
        let names = vec!["A".to_string(), "B".to_string()];
        let mut ui = Ui::new(Lang::Ru);
        ui.show_pause();
        let game = GameView { rom_name: "X", netplay: true, names: &names, stats: None, notice: None, desync: false, stalled: false, save_slot: None };
        let mut c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(game), pads: &[], config: &mut cfg, version: "0.1.0" };
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "exit must confirm");
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::LeaveGame));
    }

    #[test]
    fn ui_failed_join_keeps_code() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        ui.open_code();
        let mut c = ctx(&mut cfg, &[], None);
        let mut typed = InputFrame::default();
        typed.text = "K7QPA".into();
        ui.update(&typed, &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::JoinRoom("K7QPA".into())));
        // The app reports the failure and returns to the main menu.
        ui.show_main();
        ui.error("room not found");
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        ui.open_code();
        assert_eq!(ui.code_string(), "K7QPA");
        assert_eq!(ui.code_cur, CODE_LEN - 1);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::JoinRoom("K7QPA".into())));
    }

    #[test]
    fn ui_settings_top_level_fits() {
        let cfg = Config::default();
        let ui = Ui::new(Lang::Ru);
        assert_eq!(ui.settings_items(&cfg).len(), 8);
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings];
        ui.sel[Screen::Settings.slot()] = 7;
        let mut c = ctx(&mut cfg, &[], None);
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Advanced));
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(c.config.force_relay);
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Field(Field::Server)));
    }

    #[test]
    fn ui_settings_rows_after_volume() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings];
        let mut c = ctx(&mut cfg, &[], None);
        for _ in 0..6 {
            assert_eq!(ui.update(&nav(|n| n.down = true), &mut c, DT), None);
        }
        assert_eq!(ui.sel[Screen::Settings.slot()], 6);
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(ui.lang, Lang::En, "row 6 is Language");
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.sel[Screen::Settings.slot()], 7);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        assert_eq!(ui.stack.last(), Some(&Screen::Advanced));
        ui.pop();
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.sel[Screen::Settings.slot()], 0, "8 rows wrap");
    }

    #[test]
    fn ui_deck_keys_toasts_fit() {
        for s in [S::DeckKeys, S::DeckKeysFix] {
            for lang in [Lang::Ru, Lang::En] {
                assert!(lang.tr(s).chars().count() <= 30, "toasts are cut at 30");
            }
        }
    }

    #[test]
    fn ui_pad_toasts() {
        let pad = |slot| PadInfo { name: "DualSense".into(), kind: PadKind::PlayStation, slot };
        let mut ui = Ui::new(Lang::En);
        let mut f = InputFrame::default();
        f.pad_events = vec![PadEvent::Connected(pad(None))];
        ui.tick(&f, DT);
        assert_eq!(ui.toasts.len(), 1);
        assert_eq!(ui.toasts[0].0, format!("Gamepad DualSense {}", Lang::En.tr(S::PadPlugged)));
        f.pad_events = vec![PadEvent::Seated(pad(Some(1)))];
        ui.tick(&f, DT);
        assert_eq!(ui.toasts.len(), 1, "the app toasts seating itself");
        ui.toast_seated(&pad(None));
        assert_eq!(ui.toasts.len(), 1, "no seat, no toast");
        ui.toast_seated(&pad(Some(1)));
        assert_eq!(ui.toasts.last().unwrap().0, "Gamepad DualSense → player 2");
        f.pad_events = vec![PadEvent::Connected(pad(Some(0)))];
        ui.tick(&f, DT);
        assert_eq!(ui.toasts.last().unwrap().0, "Gamepad DualSense → player 1", "a returning pad gets its seat back");
    }

    #[test]
    fn ui_sprite_limit_cycles_in_advanced() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings, Screen::Advanced];
        ui.sel[Screen::Advanced.slot()] = 4;
        let items = ui.advanced_items(&cfg);
        assert_eq!(items.len(), 7);
        let mut c = ctx(&mut cfg, &[], None);
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.sprite_limit, SpriteLimit::Hardware);
        ui.update(&nav(|n| n.right = true), &mut c, DT);
        assert_eq!(c.config.sprite_limit, SpriteLimit::Off);
        ui.update(&nav(|n| n.right = true), &mut c, DT);
        assert_eq!(c.config.sprite_limit, SpriteLimit::Auto);
        ui.sel[Screen::Advanced.slot()] = 5;
        assert!(c.config.auto_update, "auto-update is on by default");
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(!c.config.auto_update);
        ui.sel[Screen::Advanced.slot()] = 4;
        ui.update(&nav(|n| n.left = true), &mut c, DT);
        assert_eq!(c.config.sprite_limit, SpriteLimit::Off);
        ui.sel[Screen::Advanced.slot()] = 6;
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.input_delay, Some(1), "input delay moved to Advanced");
        ui.update(&nav(|n| n.left = true), &mut c, DT);
        ui.update(&nav(|n| n.left = true), &mut c, DT);
        assert_eq!(c.config.input_delay, Some(6));
    }

    #[test]
    fn ui_effect_cycles_both_ways() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Main, Screen::Settings];
        ui.sel[Screen::Settings.slot()] = 4;
        let mut c = ctx(&mut cfg, &[], None);
        assert_eq!(c.config.effect, Effect::Off);
        for want in [Effect::CrtTv, Effect::Pvm, Effect::Lcd, Effect::Off] {
            assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
            assert_eq!(c.config.effect, want);
        }
        for want in [Effect::Lcd, Effect::Pvm, Effect::CrtTv, Effect::Off] {
            assert_eq!(ui.update(&nav(|n| n.left = true), &mut c, DT), Some(UiAction::ConfigChanged));
            assert_eq!(c.config.effect, want);
        }
        // Taps: the left part of the value steps back, the label area forward.
        redraw(&mut ui, &c);
        let y = 28 + 4 * 14;
        let (from, _) = ui
            .regions
            .iter()
            .find_map(|(_, hit)| match hit {
                Hit::Row { i: 4, back, .. } => *back,
                _ => None,
            })
            .expect("effect row cycles");
        assert_eq!(ui.update(&tap(from, y), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.effect, Effect::Lcd);
        redraw(&mut ui, &c);
        assert_eq!(ui.update(&tap(30, y), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.effect, Effect::Off);
    }

    #[test]
    fn ui_backspace_deletes_in_field() {
        let mut cfg = Config::default();
        cfg.last_join_addr = "1.2.3.4:77".into();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        let mut c = ctx(&mut cfg, &[], None);
        for _ in 0..3 {
            ui.update(&nav(|n| n.down = true), &mut c, DT);
        }
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert!(ui.wants_text_input());
        let mut bs = nav(|n| n.back = true);
        bs.backspace = true;
        ui.update(&bs, &mut c, DT);
        assert!(ui.wants_text_input(), "backspace must not leave the field");
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::JoinDirect("1.2.3.4:7".into())));
    }

    #[test]
    fn ui_code_backspace_and_paste() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Main];
        ui.open_code();
        let mut c = ctx(&mut cfg, &[], None);
        let mut p = InputFrame::default();
        p.paste = Some(" abcde ".into());
        ui.update(&p, &mut c, DT);
        let mut bs = nav(|n| n.back = true);
        bs.backspace = true;
        ui.update(&bs, &mut c, DT);
        assert!(ui.wants_text_input());
        assert_eq!(ui.code_string(), "ABCD_");
    }

    // --- PNG snapshots --------------------------------------------------------------------

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn write_png(path: &str, rgba: &[u8], w: usize, h: usize) {
        let mut raw = Vec::with_capacity((w * 4 + 1) * h);
        for row in rgba.chunks(w * 4) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        // zlib with stored (uncompressed) deflate blocks.
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, b) in blocks.iter().enumerate() {
            z.push((i + 1 == blocks.len()) as u8);
            z.extend_from_slice(&(b.len() as u16).to_le_bytes());
            z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
            z.extend_from_slice(b);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &x in &raw {
            a = (a + x as u32) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |ty: &[u8], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = ty.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32(&body).to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(w as u32).to_be_bytes());
        ihdr.extend_from_slice(&(h as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &z);
        chunk(b"IEND", &[]);
        std::fs::write(path, out).unwrap();
    }

    /// Renders Main, Lobby (3 players) and Pause to /tmp/ebobo-ui-*.png for a visual check.
    #[test]
    #[ignore]
    fn ui_render_snapshots() {
        let mut font = Font::new();
        let mut cfg = Config::default();
        let pads = [PadInfo { name: "DualSense".into(), kind: PadKind::PlayStation, slot: Some(0) }];
        let lob = lobby(3, true, false);
        let names = vec!["Ваня".to_string(), "Саша".to_string()];
        let stats = NetStats::default();
        let shots: [(&str, Screen); 5] =
            [("main", Screen::Main), ("lobby", Screen::Lobby), ("pause", Screen::Pause), ("settings", Screen::Settings), ("advanced", Screen::Advanced)];
        for (name, screen) in shots {
            let mut ui = Ui::new(Lang::Ru);
            ui.stack = vec![screen];
            ui.toast("Геймпад DualSense → игрок 2");
            let mut canvas = Canvas::new();
            let game = (screen == Screen::Pause).then(|| GameView { rom_name: "Test Game (USA)", netplay: true, names: &names, stats: Some(&stats), notice: None, desync: false, stalled: false, save_slot: None });
            if game.is_some() {
                canvas.clear(rgb(0x5C, 0x94, 0xFC));
            }
            let c = UiCtx { library: &[], scanning: false, lobby: Some(&lob), game, pads: &pads, config: &mut cfg, version: "0.1.0" };
            ui.draw(&mut canvas, &mut font, &c, &hint(PadKind::PlayStation));
            write_png(&format!("/tmp/ebobo-ui-{name}.png"), &canvas.px, 256, 240);
        }
    }

    fn diag(server_ok: bool, nat: NatKind) -> NetDiag {
        NetDiag {
            server: if server_ok { Ok(Duration::from_millis(54)) } else { Err("DNS: failed to lookup address".into()) },
            websocket: if server_ok { Ok(()) } else { Err("timeout".into()) },
            public_addr: (nat != NatKind::UdpBlocked).then(|| "98.128.229.202:55963".parse().unwrap()),
            local_ip: Some("192.168.1.6".parse().unwrap()),
            nat,
        }
    }

    #[test]
    fn ui_net_check_runs_on_open_and_rechecks_on_confirm() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings, Screen::Advanced];
        ui.sel[Screen::Advanced.slot()] = 2;
        let mut c = ctx(&mut cfg, &[], None);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::RunNetCheck));
        assert_eq!(ui.stack.last(), Some(&Screen::NetCheck));
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "one check at a time");
        ui.set_net_diag(diag(true, NatKind::Cone));
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::RunNetCheck));
        ui.set_net_diag(diag(true, NatKind::Cone));
        ui.update(&nav(|n| n.back = true), &mut c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Advanced));
    }

    #[test]
    fn ui_net_advice_follows_readme() {
        assert_eq!(net_advice(&diag(false, NatKind::Cone)), S::AdviceServer);
        let mut d = diag(true, NatKind::Cone);
        d.websocket = Err("x".into());
        assert_eq!(net_advice(&d), S::AdviceRooms);
        assert_eq!(net_advice(&diag(true, NatKind::Symmetric)), S::AdviceSymmetric);
        assert_eq!(net_advice(&diag(true, NatKind::UdpBlocked)), S::AdviceBlocked);
        assert_eq!(net_advice(&diag(true, NatKind::Cone)), S::AdviceAllGood);
    }

    /// Renders the network check (running, symmetric NAT, server down) in Russian and English
    /// (`/tmp/ebobo-ui-<name>[-en].png`) and the in-game "waiting for a peer" overlay.
    #[test]
    #[ignore]
    fn ui_render_net_snapshots() {
        let mut font = Font::new();
        let mut cfg = Config::default();
        let pads = [PadInfo { name: "DualSense".into(), kind: PadKind::PlayStation, slot: Some(0) }];
        let shots = [
            ("netcheck-running", None),
            ("netcheck", Some(diag(true, NatKind::Symmetric))),
            ("netcheck-ok", Some(diag(true, NatKind::Cone))),
            ("netcheck-down", Some(diag(false, NatKind::UdpBlocked))),
        ];
        for (lang, suffix) in [(Lang::Ru, ""), (Lang::En, "-en")] {
            for (name, result) in shots.clone() {
                let mut ui = Ui::new(lang);
                ui.stack = vec![Screen::Settings, Screen::Advanced, Screen::NetCheck];
                match result {
                    Some(d) => ui.set_net_diag(d),
                    None => ui.diag_running = true,
                }
                let mut canvas = Canvas::new();
                let c = UiCtx { library: &[], scanning: false, lobby: None, game: None, pads: &pads, config: &mut cfg, version: "0.1.0" };
                ui.draw(&mut canvas, &mut font, &c, &hint(PadKind::PlayStation));
                write_png(&format!("/tmp/ebobo-ui-{name}{suffix}.png"), &canvas.px, 256, 240);
            }
        }
        let names = vec!["Ваня".to_string(), "Саша".to_string()];
        let mut stats = NetStats::default();
        stats.peers.push(ebobo_net::PeerStats { peer: Some(PeerId(1)), slot: 1, ping_ms: 34, ..Default::default() });
        let mut ui = Ui::new(Lang::Ru);
        ui.hide();
        ui.toast("Связь прерывалась (сон)");
        let mut canvas = Canvas::new();
        canvas.clear(rgb(0x5C, 0x94, 0xFC));
        let notice = Some("Ждём Саша... 7 с".to_string());
        let game = GameView { rom_name: "Test Game (USA)", netplay: true, names: &names, stats: Some(&stats), notice, desync: false, stalled: true, save_slot: None };
        let c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(game), pads: &pads, config: &mut cfg, version: "0.1.0" };
        ui.draw(&mut canvas, &mut font, &c, &hint(PadKind::PlayStation));
        write_png("/tmp/ebobo-ui-game-wait.png", &canvas.px, 256, 240);
    }

    fn pad(f: impl FnOnce(&mut Nav)) -> InputFrame {
        let mut i = nav(f);
        i.pad.used = true;
        i
    }

    fn key_tap(ui: &Ui, c: char) -> InputFrame {
        let k = ui.osk.as_ref().expect("keyboard shown");
        let i = k.key_of(c).expect("key");
        let r = k.key_rects().find(|(j, _)| *j == i).expect("enabled").1;
        tap(r.x + r.w / 2, r.y + r.h / 2)
    }

    fn open_nick_by_pad(ui: &mut Ui, c: &mut UiCtx) {
        ui.stack = vec![Screen::Main, Screen::Settings];
        ui.update(&pad(|n| n.confirm = true), c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Field(Field::Nick)));
    }

    #[test]
    fn ui_osk_nick_by_gamepad() {
        let mut cfg = Config::default();
        cfg.nickname = "Ab".into();
        let mut ui = Ui::new(Lang::Ru);
        let mut c = ctx(&mut cfg, &[], None);
        open_nick_by_pad(&mut ui, &mut c);
        redraw(&mut ui, &c);
        assert!(ui.regions.iter().any(|(_, h)| matches!(h, Hit::Osk(_))), "keyboard drawn");
        assert!(ui.regions.iter().all(|(a, h)| !matches!(h, Hit::Key(_)) && (a.y >= osk::TOP || matches!(h, Hit::Cell(_)) || a.y + a.h <= osk::TOP)));
        // Cursor starts on q; right moves to w, A types it.
        ui.update(&pad(|n| n.right = true), &mut c, DT);
        ui.update(&pad(|n| n.confirm = true), &mut c, DT);
        assert_eq!(ui.field, "Abw");
        // B deletes, Start confirms.
        ui.update(&pad(|n| n.back = true), &mut c, DT);
        assert_eq!(ui.field, "Ab");
        let mut start = pad(|_| {});
        start.pad.start = true;
        assert_eq!(ui.update(&start, &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.nickname, "Ab");
    }

    #[test]
    fn ui_osk_tap_types_and_b_leaves_empty() {
        let mut cfg = Config::default();
        cfg.nickname = String::new();
        let mut ui = Ui::new(Lang::Ru);
        let mut c = ctx(&mut cfg, &[], None);
        open_nick_by_pad(&mut ui, &mut c);
        redraw(&mut ui, &c);
        ui.update(&key_tap(&ui, 'z'), &mut c, DT);
        assert_eq!(ui.field, "z");
        assert_eq!(ui.osk.as_ref().map(|k| k.cursor()), ui.osk.as_ref().and_then(|k| k.key_of('z')));
        ui.update(&pad(|n| n.back = true), &mut c, DT);
        assert_eq!(ui.field, "");
        assert!(ui.wants_text_input(), "B with text only deletes");
        ui.update(&pad(|n| n.back = true), &mut c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Settings), "B on an empty field leaves");
    }

    #[test]
    fn ui_osk_room_code_joins_like_typing() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_main();
        let mut c = ctx(&mut cfg, &[], None);
        ui.update(&pad(|n| n.down = true), &mut c, DT);
        ui.update(&pad(|n| n.confirm = true), &mut c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Code));
        assert!(ui.osk.is_some());
        for ch in "k7qpa".chars() {
            redraw(&mut ui, &c);
            ui.update(&key_tap(&ui, ch), &mut c, DT);
        }
        assert_eq!(ui.code_string(), "K7QPA");
        let mut start = pad(|_| {});
        start.pad.start = true;
        assert_eq!(ui.update(&start, &mut c, DT), Some(UiAction::JoinRoom("K7QPA".into())));
    }

    #[test]
    fn ui_osk_physical_typing_hides_it() {
        let mut cfg = Config::default();
        cfg.nickname = String::new();
        let mut ui = Ui::new(Lang::Ru);
        let mut c = ctx(&mut cfg, &[], None);
        open_nick_by_pad(&mut ui, &mut c);
        let mut typed = InputFrame::default();
        typed.text = "Ivan".into();
        ui.update(&typed, &mut c, DT);
        assert!(ui.osk.is_none());
        assert_eq!(ui.field, "Ivan");
        redraw(&mut ui, &c);
        assert!(ui.regions.iter().any(|(_, h)| *h == Hit::Key(Key::Back)), "hint bar back");
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.nickname, "Ivan");
        // Enter on a physical keyboard opens the field without the built-in keyboard.
        ui.stack = vec![Screen::Main, Screen::Settings];
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert!(ui.wants_text_input());
        assert!(ui.osk.is_none());
    }

    /// Renders the nickname field with the keyboard (Latin, Cyrillic) and the room-code
    /// screen to /tmp/oskwire-*.png for a visual check.
    #[test]
    #[ignore]
    fn ui_osk_snapshots() {
        let mut font = Font::new();
        let pads = [PadInfo { name: "Steam Deck".into(), kind: PadKind::SteamDeck, slot: Some(0) }];
        for name in ["nick-latin", "nick-cyr", "code"] {
            let mut cfg = Config::default();
            cfg.nickname = if name == "nick-cyr" { "Ваня".into() } else { "ivan".into() };
            let mut ui = Ui::new(Lang::Ru);
            let mut c = UiCtx { library: &[], scanning: false, lobby: None, game: None, pads: &pads, config: &mut cfg, version: "0.1.0" };
            if name == "code" {
                ui.show_main();
                ui.update(&pad(|n| n.down = true), &mut c, DT);
                ui.update(&pad(|n| n.confirm = true), &mut c, DT);
                let mut typed = pad(|_| {});
                typed.text = "K7Q".into();
                ui.update(&typed, &mut c, DT);
            } else {
                open_nick_by_pad(&mut ui, &mut c);
                if name == "nick-cyr" {
                    let mut x = pad(|_| {});
                    x.pad.layout = true;
                    ui.update(&x, &mut c, DT);
                }
            }
            ui.toast("Геймпад подключён");
            let mut canvas = Canvas::new();
            ui.draw(&mut canvas, &mut font, &c, &hint(PadKind::SteamDeck));
            let k = 4;
            let mut big = vec![0u8; 256 * k * 240 * k * 4];
            for y in 0..240 * k {
                for x in 0..256 * k {
                    let s = ((y / k) * 256 + x / k) * 4;
                    big[(y * 256 * k + x) * 4..][..4].copy_from_slice(&canvas.px[s..s + 4]);
                }
            }
            write_png(&format!("/tmp/oskwire-{name}.png"), &big, 256 * k, 240 * k);
        }
    }

    /// Renders Settings (Effect row selected, CRT TV) in Russian and English to
    /// /tmp/crtwire-settings-{ru,en}.png at 4x for a visual check.
    #[test]
    #[ignore]
    fn ui_settings_effect_snapshots() {
        let mut font = Font::new();
        let pads = [PadInfo { name: "Steam Deck".into(), kind: PadKind::SteamDeck, slot: Some(0) }];
        for (lang, name) in [(Lang::Ru, "ru"), (Lang::En, "en")] {
            let mut cfg = Config::default();
            cfg.nickname = "Ваня".into();
            cfg.effect = Effect::CrtTv;
            let mut ui = Ui::new(lang);
            ui.stack = vec![Screen::Main, Screen::Settings];
            ui.sel[Screen::Settings.slot()] = 4;
            let c = UiCtx { library: &[], scanning: false, lobby: None, game: None, pads: &pads, config: &mut cfg, version: "0.1.0" };
            let mut canvas = Canvas::new();
            ui.draw(&mut canvas, &mut font, &c, &hint(PadKind::SteamDeck));
            let k = 4;
            let mut big = vec![0u8; 256 * k * 240 * k * 4];
            for y in 0..240 * k {
                for x in 0..256 * k {
                    let s = ((y / k) * 256 + x / k) * 4;
                    big[(y * 256 * k + x) * 4..][..4].copy_from_slice(&canvas.px[s..s + 4]);
                }
            }
            write_png(&format!("/tmp/crtwire-settings-{name}.png"), &big, 256 * k, 240 * k);
        }
    }

    /// Renders Settings with the gamepad list and button legend, in Russian and English, to
    /// `/tmp/ebobo-pads-<case>-<ru|en>.png` at 4x: `deck`, `ps` (DualSense seated, Deck idle),
    /// `four`, `nintendo` (Switch Pro without a seat) and `none`.
    #[test]
    #[ignore]
    fn ui_settings_pads_snapshots() {
        let mut font = Font::new();
        let pad = |name: &str, kind, slot| PadInfo { name: name.into(), kind, slot };
        let deck = [pad("Steam Deck", PadKind::SteamDeck, Some(0))];
        let ps = [pad("DualSense Wireless Controller", PadKind::PlayStation, Some(0)), pad("Steam Deck", PadKind::SteamDeck, None)];
        let four = [
            pad("Xbox Series X Controller", PadKind::Xbox, Some(0)),
            pad("DualSense Wireless Controller", PadKind::PlayStation, Some(1)),
            pad("Nintendo Switch Pro Controller", PadKind::Nintendo, None),
            pad("Steam Deck", PadKind::SteamDeck, None),
        ];
        let nintendo = [pad("Nintendo Switch Pro Controller", PadKind::Nintendo, None)];
        let cases: [(&str, &[PadInfo], Option<PadKind>); 5] = [
            ("deck", &deck, Some(PadKind::SteamDeck)),
            ("ps", &ps, Some(PadKind::PlayStation)),
            ("four", &four, Some(PadKind::Xbox)),
            ("nintendo", &nintendo, Some(PadKind::Nintendo)),
            ("none", &[], None),
        ];
        for (lang, name) in [(Lang::Ru, "ru"), (Lang::En, "en")] {
            for (case, pads, kind) in cases {
                let mut cfg = Config::default();
                cfg.nickname = "Ваня".into();
                let mut ui = Ui::new(lang);
                ui.stack = vec![Screen::Main, Screen::Settings];
                ui.sel[Screen::Settings.slot()] = 0;
                let c = UiCtx { library: &[], scanning: false, lobby: None, game: None, pads, config: &mut cfg, version: "0.1.0" };
                let mut canvas = Canvas::new();
                ui.draw(&mut canvas, &mut font, &c, &InputFrame { hint_kind: kind, ..InputFrame::default() });
                let k = 4;
                let mut big = vec![0u8; 256 * k * 240 * k * 4];
                for y in 0..240 * k {
                    for x in 0..256 * k {
                        let s = ((y / k) * 256 + x / k) * 4;
                        big[(y * 256 * k + x) * 4..][..4].copy_from_slice(&canvas.px[s..s + 4]);
                    }
                }
                write_png(&format!("/tmp/ebobo-pads-{case}-{name}.png"), &big, 256 * k, 240 * k);
            }
        }
    }
}
