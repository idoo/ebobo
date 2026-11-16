//! Menus, lobby screen, pause menu, on-screen display and toasts, drawn on the 256x240 canvas.
//! Gamepad-first (Steam Deck), keyboard works everywhere. Owner: ui slice (also owns i18n.rs).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ebobo_art::shelf::Shelf;
use ebobo_art::Art;

use ebobo_net::{
    normalize_code, LobbyStatus, NatKind, NetDiag, NetError, NetStats, ResumeInfo, ResumeKind, RomState, RouteKind, Stage,
    CODE_ALPHABET, CODE_LEN,
};

use crate::canvas::{Canvas, Color, Font};
use crate::cards::{self, Cards};
use crate::config::{is_steam_deck, Config, ScreenMode, SpriteLimit};
use crate::crt::Effect;
use crate::enhance::Mode;
use crate::i18n::{Lang, S};
use crate::input::{InputFrame, Nav, PadEvent, PadInfo, PadKind, PointerEvent};
use crate::osk::{self, Charset, Osk, OskNav, OskOutput};
use crate::library::{RomEntry, RomFileError};

/// Why a game did not open: the file could not be read as a ROM, or the console rejected it.
#[derive(Clone, Copy, Debug)]
pub enum OpenFailure<'a> {
    File(&'a RomFileError),
    Console(&'a ebobo_console::RomError),
}

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
    /// Host changed what the game starts from in the lobby: an index into the options given
    /// to `Ui::set_resume_options`, `None` = a new game.
    PickResume(Option<usize>),
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
    /// Delete a game's file for good; asked for and confirmed in the game's menu.
    DeleteRom(PathBuf),
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
    /// Netplay: a shared save exists, so "Load for everyone" can load it.
    pub can_load: bool,
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
    /// How to open the menu from a game, for a gamepad and for the keyboard.
    MenuHintPad,
    MenuHintKeys,
    /// "{name} saved for everyone" / "{name} loaded the save" (netplay, `{name}` = the player).
    NetSaved,
    NetLoaded,
    /// A netplay game started from the last moment / from the save.
    ResumedLast,
    ResumedSave,
    /// A game's file was deleted / could not be deleted.
    FileDeleted,
    CantDelete,
    /// The room code went to the clipboard.
    CodeCopied,
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
/// `SHADE` over `BG` as one solid colour: a toast's box over a menu.
const TOAST_BG: Color = rgb(0x07, 0x06, 0x09);
const SLOT: [Color; 4] = [rgb(0x3B, 0x7B, 0xE4), rgb(0xE4, 0x3B, 0x2C), rgb(0x4C, 0xC6, 0x6A), rgb(0xE4, 0x6A, 0xB4)];

/// Smallest tap target height, canvas px (≈ 44 pt on the Deck).
const MIN_TAP: i32 = 14;
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

/// Name of a saved online game kind.
fn resume_label(kind: ResumeKind) -> S {
    match kind {
        ResumeKind::Last => S::ResumeLast,
        ResumeKind::Save => S::ResumeSave,
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

/// The shortcut that copies a room code on this platform.
const COPY_KEYS: &str = if cfg!(target_os = "macos") { "Cmd+C" } else { "Ctrl+C" };

/// The line under a room code: "Tell friends the code", plus the copy shortcut in brackets
/// when the player is on a keyboard.
fn tell_code_line(text: &str, keys: Option<&str>) -> String {
    match keys {
        Some(keys) => format!("{text} ({keys})"),
        None => text.to_string(),
    }
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
    /// The library's search text.
    Search,
    /// The user's own name for a game of the library.
    Rename(ebobo_art::Key),
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
    /// Two-choice confirmation instead of an OK box.
    confirm: Option<Confirm>,
    sel: usize,
}

/// The menu of one game of the library. It keeps what it needs of the game itself: the
/// library may be rescanned while the menu is open.
struct GameMenu {
    key: ebobo_art::Key,
    path: PathBuf,
    /// The name the game's file gave it (the last resort of `cards::shown_name`).
    name: String,
    sel: usize,
}

/// The rows of a game's menu: favourite, rename, hide, delete.
const GAME_MENU_ROWS: usize = 4;

/// The two answers of a confirmation box: the first performs `action`, the second (the safe
/// one, preselected) closes the box.
struct Confirm {
    action: UiAction,
    yes: S,
    no: S,
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
const PAUSE_NET: [S; 5] = [S::Resume, S::SaveAll, S::LoadAll, S::ResetAll, S::ExitGame];
const DELAYS: [Option<u8>; 7] = [None, Some(1), Some(2), Some(3), Some(4), Some(5), Some(6)];

/// Hint bar glyphs.
#[derive(Clone, Copy)]
enum Glyph {
    Confirm,
    Back,
    LeftRight,
    /// The options button (Start, or Space on a keyboard): the game's menu.
    Options,
    /// West face button: search.
    West,
    /// The shoulder pair: previous / next filter.
    Shoulders,
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
    Options,
    Search,
    PrevTab,
    NextTab,
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
            Key::Options => n.options = true,
            Key::Search => n.search = true,
            Key::PrevTab => n.prev_tab = true,
            Key::NextTab => n.next_tab = true,
        }
        n
    }
}

/// What a hit region does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    /// Row `i` of the top screen's list. Press selects it, release
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
    /// Row of a game's menu: press selects it, release does it.
    MenuRow(usize),
    /// The box of a game's menu off its rows: a tap there does nothing.
    MenuBox,
    /// The dimmed screen around a game's menu: a tap there closes it.
    MenuOutside,
}

/// A press in progress (mouse button or finger down).
#[derive(Clone, Copy, Debug)]
struct Press {
    hit: Hit,
}

pub struct Ui {
    lang: Lang,
    stack: Vec<Screen>,
    sel: [usize; 10],
    /// The library screen (one state for the three purposes).
    cards: Cards,
    /// The games list handed to `cards`, kept empty between frames for its allocation.
    games: Vec<cards::Game<'static>>,
    /// "Cannot run" texts of the library's unsupported NES games: (library index, mapper, text).
    notes: Vec<(usize, u16, String)>,
    /// Unix seconds, read from the system clock at every `update`.
    now: u64,
    /// Snapshot tests fix `now`.
    #[cfg(test)]
    now_pinned: bool,
    code: [Option<u8>; CODE_LEN],
    code_cur: usize,
    field: String,
    /// The field's prefilled text is selected as a whole (a game's name being changed): the
    /// first character typed or deleted replaces all of it, as in any rename box. An arrow
    /// key or a tap on the box keeps the text to add to.
    field_all: bool,
    /// Last room code sent to join; pre-filled on the next code entry (kept after a failure).
    last_code: Option<String>,
    modal: Option<Modal>,
    /// The open menu of a library game; only it takes input while it is open.
    menu: Option<GameMenu>,
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
    /// Host lobby: what the game can start from (`Last` before `Save`) and the current pick
    /// (`None` = a new game). Empty for guests and when nothing was saved.
    resume_options: Vec<ResumeInfo>,
    resume_pick: Option<usize>,
    /// Pictures and names of the library's cards.
    art: Art,
    /// Favourites and play history.
    shelf: Shelf,
}

impl Ui {
    /// An interface without the app around it (tests): pictures come from an offline `Art`
    /// over a fresh temporary folder, favourites and history stay in memory.
    #[cfg(test)]
    pub fn new(lang: Lang) -> Ui {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("ebobo-art-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        Ui::with_library(lang, Art::new(dir, false), Shelf::in_memory())
    }

    /// The interface with the app's library services.
    pub fn with_library(lang: Lang, art: Art, shelf: Shelf) -> Ui {
        Ui {
            lang,
            stack: Vec::new(),
            sel: [0; 10],
            cards: Cards::new(),
            games: Vec::new(),
            notes: Vec::new(),
            now: unix_now(),
            #[cfg(test)]
            now_pinned: false,
            code: [None; CODE_LEN],
            code_cur: 0,
            field: String::new(),
            field_all: false,
            last_code: None,
            modal: None,
            menu: None,
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
            resume_options: Vec::new(),
            resume_pick: None,
            art,
            shelf,
        }
    }

    pub fn art(&self) -> &Art {
        &self.art
    }

    pub fn art_mut(&mut self) -> &mut Art {
        &mut self.art
    }

    pub fn shelf_mut(&mut self) -> &mut Shelf {
        &mut self.shelf
    }

    pub fn set_lang(&mut self, lang: Lang) {
        self.lang = lang;
        self.notes.clear();
    }

    /// Main menu (no game running).
    pub fn show_main(&mut self) {
        self.stack = vec![Screen::Main];
        self.press = None;
    }

    /// The solo library of cards over the main menu (`--library`).
    pub fn show_library(&mut self) {
        self.stack = vec![Screen::Main, Screen::Picker(Purpose::Solo)];
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

    /// Host lobby: the saves this game can continue from and the one picked (`None` = a new
    /// game). The lobby shows a row to change it when `options` is not empty.
    pub fn set_resume_options(&mut self, options: Vec<ResumeInfo>, pick: Option<usize>) {
        self.resume_options = options;
        self.resume_pick = pick;
    }

    /// Lobby screen (room code, players, Start for the host). The selection starts on Start,
    /// which is row 1 under the host's resume row and clamps to row 0 without it.
    pub fn show_lobby(&mut self) {
        self.stack = vec![Screen::Lobby];
        self.sel[Screen::Lobby.slot()] = 1;
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
        self.menu = None;
        self.press = None;
    }

    pub fn is_visible(&self) -> bool {
        !self.stack.is_empty() || self.modal.is_some()
    }

    /// The keyboard types into the interface: a text field (room code, IP, nickname, server,
    /// search) has focus, or the library is on top (typing searches).
    pub fn wants_text_input(&self) -> bool {
        self.field_focused() || (self.modal.is_none() && self.menu.is_none() && matches!(self.stack.last(), Some(Screen::Picker(_))))
    }

    /// A text field screen is on top.
    fn field_focused(&self) -> bool {
        self.modal.is_none() && matches!(self.stack.last(), Some(Screen::Code | Screen::Field(_)))
    }

    /// Modal message box (dismissed with confirm/back).
    pub fn error(&mut self, message: &str) {
        self.modal = Some(Modal { title: self.t(S::Error).to_string(), lines: wrap(message, 27), confirm: None, sel: 0 });
    }

    /// Why a game did not open, in the interface's language: "<name>: <reason>".
    pub fn open_failure(&self, name: &str, why: OpenFailure<'_>) -> String {
        use ebobo_console::{md, nes, RomError};
        let reason = match why {
            OpenFailure::File(RomFileError::NotRom)
            | OpenFailure::Console(
                RomError::Unknown | RomError::Nes(nes::RomError::NotNes) | RomError::Md(md::RomError::NotMegaDrive),
            ) => self.t(S::OpenNotRom).to_string(),
            OpenFailure::Console(
                RomError::Nes(nes::RomError::Truncated | nes::RomError::Invalid(_))
                | RomError::Md(md::RomError::Truncated | md::RomError::Invalid(_)),
            ) => self.t(S::OpenDamaged).to_string(),
            OpenFailure::Console(RomError::Nes(nes::RomError::UnsupportedMapper(n))) => {
                self.t(S::OpenMapper).replace("{n}", &n.to_string())
            }
            OpenFailure::File(RomFileError::TooLarge) => self.t(S::OpenTooLarge).to_string(),
            OpenFailure::File(RomFileError::NoRomInZip) => self.t(S::OpenNoRomInZip).to_string(),
            OpenFailure::File(RomFileError::Read(system)) => format!("{} ({system})", self.t(S::OpenUnreadable)),
        };
        format!("{name}: {reason}")
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
            Text::MenuHintPad => S::MenuHintPad,
            Text::MenuHintKeys => S::MenuHintKeys,
            Text::NetSaved => S::NetSaved,
            Text::NetLoaded => S::NetLoaded,
            Text::ResumedLast => S::ResumedLast,
            Text::ResumedSave => S::ResumedSave,
            Text::FileDeleted => S::FileDeleted,
            Text::CantDelete => S::CantDelete,
            Text::CodeCopied => S::CodeCopied,
        })
    }

    /// Result of the network check started by `UiAction::RunNetCheck`.
    pub fn set_net_diag(&mut self, diag: NetDiag) {
        self.diag = Some(diag);
        self.diag_running = false;
    }

    pub fn update(&mut self, input: &InputFrame, ctx: &mut UiCtx, dt: Duration) -> Option<UiAction> {
        self.clock += dt.as_secs_f32();
        self.read_clock();
        // A gamepad press or a tap opens the next text field with the built-in keyboard,
        // Enter on a physical keyboard without it.
        if input.pad.used || input.pointer.iter().any(|e| matches!(e, PointerEvent::Up(_))) {
            self.osk_wanted = true;
        } else if input.nav.confirm {
            self.osk_wanted = false;
        }
        // Taps and clicks become the button they stand for, so every screen handles them
        // exactly like the keyboard/gamepad.
        let tapped = self.pointer(input);
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
                fav: n.fav || tapped.fav,
                search: n.search || tapped.search,
                prev_tab: n.prev_tab || tapped.prev_tab,
                next_tab: n.next_tab || tapped.next_tab,
                options: n.options || tapped.options,
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
                        return modal.confirm.map(|c| c.action);
                    }
                }
            } else if nav.confirm || nav.back {
                self.modal = None;
            }
            return None;
        }
        if self.menu.is_some() {
            return self.update_menu(&nav);
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
            Screen::Picker(purpose) => self.update_picker(purpose, input, ctx, dt),
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
                let host = lobby.is_host && lobby.stage == Stage::Lobby;
                let choosing = host && !self.resume_options.is_empty();
                let confirm = self.menu_step(top, if choosing { 2 } else { 1 }, &nav);
                if choosing && self.sel[top.slot()] == 0 {
                    if !(nav.left || nav.right || confirm.is_some()) {
                        return None;
                    }
                    self.resume_pick = self.cycle_resume(nav.left);
                    return Some(UiAction::PickResume(self.resume_pick));
                }
                (confirm.is_some() && host && lobby.can_start).then_some(UiAction::StartLobbyGame)
            }
            Screen::Pause => {
                if nav.back || input.menu {
                    self.hide();
                    return Some(UiAction::Resume);
                }
                let netplay = ctx.game.as_ref().is_some_and(|g| g.netplay);
                let can_load = ctx.game.as_ref().is_some_and(|g| g.can_load);
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
                    S::SaveState | S::SaveAll => UiAction::SaveState,
                    S::LoadState => UiAction::LoadState,
                    // Loading throws away everyone's progress since the save: ask first. F7
                    // skips the question (the app handles it without the menu).
                    S::LoadAll => {
                        if can_load {
                            let lines = wrap(self.t(S::LoadAllLoses), 27);
                            self.ask(S::LoadAllQ, lines, Confirm { action: UiAction::LoadState, yes: S::LoadYes, no: S::Cancel });
                        }
                        return None;
                    }
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
            Some(Screen::Picker(p)) => self.draw_picker(canvas, font, ctx, p),
            Some(Screen::Code) => self.draw_code(canvas, font, &mut h),
            Some(Screen::ByIp) => self.draw_by_ip(canvas, font, ctx, &mut h),
            Some(Screen::Field(f)) => self.draw_field(canvas, font, f),
            Some(Screen::Settings) => self.draw_settings(canvas, font, ctx, input.hint_kind, &mut h),
            Some(Screen::Advanced) => self.draw_advanced(canvas, font, ctx, &mut h),
            Some(Screen::NetCheck) => self.draw_net_check(canvas, font),
            Some(Screen::Lobby) => self.draw_lobby(canvas, font, ctx, input.hint_kind.is_none(), &mut h),
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
        // A toast in the library takes the place of the second detail line. The line is wiped
        // here, under any box that dims the screen, so it dims with the rest and nothing reads
        // beside the toast.
        let library = matches!(top, Some(Screen::Picker(_)));
        if library {
            for i in 0..self.toasts.len() as i32 {
                canvas.fill_rect(0, cards::INFO_Y - i * 14 - 3, 256, 14, BG);
            }
        }
        if let Some(menu) = self.menu.as_ref().filter(|_| visible) {
            // Like a modal: only the menu takes taps, and only it reads at full strength.
            h.clear();
            canvas.dim(0.55);
            self.draw_menu(canvas, font, menu, &mut h);
        }
        if let Some(modal) = &self.modal {
            // Only the modal takes taps while it is open, and only it reads at full strength.
            h.clear();
            canvas.dim(0.55);
            self.draw_modal(canvas, font, modal, &mut h);
        }
        // The keyboard covers the hint bar (its keys say the same) and the toast line. Over a
        // menu a toast's box is solid, so the text under it never shows through.
        let (base, solid) = match top {
            _ if osk_shown => (osk::TOP - 16, true),
            _ if library => (cards::INFO_Y, true),
            _ => (if visible { 214 } else { 224 }, visible),
        };
        self.draw_toasts(canvas, font, base, solid);
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

    /// `save_age` of a time in Unix seconds (online saves travel as Unix seconds).
    fn unix_age(&self, saved_at: u64) -> String {
        self.save_age(SystemTime::UNIX_EPOCH + Duration::from_secs(saved_at), SystemTime::now())
    }

    /// The host's lobby row choosing what the game starts from: the pick, with its age for a
    /// save; ◀ ▶ (or a tap on either side) change it. "New game" draws the arrows alone.
    fn resume_item(&self) -> Item {
        match self.resume_pick.and_then(|i| self.resume_options.get(i)) {
            Some(r) => Item::value(self.t(resume_label(r.kind)), self.unix_age(r.saved_at), true),
            None => Item::value(self.t(S::ResumeNew), String::new(), true),
        }
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
    /// cancels. The library has no regions but its hint bar: it reads the pointer itself.
    fn pointer(&mut self, input: &InputFrame) -> Nav {
        let mut out = Nav::default();
        for ev in &input.pointer {
            match *ev {
                PointerEvent::Down(Some((x, y))) => {
                    self.press = self.hit_at(x, y).map(|hit| Press { hit });
                    if let Some(p) = self.press {
                        self.select(p.hit);
                    }
                }
                PointerEvent::Down(None) => {
                    self.press = None;
                    self.release_osk();
                }
                PointerEvent::Move(_) | PointerEvent::Wheel(_) | PointerEvent::Secondary(_) => {}
                PointerEvent::Up(pos) => {
                    self.release_osk();
                    let Some(p) = self.press.take() else { continue };
                    let Some((x, y)) = pos else { continue };
                    if self.hit_at(x, y) == Some(p.hit) {
                        if let Hit::Osk(i) = p.hit {
                            self.osk_tap = Some(i);
                        }
                        out = self.activate(p.hit, x);
                        // The screen may change; later events would hit stale regions.
                        break;
                    }
                }
            }
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
            Hit::MenuRow(i) => {
                if let Some(m) = &mut self.menu {
                    m.sel = i;
                }
            }
            Hit::Cell(i) => self.code_cur = i,
            Hit::Osk(i) => {
                if let Some(k) = &mut self.osk {
                    k.set_pressed(Some(i));
                }
            }
            Hit::Row { .. } | Hit::Key(_) | Hit::MenuBox | Hit::MenuOutside => {}
        }
    }

    /// The button a completed tap at column `x` stands for.
    fn activate(&self, hit: Hit, x: i32) -> Nav {
        match hit {
            Hit::Row { enabled: false, .. } | Hit::Cell(_) | Hit::Osk(_) | Hit::MenuBox => Nav::default(),
            Hit::Row { back: Some((from, to)), .. } => (if (from..to).contains(&x) { Key::Left } else { Key::Right }).nav(),
            Hit::Row { .. } | Hit::Modal(_) | Hit::MenuRow(_) => Key::Confirm.nav(),
            Hit::MenuOutside => Key::Back.nav(),
            Hit::Key(k) => k.nav(),
        }
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
        self.ask(title, Vec::new(), Confirm { action, yes: S::Leave, no: S::Stay });
    }

    /// Confirmation box with the safe answer (`confirm.no`) preselected.
    fn ask(&mut self, title: S, lines: Vec<String>, confirm: Confirm) {
        self.modal = Some(Modal { title: self.t(title).to_string(), lines, confirm: Some(confirm), sel: 1 });
    }

    /// The resume pick after the current one (`back`: before it), cycling through "new game"
    /// (`None`), then each option in order.
    fn cycle_resume(&self, back: bool) -> Option<usize> {
        let n = self.resume_options.len() + 1;
        let pos = self.resume_pick.map_or(0, |i| i + 1);
        let next = if back { (pos + n - 1) % n } else { (pos + 1) % n };
        next.checked_sub(1)
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
        self.field_all = matches!(field, Field::Rename(_)) && !prefill.is_empty();
        self.field = prefill;
        self.push(Screen::Field(field));
        self.open_osk(match field {
            Field::Addr => Charset::Address,
            Field::Nick | Field::Search | Field::Rename(_) => Charset::Nickname,
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
        self.osk.is_some() && self.field_focused()
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
        // The shoulders pass through: in a text field they move the caret on every console.
        let nav = Nav { back: n.back, confirm: n.confirm && !pad.used, prev_tab: n.prev_tab, next_tab: n.next_tab, ..Nav::default() };
        let mut f = InputFrame { nav, ..input.clone() };
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

    /// Reads the system clock into `now` (tests may have pinned it).
    fn read_clock(&mut self) {
        #[cfg(test)]
        if self.now_pinned {
            return;
        }
        self.now = unix_now();
    }

    /// Brings `notes` in line with the library: nothing is allocated while it has not changed.
    fn sync_notes(&mut self, lib: &[RomEntry]) {
        let noted = |e: &RomEntry| !e.supported && matches!(e.system, ebobo_art::System::Nes);
        let mut cached = self.notes.iter();
        let same = lib.iter().enumerate().filter(|(_, e)| noted(e)).all(|(i, e)| cached.next().is_some_and(|n| n.0 == i && n.1 == e.mapper))
            && cached.next().is_none();
        if !same {
            let word = self.lang.tr(S::Unsupported);
            self.notes.clear();
            self.notes.extend(lib.iter().enumerate().filter(|(_, e)| noted(e)).map(|(i, e)| (i, e.mapper, format!("{word} {}", e.mapper))));
        }
    }

    fn update_picker(&mut self, purpose: Purpose, input: &InputFrame, ctx: &UiCtx, dt: Duration) -> Option<UiAction> {
        let lib = ctx.library;
        self.sync_notes(lib);
        let mut games = recycle(std::mem::take(&mut self.games));
        fill_games(&mut games, lib, &self.notes);
        let action = self.cards.update(input, &games, &mut self.shelf, self.now, dt);
        self.games = recycle(games);
        match action? {
            cards::Action::Start(i) => {
                let entry = lib[i].clone();
                Some(match purpose {
                    Purpose::Solo => UiAction::PlaySolo(entry),
                    Purpose::Host => UiAction::HostRoom(entry),
                    Purpose::Direct => UiAction::HostDirect(entry),
                })
            }
            cards::Action::Rescan => Some(UiAction::Rescan),
            cards::Action::Back => {
                self.pop();
                None
            }
            cards::Action::EditSearch => {
                let text = self.cards.search().to_string();
                self.open_field(Field::Search, text);
                None
            }
            cards::Action::Menu(i) => {
                let entry = &lib[i];
                self.menu = Some(GameMenu {
                    key: ebobo_art::Key { system: entry.system, crc32: entry.crc32 },
                    path: entry.path.clone(),
                    name: entry.name.clone(),
                    sel: 0,
                });
                self.press = None;
                None
            }
        }
    }

    /// The open game menu's frame: up/down move with wrap, confirm does the row, back or
    /// another options press closes it.
    fn update_menu(&mut self, nav: &Nav) -> Option<UiAction> {
        let menu = self.menu.as_mut()?;
        if nav.up {
            menu.sel = (menu.sel + GAME_MENU_ROWS - 1) % GAME_MENU_ROWS;
        }
        if nav.down {
            menu.sel = (menu.sel + 1) % GAME_MENU_ROWS;
        }
        if nav.back || nav.options {
            self.menu = None;
            return None;
        }
        if !nav.confirm {
            return None;
        }
        let menu = self.menu.take()?;
        let name = cards::shown_name(menu.key, &menu.name, &self.shelf, &self.art);
        match menu.sel {
            0 => {
                self.shelf.toggle_favourite(menu.key);
                None
            }
            1 => {
                self.open_field(Field::Rename(menu.key), name.chars().take(ebobo_art::shelf::NAME_MAX).collect());
                None
            }
            2 => {
                let hide = !self.shelf.is_hidden(menu.key);
                self.shelf.set_hidden(menu.key, hide);
                let text = self.t(if hide { S::ToastHidden } else { S::ToastUnhidden });
                // The toast line holds 30 characters; the name gets what the words leave.
                let room = 30 - (text.chars().count() - "{name}".len());
                let msg = text.replace("{name}", &trunc(&name, room));
                self.toast(&msg);
                None
            }
            _ => {
                self.ask_delete(menu.path, &menu.name);
                None
            }
        }
    }

    /// The question before a game's file is deleted: its name, that it goes for good, and the
    /// answer that does it, with Cancel preselected. `name` stands in for a file name that is
    /// not text.
    fn ask_delete(&mut self, path: PathBuf, name: &str) {
        let file = path.file_name().and_then(|f| f.to_str()).unwrap_or(name);
        let lines = delete_lines(self.lang, file);
        self.ask(S::DeleteTitle, lines, Confirm { action: UiAction::DeleteRom(path), yes: S::AnswerForGood, no: S::Cancel });
    }

    /// The rows of the menu of game `key`, in order: favourite, rename, hide, delete.
    fn menu_rows(&self, key: ebobo_art::Key) -> [S; GAME_MENU_ROWS] {
        [
            if self.shelf.is_favourite(key) { S::MenuFavRemove } else { S::MenuFavAdd },
            S::MenuRename,
            if self.shelf.is_hidden(key) { S::MenuUnhide } else { S::MenuHide },
            S::MenuDelete,
        ]
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
        // The box as `draw_field` draws it.
        let on_box = |e: &PointerEvent| matches!(e, PointerEvent::Down(Some((x, y))) if (8..248).contains(x) && (70..90).contains(y));
        // The caret only ever stands at the end, so moving it (arrows, the shoulders of a pad,
        // a tap on the box) is all that is left of it: the selection goes.
        if nav.left || nav.right || nav.prev_tab || nav.next_tab || input.pointer.iter().any(on_box) {
            self.field_all = false;
        }
        if input.backspace {
            if std::mem::take(&mut self.field_all) {
                self.field.clear();
            } else {
                self.field.pop();
            }
        } else if nav.back {
            self.pop();
            return None;
        }
        let max = match field {
            Field::Nick => NICK_MAX,
            Field::Search => 24,
            Field::Rename(_) => ebobo_art::shelf::NAME_MAX,
            Field::Addr | Field::Server => 200,
        };
        for c in input.paste.iter().flat_map(|p| p.chars()).chain(input.text.chars()) {
            if c.is_control() {
                continue;
            }
            if std::mem::take(&mut self.field_all) {
                self.field.clear();
            }
            if self.field.chars().count() < max {
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
            // Back leaves the search as it was; an empty text clears it.
            Field::Search => {
                self.cards.set_search(&value);
                self.pop();
                None
            }
            // Back leaves the name as it was; an empty text gives the game its own name back.
            Field::Rename(key) => {
                self.shelf.set_name(key, &value);
                self.pop();
                None
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
            Item::value(
                self.t(S::Enhance),
                self.t(match cfg.enhance {
                    Mode::Off => S::EnhanceOff,
                    Mode::Hd => S::EnhanceHd,
                    Mode::Depth => S::EnhanceDepth,
                })
                .to_string(),
                true,
            ),
            Item::value(self.t(S::Ambient), onoff(cfg.ambient), true),
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
            Item::value(self.t(S::OnlineArt), self.t(if cfg.online_art { S::On } else { S::Off }).to_string(), true),
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
        let confirm = self.menu_step(Screen::Settings, 10, nav);
        let row = self.sel[Screen::Settings.slot()];
        let delta: i32 = if nav.right || confirm.is_some() { 1 } else if nav.left { -1 } else { 0 };
        let cfg = &mut *ctx.config;
        match row {
            0 if confirm.is_some() => {
                let nick = cfg.nickname.clone();
                self.open_field(Field::Nick, nick);
                None
            }
            9 if confirm.is_some() => {
                self.sel[Screen::Advanced.slot()] = 0;
                self.push(Screen::Advanced);
                None
            }
            0 | 9 => None,
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
                cfg.enhance = if delta > 0 { cfg.enhance.next() } else { cfg.enhance.prev() };
                Some(UiAction::ConfigChanged)
            }
            6 => {
                cfg.ambient = !cfg.ambient;
                Some(UiAction::ConfigChanged)
            }
            7 => {
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
        let confirm = self.menu_step(Screen::Advanced, 8, nav);
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
                cfg.online_art = !cfg.online_art;
                Some(UiAction::ConfigChanged)
            }
            7 if confirm.is_some() || nav.left || nav.right => {
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

    /// The library of cards; with no games at all also where to put them.
    fn draw_picker(&mut self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, purpose: Purpose) {
        let title = self.t(match purpose {
            Purpose::Solo => S::PickRomSolo,
            Purpose::Host => S::PickRomHost,
            Purpose::Direct => S::PickRomDirect,
        });
        let lib = ctx.library;
        self.sync_notes(lib);
        let mut games = recycle(std::mem::take(&mut self.games));
        fill_games(&mut games, lib, &self.notes);
        self.cards.draw(c, f, &games, &self.shelf, &mut self.art, self.lang, title, ctx.scanning, self.now);
        self.games = recycle(games);
        if lib.is_empty() {
            if ctx.scanning {
                let base = self.t(S::Scanning);
                f.draw(c, 64, 60, &format!("{base}{}", self.ticker()), WARN, 1);
            }
            let lines = [S::PutRomsIn, S::PutRomsKinds, S::RomsHome, S::RomsDeck, S::RomsDeckMd, S::RomsDownloads];
            // Six lines 10 px apart between "No games found" (y 124) and the detail line (y 196).
            for (i, s) in lines.into_iter().enumerate() {
                f.draw(c, 16, 136 + i as i32 * 10, self.t(s), SEC, 1);
            }
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
            Field::Search => S::Search,
            Field::Rename(_) => S::RenameTitle,
        });
        // A game's name has a second line saying what an empty field does, like the room
        // code's hint: above the box, clear of the built-in keyboard and the toasts over it.
        let renaming = matches!(field, Field::Rename(_));
        f.draw_centered(c, if renaming { 36 } else { 48 }, title, WHITE, 1);
        if renaming {
            f.draw_centered(c, 50, self.t(S::RenameEmptyNote), SEC, 1);
        }
        c.fill_rect(8, 70, 240, 20, PANEL);
        c.rect(8, 70, 240, 20, ACCENT);
        // Show the tail when the text is longer than the box (28 columns incl. the caret).
        let chars: Vec<char> = self.field.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(27)..].iter().collect();
        if self.field_all {
            c.fill_rect(14, 74, f.width(&shown, 1) + 4, 12, ACCENT);
        }
        f.draw(c, 16, 76, &shown, WHITE, 1);
        f.draw(c, 16 + f.width(&shown, 1), 76, "_", ACCENT, 1);
    }

    fn draw_settings(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, kind: Option<PadKind>, h: &mut Vec<(Area, Hit)>) {
        f.draw_centered(c, 8, self.t(S::Settings), WHITE, 1);
        let items = self.settings_items(ctx.config);
        // Ten rows 12 px apart end at y 140, above the gamepads list (taps stay MIN_TAP tall).
        Self::draw_rows(c, f, &items, self.sel[Screen::Settings.slot()], 24, 12, h);
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

    /// `keyboard`: the device used last is a keyboard, so the line under the code names the
    /// shortcut that copies it.
    fn draw_lobby(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, keyboard: bool, h: &mut Vec<(Area, Hit)>) {
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
                let keys = keyboard.then_some(COPY_KEYS);
                f.draw_centered(c, 40, &tell_code_line(self.t(S::TellCode), keys), SEC, 1);
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
            let mut rows = Vec::with_capacity(2);
            if !self.resume_options.is_empty() {
                rows.push(self.resume_item());
            }
            let mut start = Item::new(self.t(S::Start));
            start.disabled = block.is_some();
            rows.push(start);
            // Two rows start higher so the reason line keeps clear of the toasts.
            let y0 = if rows.len() == 2 { 164 } else { 172 };
            let sel = self.sel[Screen::Lobby.slot()].min(rows.len() - 1);
            Self::draw_rows(c, f, &rows, sel, y0, 16, h);
            if let Some(reason) = block {
                f.draw_centered(c, y0 + rows.len() as i32 * 16 + 4, self.t(reason), WARN, 1);
            }
        } else {
            if let Some(r) = lobby.resume {
                let line = format!("{} ({})", self.t(resume_label(r.kind)), self.unix_age(r.saved_at));
                f.draw_centered(c, 161, &trunc(&line, 30), WHITE, 1);
            }
            f.draw_centered(c, 174, self.t(S::WaitHost), SEC, 1);
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
                (S::LoadAll, _) => Item { disabled: !ctx.game.as_ref().is_some_and(|g| g.can_load), ..Item::new(self.t(S::LoadAll)) },
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
        let items: Vec<Item> = match &m.confirm {
            Some(confirm) => vec![Item::new(self.t(confirm.yes)), Item::new(self.t(confirm.no))],
            None => vec![Item::new(self.t(S::Ok))],
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

    /// A game's menu over the dimmed library: the game's name, where its file is, and the
    /// four rows in the look of the modal's buttons.
    fn draw_menu(&self, c: &mut Canvas, f: &mut Font, m: &GameMenu, hits: &mut Vec<(Area, Hit)>) {
        let h = 8 + 10 + 16 + GAME_MENU_ROWS as i32 * 16 + 4;
        let y0 = (232 - h) / 2;
        hits.push((Area { x: 0, y: 0, w: 256, h: 240 }, Hit::MenuOutside));
        hits.push((Area { x: 12, y: y0, w: 232, h }, Hit::MenuBox));
        c.fill_rect(12, y0, 232, h, PANEL);
        c.rect(12, y0, 232, h, SEC);
        let name = cards::shown_name(m.key, &m.name, &self.shelf, &self.art);
        f.draw_centered(c, y0 + 8, &trunc(&name, 27), WHITE, 1);
        f.draw_centered(c, y0 + 18, &trunc_left(&crate::config::tilde(&m.path), 27), SEC, 1);
        for (i, row) in self.menu_rows(m.key).into_iter().enumerate() {
            let y = y0 + 38 + i as i32 * 16;
            if i == m.sel {
                c.fill_rect(16, y - 4, 224, 16, ACCENT);
                f.draw(c, 20, y, ">", WHITE, 1);
            }
            f.draw(c, 32, y, self.t(row), WHITE, 1);
            hits.push((Area { x: 16, y: y - 4, w: 224, h: 16 }, Hit::MenuRow(i)));
        }
    }

    /// Toasts from `base` upwards, the latest lowest. `solid`: the box hides what is under it.
    fn draw_toasts(&self, c: &mut Canvas, f: &mut Font, base: i32, solid: bool) {
        for (i, (msg, _)) in self.toasts.iter().rev().enumerate() {
            let line = trunc(msg, 30);
            let w = f.width(&line, 1);
            let y = base - i as i32 * 14;
            c.fill_rect((256 - w) / 2 - 4, y - 3, w + 8, 14, if solid { TOAST_BG } else { SHADE });
            f.draw_centered(c, y, &line, WHITE, 1);
        }
    }

    /// The library's hints: confirm (play, or host for the two hosting pickers), the options
    /// button for the game's menu, West with a magnifier (search), the shoulders filter. No
    /// Back: it fits every pad.
    fn library_hints(purpose: Purpose) -> [(Glyph, Option<S>); 4] {
        let confirm = if matches!(purpose, Purpose::Solo) { S::HintPlay } else { S::HintHost };
        [(Glyph::Confirm, Some(confirm)), (Glyph::Options, Some(S::Menu)), (Glyph::West, None), (Glyph::Shoulders, Some(S::Filter))]
    }

    fn draw_hints(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, kind: Option<PadKind>, top: Option<Screen>, h: &mut Vec<(Area, Hit)>) {
        let library = self.modal.is_none() && self.menu.is_none() && matches!(top, Some(Screen::Picker(_)));
        let entries: Vec<(Glyph, Option<S>)> = if let Some(Screen::Picker(purpose)) = top.filter(|_| library) {
            Self::library_hints(purpose).to_vec()
        } else {
            let plain: Vec<(Glyph, S)> = if self.modal.is_some() {
                vec![(Glyph::Confirm, S::Ok)]
            } else if self.menu.is_some() {
                vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Back)]
            } else {
                match top {
                    None => return,
                    Some(Screen::Main) => vec![(Glyph::Confirm, S::Select)],
                    Some(Screen::Picker(_)) => unreachable!("the library's hints and its menu's are chosen above"),
                    Some(Screen::Code) => vec![(Glyph::Confirm, S::Join), (Glyph::Back, S::Back)],
                    Some(Screen::Field(_)) => vec![(Glyph::Confirm, S::Ok), (Glyph::Back, S::Back)],
                    Some(Screen::Settings) if self.sel[Screen::Settings.slot()] == 9 => {
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
                        if host && !self.resume_options.is_empty() && self.sel[Screen::Lobby.slot()] == 0 {
                            v.insert(0, (Glyph::LeftRight, S::Change));
                        } else if host {
                            v.insert(0, (Glyph::Confirm, S::Start));
                        }
                        v
                    }
                    Some(Screen::Pause) => vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Resume)],
                    Some(Screen::ByIp) => vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Back)],
                }
            };
            plain.into_iter().map(|(g, s)| (g, Some(s))).collect()
        };
        let y = 231;
        // The library's entries sit closer, and keyboard keys show as pictograms there, so all
        // four fit with every pad and the keyboard.
        let (gap, space, right) = if library { (4, 2, 252) } else { (12, 4, 250) };
        let mut x = 8;
        for (glyph, s) in entries {
            let label = s.map(|s| self.t(s));
            let lw = label.map_or(7, |l| f.width(l, 1));
            let gw = glyph_width(f, kind, glyph, library);
            if x + gw + space + lw > right {
                break;
            }
            // The whole entry (glyph + label, padded into the gaps, down to the bottom edge)
            // is a button; LeftRight splits into a left and a right half.
            let w = gw + space + lw + gap;
            let area = Area { x: x - gap / 2, y: 224, w, h: 16 };
            let keys: &[Key] = match glyph {
                Glyph::Confirm => &[Key::Confirm],
                Glyph::Back => &[Key::Back],
                Glyph::LeftRight => &[Key::Left, Key::Right],
                Glyph::Options => &[Key::Options],
                Glyph::West => &[Key::Search],
                Glyph::Shoulders => &[Key::PrevTab, Key::NextTab],
            };
            let part = area.w / keys.len() as i32;
            for (k, key) in keys.iter().enumerate() {
                let a = Area { x: area.x + k as i32 * part, w: part, ..area };
                if self.press.is_some_and(|p| p.hit == Hit::Key(*key)) {
                    c.fill_rect(a.x, a.y, a.w, a.h, PANEL);
                }
                h.push((a, Hit::Key(*key)));
            }
            draw_glyph(c, f, x, y, kind, glyph, library);
            x += gw + space;
            match label {
                Some(l) => f.draw(c, x, y, l, SEC, 1),
                None => bitmap(c, x, y, &MAGNIFIER, 7, SEC),
            }
            x += lw + gap;
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
        let cx = if right { x + k } else { x + 3 - k };
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
const LETTER_X: [u8; 5] = [0b101, 0b101, 0b010, 0b101, 0b101];
const LETTER_Y: [u8; 5] = [0b101, 0b101, 0b010, 0b010, 0b010];

/// The text of a glyph drawn as a boxed label: keyboard keys, and the shoulder pair of a pad.
fn glyph_label(kind: Option<PadKind>, glyph: Glyph) -> Option<&'static str> {
    Some(match (kind, glyph) {
        (_, Glyph::LeftRight) => return None,
        (None, Glyph::Confirm) => "Enter",
        (None, Glyph::Back) => "Esc",
        (None, Glyph::Options) => "Space",
        (None, Glyph::West) => "A-Z",
        (None, Glyph::Shoulders) => "Tab",
        (Some(PadKind::PlayStation | PadKind::SteamDeck), Glyph::Shoulders) => "L1 R1",
        (Some(PadKind::Nintendo), Glyph::Shoulders) => "L R",
        (Some(PadKind::Xbox | PadKind::Generic), Glyph::Shoulders) => "LB RB",
        (Some(_), _) => return None,
    })
}

/// The options button of a pad, 7x7: three bars, and the plus Nintendo prints there.
const OPTIONS_BARS: [u8; 7] = [0, 0b1111111, 0, 0b1111111, 0, 0b1111111, 0];
const OPTIONS_PLUS: [u8; 7] = [0b0001000, 0b0001000, 0b0001000, 0b1111111, 0b0001000, 0b0001000, 0b0001000];
/// The search entry's magnifier, 7x7.
const MAGNIFIER: [u8; 7] = [0b0111000, 0b1000100, 0b1000100, 0b1000100, 0b0111000, 0b0000110, 0b0000011];

/// Return arrow, space bar and tab arrow, 7x7, bit 6 = leftmost pixel.
const KEY_ENTER: [u8; 7] = [0b0000001, 0b0000001, 0b0010001, 0b0110001, 0b1111111, 0b0110000, 0b0010000];
const KEY_SPACE: [u8; 7] = [0, 0, 0, 0b1000001, 0b1000001, 0b1111111, 0];
const KEY_TAB: [u8; 7] = [0b0000001, 0b0001001, 0b0001101, 0b1111111, 0b0001101, 0b0001001, 0b0000001];

/// A keyboard key drawn as a pictogram in the library's tight hint bar: Enter, Space, Tab.
fn key_icon(kind: Option<PadKind>, glyph: Glyph, compact: bool) -> Option<&'static [u8; 7]> {
    match (compact, kind, glyph) {
        (true, None, Glyph::Confirm) => Some(&KEY_ENTER),
        (true, None, Glyph::Options) => Some(&KEY_SPACE),
        (true, None, Glyph::Shoulders) => Some(&KEY_TAB),
        _ => None,
    }
}

fn glyph_width(f: &Font, kind: Option<PadKind>, glyph: Glyph, compact: bool) -> i32 {
    if key_icon(kind, glyph, compact).is_some() {
        return 11;
    }
    match (glyph_label(kind, glyph), kind, glyph) {
        (_, _, Glyph::LeftRight) => 12,
        (Some(label), _, _) => f.width(label, 1) + 4,
        (None, Some(_), Glyph::Options) => 8,
        (None, Some(PadKind::PlayStation), _) => 8,
        (None, _, _) => 9,
    }
}

fn draw_glyph(c: &mut Canvas, f: &mut Font, x: i32, y: i32, kind: Option<PadKind>, glyph: Glyph, compact: bool) {
    if let Some(rows) = key_icon(kind, glyph, compact) {
        c.fill_rect(x, y - 1, 11, 10, DIS_BAR);
        bitmap(c, x + 2, y, rows, 7, WHITE);
        return;
    }
    if let Some(label) = glyph_label(kind, glyph) {
        c.fill_rect(x, y - 1, f.width(label, 1) + 4, 10, DIS_BAR);
        f.draw(c, x + 2, y, label, WHITE, 1);
        return;
    }
    match (kind, glyph) {
        (Some(kind), Glyph::Options) => {
            let rows = if kind == PadKind::Nintendo { &OPTIONS_PLUS } else { &OPTIONS_BARS };
            bitmap(c, x, y, rows, 7, WHITE);
        }
        (_, Glyph::LeftRight) => {
            tri(c, x, y, SEC, false);
            tri(c, x + 7, y, SEC, true);
        }
        (Some(PadKind::PlayStation), _) => {
            let (shape, col) = match glyph {
                Glyph::Confirm => (&PS_CROSS, PS_BLUE),
                Glyph::Back => (&PS_CIRCLE, PS_RED),
                _ => (&PS_SQUARE, PS_PINK),
            };
            bitmap(c, x, y, shape, 8, col);
        }
        (kind, _) => {
            // By position: south confirms, east goes back, west is the search. Nintendo
            // prints B A Y there, everyone else A B X.
            let nintendo = kind == Some(PadKind::Nintendo);
            let (letter, col) = match (glyph, nintendo) {
                (Glyph::Confirm, false) | (Glyph::Back, true) => (&LETTER_A, OK),
                (Glyph::Confirm, true) | (Glyph::Back, false) => (&LETTER_B, ERR),
                (Glyph::West, true) => (&LETTER_Y, WARN),
                _ => (&LETTER_X, PS_BLUE),
            };
            for dy in 0..9 {
                for dx in 0..9 {
                    let (px, py) = (dx * 2 - 8, dy * 2 - 8);
                    if px * px + py * py <= 81 {
                        c.fill_rect(x + dx, y - 1 + dy, 1, 1, col);
                    }
                }
            }
            bitmap(c, x + 3, y + 1, letter, 3, BG);
        }
    }
}

/// Cuts `s` to its last `cols` characters, starting with ".." when cut (a path's end says
/// more than its start).
fn trunc_left(s: &str, cols: usize) -> String {
    let n = s.chars().count();
    if n <= cols {
        s.to_string()
    } else {
        let mut out = String::from("..");
        out.extend(s.chars().skip(n - cols.saturating_sub(2)));
        out
    }
}

/// What the delete question says: the file's name with its extension (wrapped at word
/// boundaries, a word longer than a line split across lines, so all of it shows), that it is
/// deleted for good, and that the saves are not touched.
fn delete_lines(lang: Lang, file: &str) -> Vec<String> {
    // A piece of a split word fills its line, so `wrap` never puts a space inside the word.
    let pieces: Vec<String> = file
        .split_whitespace()
        .flat_map(|w| w.chars().collect::<Vec<_>>().chunks(27).map(|c| c.iter().collect()).collect::<Vec<String>>())
        .collect();
    let mut lines = wrap(&pieces.join(" "), 27);
    lines.push(String::new());
    lines.extend(wrap(lang.tr(S::DeleteForGood), 27));
    lines.extend(wrap(lang.tr(S::SavesStay), 27));
    lines
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The emptied list under another lifetime, keeping its allocation (the closure never runs:
/// an in-place collect of nothing).
fn recycle<'a, 'b>(mut games: Vec<cards::Game<'a>>) -> Vec<cards::Game<'b>> {
    games.clear();
    games.into_iter().map(|g| cards::Game { name: "", file: "", path: Path::new(""), note: None, key: g.key, pal: g.pal, runnable: g.runnable, mtime: g.mtime }).collect()
}

/// One card per library entry, in library order; `notes` as `Ui::sync_notes` leaves them.
fn fill_games<'a>(out: &mut Vec<cards::Game<'a>>, lib: &'a [RomEntry], notes: &'a [(usize, u16, String)]) {
    let mut notes = notes.iter().peekable();
    out.extend(lib.iter().enumerate().map(|(i, e)| cards::Game {
        key: ebobo_art::Key { system: e.system, crc32: e.crc32 },
        name: &e.name,
        file: e.path.file_stem().and_then(|s| s.to_str()).unwrap_or(&e.name),
        path: &e.path,
        pal: e.region.is_pal(),
        runnable: e.supported,
        note: notes.next_if(|n| n.0 == i).map(|n| n.2.as_str()),
        mtime: e.mtime,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ebobo_console::{Region, System};
    use ebobo_net::{PeerId, PlayerInfo};
    use std::path::PathBuf;

    const DT: Duration = Duration::from_millis(16);

    fn entry(name: &str, supported: bool, region: Region) -> RomEntry {
        RomEntry { path: PathBuf::from(format!("/r/{name}.nes")), name: name.into(), system: System::Nes, crc32: 1, mapper: if supported { 1 } else { 90 }, supported, region, mtime: 0 }
    }

    /// A build of `name` dated `unix_secs`, in its own folder (as received ROMs are).
    fn build(name: &str, crc32: u32, unix_secs: u64, region: Region) -> RomEntry {
        RomEntry { path: PathBuf::from(format!("/r/{crc32:08x}/{name}.nes")), crc32, mtime: unix_secs, ..entry(name, true, region) }
    }

    /// Two builds of the Chip 'n Dale hack and of Contra, and an unsupported game between
    /// unique names.
    fn library_with_builds() -> Vec<RomEntry> {
        // 2026-11-14 12:00 and 2026-11-09 12:00 UTC: midday keeps the day in any zone ±11 h.
        let (new, old) = (1_794_657_600, 1_794_225_600);
        vec![
            build("Battletoads (USA)", 0x0101_0101, old, Region::Ntsc),
            build("Chip 'n Dale - Rescue Rangers 4P (Europe)", 0x1111_1111, new, Region::Pal),
            build("Chip 'n Dale - Rescue Rangers 4P (Europe)", 0x2222_2222, old, Region::Pal),
            build("Contra (USA)", 0x3333_3333, new, Region::Ntsc),
            build("contra (usa)", 0x4444_4444, old, Region::Ntsc),
            RomEntry { crc32: 0x5555_5555, ..entry("Kirby's Adventure (USA)", false, Region::Ntsc) },
            build("Super Mario Bros. (World)", 0x6666_6666, old, Region::Ntsc),
        ]
    }

    /// `n` runnable games "Game 00", "Game 01", ... with their own checksums.
    fn games(n: u32) -> Vec<RomEntry> {
        (0..n).map(|i| build(&format!("Game {i:02}"), i + 1, 0, Region::Ntsc)).collect()
    }

    fn library(purpose: Purpose) -> Ui {
        let mut ui = Ui::new(Lang::En);
        ui.stack = vec![Screen::Main, Screen::Picker(purpose)];
        ui
    }

    #[test]
    fn ui_library_starts_the_first_game_for_each_purpose() {
        let mut cfg = Config::default();
        let lib = games(3);
        let confirm = nav(|n| n.confirm = true);
        let mut c = ctx(&mut cfg, &lib, None);
        assert_eq!(library(Purpose::Solo).update(&confirm, &mut c, DT), Some(UiAction::PlaySolo(lib[0].clone())));
        assert_eq!(library(Purpose::Host).update(&confirm, &mut c, DT), Some(UiAction::HostRoom(lib[0].clone())));
        assert_eq!(library(Purpose::Direct).update(&confirm, &mut c, DT), Some(UiAction::HostDirect(lib[0].clone())));
    }

    #[test]
    fn ui_library_rescan_tile_and_back() {
        let mut cfg = Config::default();
        let lib = games(1);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        // The tile after the only game is Rescan.
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), None);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::Rescan));
        assert_eq!(ui.update(&nav(|n| n.back = true), &mut c, DT), None);
        assert_eq!(ui.stack, vec![Screen::Main]);
        // An empty library has the Rescan tile alone.
        let mut ui = library(Purpose::Host);
        let mut c = ctx(&mut cfg, &[], None);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::Rescan));
    }

    #[test]
    fn ui_library_search_goes_through_the_text_field() {
        let mut cfg = Config::default();
        let lib = [build("Alpha", 1, 0, Region::Ntsc), build("Beta", 2, 0, Region::Ntsc)];
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        assert!(ui.wants_text_input(), "a keyboard types into the library's search");
        assert_eq!(ui.update(&nav(|n| n.search = true), &mut c, DT), None);
        assert_eq!(ui.stack.last(), Some(&Screen::Field(Field::Search)));
        // Typing (a space too) stays in the field; confirm hands the text to the library.
        ui.update(&InputFrame { text: "be".into(), ..nav(|n| n.fav = true) }, &mut c, DT);
        assert_eq!(ui.cards.search(), "");
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        assert_eq!(ui.cards.search(), "be");
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::PlaySolo(lib[1].clone())), "only Beta is left");
        // Leaving the field with Back keeps the search.
        ui.update(&nav(|n| n.search = true), &mut c, DT);
        ui.update(&InputFrame { text: "zzz".into(), ..InputFrame::default() }, &mut c, DT);
        ui.update(&nav(|n| n.back = true), &mut c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        assert_eq!(ui.cards.search(), "be");
    }

    #[test]
    fn ui_online_art_row_toggles() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::En);
        ui.stack = vec![Screen::Settings, Screen::Advanced];
        ui.sel[Screen::Advanced.slot()] = 6;
        assert_eq!(ui.advanced_items(&cfg)[6].label, Lang::En.tr(S::OnlineArt));
        let mut c = ctx(&mut cfg, &[], None);
        assert!(c.config.online_art, "on by default");
        assert_eq!(ui.update(&nav(|n| n.left = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(!c.config.online_art);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(c.config.online_art);
        // A tap on the row's value toggles it too.
        redraw(&mut ui, &c);
        let row = ui.regions.iter().find(|(_, hit)| matches!(hit, Hit::Row { i: 6, .. })).map(|(a, _)| *a).expect("row");
        assert_eq!(ui.update(&tap(row.x + row.w - 4, row.y + 2), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(!c.config.online_art);
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
        LobbyStatus { stage: Stage::Lobby, code: Some("K7QPA".into()), me: Some(PeerId(0)), is_host: host, players, rom_name: Some("Super Mario Bros. 3 (USA)".into()), can_start, error: None, resume: None }
    }

    #[test]
    fn ui_open_failure_speaks_the_interface_language_and_fits_the_box() {
        use ebobo_console::{md, nes, RomError};
        let read = RomFileError::Read(std::io::Error::from(std::io::ErrorKind::PermissionDenied).to_string());
        let cases: [(OpenFailure, &str, &str); 9] = [
            (OpenFailure::Console(&RomError::Unknown), "это не игра NES или Sega", "not a NES or Genesis game"),
            (OpenFailure::Console(&RomError::Nes(nes::RomError::NotNes)), "это не игра NES или Sega", "not a NES or Genesis game"),
            (OpenFailure::Console(&RomError::Md(md::RomError::NotMegaDrive)), "это не игра NES или Sega", "not a NES or Genesis game"),
            (OpenFailure::File(&RomFileError::NotRom), "это не игра NES или Sega", "not a NES or Genesis game"),
            (OpenFailure::Console(&RomError::Md(md::RomError::Truncated)), "файл игры повреждён", "the game's file is damaged"),
            (OpenFailure::Console(&RomError::Nes(nes::RomError::Invalid("x"))), "файл игры повреждён", "the game's file is damaged"),
            (
                OpenFailure::Console(&RomError::Nes(nes::RomError::UnsupportedMapper(123))),
                "картридж такого типа пока не поддерживается (mapper 123)",
                "this cartridge type is not supported yet (mapper 123)",
            ),
            (OpenFailure::File(&RomFileError::TooLarge), "файл больше 8 МБ", "the file is larger than 8 MB"),
            (OpenFailure::File(&RomFileError::NoRomInZip), "в архиве нет игры", "no game inside the archive"),
        ];
        let unreadable = (OpenFailure::File(&read), "файл не читается (permission denied)", "the file cannot be read (permission denied)");
        for (why, ru, en) in cases.into_iter().chain([unreadable]) {
            for (lang, want) in [(Lang::Ru, ru), (Lang::En, en)] {
                let mut ui = Ui::new(lang);
                let text = ui.open_failure("Chip 'n Dale (Europe)", why);
                assert_eq!(text, format!("Chip 'n Dale (Europe): {want}"));
                ui.error(&text);
                let lines = &ui.modal.as_ref().unwrap().lines;
                assert!(lines.iter().all(|l| l.chars().count() <= 27), "{lines:?}");
                assert_eq!(lines.join(" "), text, "no word is cut by the box");
            }
        }
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
        let volume_y = 24 + 7 * 12;
        let back = |ui: &Ui| {
            ui.regions
                .iter()
                .find_map(|(_, hit)| match hit {
                    Hit::Row { i: 7, back, .. } => *back,
                    _ => None,
                })
                .expect("volume row cycles")
        };
        let mut c = ctx(&mut cfg, &[], None);
        redraw(&mut ui, &c);
        let (from, to) = back(&ui);
        assert_eq!(ui.update(&tap(from, volume_y), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.volume, 0.4);
        assert_eq!(ui.sel[Screen::Settings.slot()], 7);
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
    fn ui_library_tap_starts_and_wheel_scrolls() {
        let mut cfg = Config::default();
        let lib = games(40);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        redraw(&mut ui, &c);
        // Press and release over the second tile of the first row.
        assert_eq!(ui.update(&tap(80, 65), &mut c, DT), Some(UiAction::PlaySolo(lib[1].clone())));
        // The wheel towards the user shows later rows and starts nothing.
        assert_eq!(ui.update(&pointer(&[PointerEvent::Wheel(-3)]), &mut c, DT), None);
        for _ in 0..20 {
            assert_eq!(ui.update(&InputFrame::default(), &mut c, DT), None);
        }
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        redraw(&mut ui, &c);
        let Some(UiAction::PlaySolo(e)) = ui.update(&tap(32, 65), &mut c, DT) else { panic!("a tile under the tap") };
        assert!(e.crc32 > 5, "the first row scrolled away: {}", e.name);
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
    fn ui_library_cannot_start_unsupported() {
        let mut cfg = Config::default();
        let lib = [RomEntry { crc32: 9, ..entry("Bad", false, Region::Ntsc) }, build("Good", 2, 0, Region::Ntsc)];
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        redraw(&mut ui, &c);
        assert_eq!(ui.notes, [(0, 90, "mapper 90".to_string())]);
        // Neither confirm on it nor a tap starts it.
        if ui.cards.selected() == Some(0) {
            assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        }
        assert_eq!(ui.update(&tap(32, 65), &mut c, DT), None);
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        assert_eq!(ui.update(&tap(80, 65), &mut c, DT), Some(UiAction::PlaySolo(lib[1].clone())));
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
            for _ in 0..7 {
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
        ui.sel[Screen::Settings.slot()] = 8;
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
    fn ui_lobby_tell_code_line_fits_with_the_shortcut() {
        for lang in [Lang::Ru, Lang::En] {
            let text = lang.tr(S::TellCode);
            assert_eq!(tell_code_line(text, None), text, "a gamepad sees the line as it was");
            for keys in ["Cmd+C", "Ctrl+C"] {
                let line = tell_code_line(text, Some(keys));
                assert!(line.ends_with(&format!("({keys})")));
                assert!(line.chars().count() <= 32, "{line:?} fits the 32-column screen");
            }
        }
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
        let solo = |slot| GameView { rom_name: "X", netplay: false, names: &[], stats: None, notice: None, desync: false, stalled: false, save_slot: Some((slot, None)), can_load: false };
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
        let game = GameView { rom_name: "X", netplay: true, names: &names, stats: None, notice: None, desync: false, stalled: false, save_slot: None, can_load: false };
        let mut c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(game), pads: &[], config: &mut cfg, version: "0.1.0" };
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "exit must confirm");
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::LeaveGame));
    }

    fn resume_options() -> Vec<ResumeInfo> {
        vec![ResumeInfo { kind: ResumeKind::Last, saved_at: 1_000 }, ResumeInfo { kind: ResumeKind::Save, saved_at: 900 }]
    }

    fn lobby_rows(ui: &Ui) -> usize {
        ui.regions.iter().filter(|(_, h)| matches!(h, Hit::Row { .. })).count()
    }

    #[test]
    fn ui_lobby_resume_row_cycles_for_the_host() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.set_resume_options(resume_options(), Some(0));
        ui.show_lobby();
        let ready = lobby(2, true, true);
        let mut c = ctx(&mut cfg, &[], Some(&ready));
        redraw(&mut ui, &c);
        assert_eq!(lobby_rows(&ui), 2, "resume row above Start");
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::StartLobbyGame), "Start is preselected");
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::PickResume(Some(1))));
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::PickResume(None)), "after the save: new game");
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::PickResume(Some(0))));
        assert_eq!(ui.update(&nav(|n| n.left = true), &mut c, DT), Some(UiAction::PickResume(None)));
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::PickResume(Some(0))), "confirm steps forward");
        // A tap: the left part of the value area steps back, the rest of the row forward.
        redraw(&mut ui, &c);
        let (area, back) = ui
            .regions
            .iter()
            .find_map(|(a, h)| match h {
                Hit::Row { i: 0, back: Some(b), .. } => Some((*a, *b)),
                _ => None,
            })
            .expect("resume row takes taps");
        let y = area.y + area.h / 2;
        assert_eq!(ui.update(&tap(back.0 + 1, y), &mut c, DT), Some(UiAction::PickResume(None)));
        assert_eq!(ui.update(&tap(40, y), &mut c, DT), Some(UiAction::PickResume(Some(0))));
        assert_eq!(ui.resume_pick, Some(0));
    }

    #[test]
    fn ui_lobby_resume_row_hidden_without_saves_and_for_guests() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.show_lobby();
        let ready = lobby(2, true, true);
        let mut c = ctx(&mut cfg, &[], Some(&ready));
        redraw(&mut ui, &c);
        assert_eq!(lobby_rows(&ui), 1, "nothing to continue from: Start only");
        assert_eq!(ui.update(&nav(|n| n.left = true), &mut c, DT), None);
        assert_eq!(ui.update(&nav(|n| n.up = true), &mut c, DT), None);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::StartLobbyGame));

        // A guest sees the host's choice as text and cannot change it.
        let mut guest = lobby(2, false, false);
        guest.resume = Some(ResumeInfo { kind: ResumeKind::Save, saved_at: 1_000 });
        let mut ui = Ui::new(Lang::Ru);
        ui.set_resume_options(resume_options(), Some(0));
        ui.show_lobby();
        let mut c = ctx(&mut cfg, &[], Some(&guest));
        redraw(&mut ui, &c);
        assert_eq!(lobby_rows(&ui), 0);
        for key in [Key::Up, Key::Left, Key::Right, Key::Confirm] {
            assert_eq!(ui.update(&nav(|n| *n = key.nav()), &mut c, DT), None, "{key:?}");
        }
    }

    fn net_game(names: &[String], can_load: bool) -> GameView<'_> {
        GameView { rom_name: "X", netplay: true, names, stats: None, notice: None, desync: false, stalled: false, save_slot: None, can_load }
    }

    #[test]
    fn ui_netplay_pause_saves_and_loads_for_everyone() {
        let mut cfg = Config::default();
        let names = vec!["A".to_string(), "B".to_string()];
        let mut ui = Ui::new(Lang::Ru);
        ui.show_pause();
        let mut c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(net_game(&names, false)), pads: &[], config: &mut cfg, version: "0.1.0" };
        redraw(&mut ui, &c);
        let rows: Vec<bool> = ui.regions.iter().filter_map(|(_, h)| if let Hit::Row { enabled, .. } = h { Some(*enabled) } else { None }).collect();
        assert_eq!(rows, [true, true, false, true, true], "Resume, Save, Load (no save yet), Reset, Leave");
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::SaveState));
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "no save: Load does nothing");
        assert!(ui.modal.is_none());

        c.game = Some(net_game(&names, true));
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "Load asks first");
        let m = ui.modal.as_ref().expect("confirm box");
        assert_eq!((m.title.as_str(), m.sel), ("Загрузить сохранение?", 1), "Cancel preselected");
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "a stray confirm cancels");
        assert!(ui.modal.is_none());
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::LoadState));
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::Reset));
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
        assert_eq!(ui.settings_items(&cfg).len(), 10);
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings];
        ui.sel[Screen::Settings.slot()] = 9;
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
        for _ in 0..8 {
            assert_eq!(ui.update(&nav(|n| n.down = true), &mut c, DT), None);
        }
        assert_eq!(ui.sel[Screen::Settings.slot()], 8);
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(ui.lang, Lang::En, "row 8 is Language");
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.sel[Screen::Settings.slot()], 9);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        assert_eq!(ui.stack.last(), Some(&Screen::Advanced));
        ui.pop();
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.sel[Screen::Settings.slot()], 0, "10 rows wrap");
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
        assert_eq!(items.len(), 8);
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
        ui.sel[Screen::Advanced.slot()] = 7;
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
        let y = 24 + 4 * 12;
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

    /// The rows after Effect: Enhance cycles both ways, by confirm and by taps; Ambient
    /// toggles by button and tap; the rows below (Volume, Language) still do their job by tap.
    #[test]
    fn ui_enhance_and_ambient_rows() {
        let mut cfg = Config::default();
        cfg.volume = 0.5;
        let mut ui = Ui::new(Lang::En);
        ui.stack = vec![Screen::Main, Screen::Settings];
        let mut c = ctx(&mut cfg, &[], None);
        let items = ui.settings_items(c.config);
        for (row, s) in [(4, S::Effect), (5, S::Enhance), (6, S::Ambient), (7, S::Volume), (8, S::Language), (9, S::Advanced)] {
            assert_eq!(items[row].label, Lang::En.tr(s), "row {row}");
        }
        ui.sel[Screen::Settings.slot()] = 5;
        assert_eq!(c.config.enhance, Mode::Off);
        for want in [Mode::Hd, Mode::Depth, Mode::Off] {
            assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
            assert_eq!(c.config.enhance, want);
        }
        for want in [Mode::Depth, Mode::Hd, Mode::Off] {
            assert_eq!(ui.update(&nav(|n| n.left = true), &mut c, DT), Some(UiAction::ConfigChanged));
            assert_eq!(c.config.enhance, want);
        }
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.enhance, Mode::Hd);
        // Taps: the left part of the value steps back, the label area forward.
        let y = |row: i32| 24 + row * 12;
        redraw(&mut ui, &c);
        let (from, _) = ui
            .regions
            .iter()
            .find_map(|(_, hit)| match hit {
                Hit::Row { i: 5, back, .. } => *back,
                _ => None,
            })
            .expect("enhance row cycles");
        assert_eq!(ui.update(&tap(from, y(5)), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.enhance, Mode::Off);
        redraw(&mut ui, &c);
        assert_eq!(ui.update(&tap(30, y(5)), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.enhance, Mode::Hd);
        // Ambient light: on by default, buttons and a tap toggle it.
        assert!(c.config.ambient);
        ui.sel[Screen::Settings.slot()] = 6;
        assert_eq!(ui.update(&nav(|n| n.right = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(!c.config.ambient);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(c.config.ambient);
        redraw(&mut ui, &c);
        assert_eq!(ui.update(&tap(30, y(6)), &mut c, DT), Some(UiAction::ConfigChanged));
        assert!(!c.config.ambient);
        // The rows below follow: Volume steps up, Language switches.
        redraw(&mut ui, &c);
        assert_eq!(ui.update(&tap(30, y(7)), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(c.config.volume, 0.6);
        assert_eq!(ui.sel[Screen::Settings.slot()], 7);
        redraw(&mut ui, &c);
        assert_eq!(ui.update(&tap(30, y(8)), &mut c, DT), Some(UiAction::ConfigChanged));
        assert_eq!(ui.lang, Lang::Ru);
        assert_eq!((c.config.enhance, c.config.ambient), (Mode::Hd, false), "untouched by the rows below");
    }

    #[test]
    fn ui_show_library_opens_solo_library() {
        let mut ui = Ui::new(Lang::En);
        ui.show_pause();
        ui.show_library();
        assert_eq!(ui.stack, vec![Screen::Main, Screen::Picker(Purpose::Solo)]);
        let mut cfg = Config::default();
        let lib = library_with_builds();
        let c = ctx(&mut cfg, &lib, None);
        redraw(&mut ui, &c);
        assert!(ui.regions.iter().any(|(_, hit)| *hit == Hit::Key(Key::Options)), "the library's hints");
    }

    /// The library's hint bar keeps all four entries (no Back) and ends at x ≤ 252 with every
    /// pad kind and the keyboard, in both languages and for every purpose.
    #[test]
    fn ui_library_hints_fit_every_pad() {
        let mut font = Font::new();
        let mut cfg = Config::default();
        let lib = library_with_builds();
        let kinds = [
            Some(PadKind::PlayStation),
            Some(PadKind::Xbox),
            Some(PadKind::Nintendo),
            Some(PadKind::SteamDeck),
            Some(PadKind::Generic),
            None,
        ];
        for lang in [Lang::Ru, Lang::En] {
            for purpose in [Purpose::Solo, Purpose::Host, Purpose::Direct] {
                for kind in kinds {
                    let mut ui = Ui::new(lang);
                    ui.stack = vec![Screen::Main, Screen::Picker(purpose)];
                    let c = ctx(&mut cfg, &lib, None);
                    let mut canvas = Canvas::new();
                    ui.draw(&mut canvas, &mut font, &c, &InputFrame { hint_kind: kind, ..InputFrame::default() });
                    let keys: Vec<(Area, Key)> = ui
                        .regions
                        .iter()
                        .filter_map(|(a, hit)| match hit {
                            Hit::Key(k) if a.y == 224 => Some((*a, *k)),
                            _ => None,
                        })
                        .collect();
                    for want in [Key::Confirm, Key::Options, Key::Search, Key::PrevTab, Key::NextTab] {
                        assert!(keys.iter().any(|(_, k)| *k == want), "{lang:?} {kind:?}: an entry dropped");
                    }
                    assert!(!keys.iter().any(|(_, k)| *k == Key::Back), "no Back hint in the library");
                    // Each area reaches half the 4 px gap past its text.
                    let end = keys.iter().map(|(a, _)| a.x + a.w - 2).max().unwrap_or(0);
                    assert!(end <= 252, "{lang:?} {kind:?}: the last hint ends at {end}");
                }
            }
        }
    }

    fn key_of(e: &RomEntry) -> ebobo_art::Key {
        ebobo_art::Key { system: e.system, crc32: e.crc32 }
    }

    /// The options press on the selected game, which must open its menu.
    fn open_menu(ui: &mut Ui, c: &mut UiCtx) {
        assert_eq!(ui.update(&nav(|n| n.options = true), c, DT), None);
        assert!(ui.menu.is_some(), "options on a game opens its menu");
    }

    #[test]
    fn ui_game_menu_takes_all_input_while_open() {
        let mut cfg = Config::default();
        let lib = games(3);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        open_menu(&mut ui, &mut c);
        assert!(!ui.wants_text_input(), "typing does not search under the menu");
        ui.update(&nav(|n| n.right = true), &mut c, DT);
        ui.update(&InputFrame { text: "zz".into(), ..InputFrame::default() }, &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.back = true), &mut c, DT), None);
        assert!(ui.menu.is_none());
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)), "back closes the menu alone");
        ui.update(&InputFrame::default(), &mut c, DT);
        assert_eq!(ui.cards.selected(), Some(0), "right moved in the menu, not the library");
        assert_eq!(ui.cards.search(), "");
        // Up and down wrap; another options press closes the menu too.
        open_menu(&mut ui, &mut c);
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.menu.as_ref().map(|m| m.sel), Some(3));
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.menu.as_ref().map(|m| m.sel), Some(0));
        ui.update(&nav(|n| n.options = true), &mut c, DT);
        assert!(ui.menu.is_none());
    }

    #[test]
    fn ui_game_menu_favourite_row() {
        let mut cfg = Config::default();
        let lib = games(2);
        let key = key_of(&lib[0]);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        open_menu(&mut ui, &mut c);
        assert_eq!(ui.menu_rows(key)[0], S::MenuFavAdd);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        assert!(ui.menu.is_none());
        assert!(ui.shelf.is_favourite(key));
        open_menu(&mut ui, &mut c);
        assert_eq!(ui.menu_rows(key)[0], S::MenuFavRemove, "the next opening offers to remove it");
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert!(!ui.shelf.is_favourite(key));
    }

    #[test]
    fn ui_game_menu_hide_row() {
        let mut cfg = Config::default();
        let lib = games(2);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        open_menu(&mut ui, &mut c);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        assert!(ui.menu.is_none());
        assert!(ui.shelf.is_hidden(key_of(&lib[0])));
        assert_eq!(ui.toasts.last().map(|t| t.0.as_str()), Some("Hidden: Game 00"));
        // One game is left: it is selected, and the tile after it is Rescan.
        ui.update(&InputFrame::default(), &mut c, DT);
        assert_eq!(ui.cards.selected(), Some(1));
        ui.update(&nav(|n| n.right = true), &mut c, DT);
        assert_eq!(ui.cards.selected(), None);
        // The hide row of a hidden game puts it back, and the toast says so.
        ui.menu = Some(GameMenu { key: key_of(&lib[0]), path: lib[0].path.clone(), name: lib[0].name.clone(), sel: 2 });
        assert_eq!(ui.menu_rows(key_of(&lib[0]))[2], S::MenuUnhide);
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert!(!ui.shelf.is_hidden(key_of(&lib[0])));
        assert_eq!(ui.toasts.last().map(|t| t.0.as_str()), Some("Back in the list: Game 00"));
    }

    #[test]
    fn ui_game_menu_hide_toast_fits() {
        let mut cfg = Config::default();
        let lib = [build("Chip 'n Dale - Rescue Rangers 4P (Europe)", 7, 0, Region::Pal)];
        for lang in [Lang::Ru, Lang::En] {
            let mut ui = Ui::new(lang);
            ui.stack = vec![Screen::Main, Screen::Picker(Purpose::Solo)];
            let mut c = ctx(&mut cfg, &lib, None);
            open_menu(&mut ui, &mut c);
            ui.menu.as_mut().unwrap().sel = 2;
            ui.update(&nav(|n| n.confirm = true), &mut c, DT);
            let toast = &ui.toasts.last().expect("toast").0;
            assert_eq!(toast.chars().count(), 30, "{lang:?}: {toast}");
            assert!(toast.ends_with(".."), "{toast}");
        }
    }

    #[test]
    fn ui_game_menu_rename_row() {
        let mut cfg = Config::default();
        let lib = games(1);
        let key = key_of(&lib[0]);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        let rename = |ui: &mut Ui, c: &mut UiCtx| {
            open_menu(ui, c);
            ui.update(&nav(|n| n.down = true), c, DT);
            ui.update(&nav(|n| n.confirm = true), c, DT);
            assert_eq!(ui.stack.last(), Some(&Screen::Field(Field::Rename(key))));
            assert!(ui.menu.is_none());
        };
        rename(&mut ui, &mut c);
        assert_eq!(ui.field, "Game 00", "prefilled with the shown name");
        // The prefilled name is selected: typing replaces it.
        ui.update(&InputFrame { text: "Mine".into(), ..InputFrame::default() }, &mut c, DT);
        assert_eq!(ui.field, "Mine");
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None);
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        assert_eq!(ui.shelf.name(key), Some("Mine"));
        // A shoulder press of a pad keeps the name too (the built-in keyboard has the d-pad).
        open_menu(&mut ui, &mut c);
        ui.update(&nav(|n| n.down = true), &mut c, DT);
        ui.update(&pad(|n| n.confirm = true), &mut c, DT);
        assert!(ui.osk.is_some(), "opened by a pad: the built-in keyboard shows");
        ui.update(&pad(|n| n.next_tab = true), &mut c, DT);
        ui.update(&InputFrame { text: "!".into(), ..InputFrame::default() }, &mut c, DT);
        assert_eq!(ui.field, "Mine!");
        ui.update(&nav(|n| n.back = true), &mut c, DT);
        // A tap on the box keeps the name to add to; one delete then takes one character.
        rename(&mut ui, &mut c);
        ui.update(&pointer(&[PointerEvent::Down(Some((100, 80))), PointerEvent::Up(Some((100, 80)))]), &mut c, DT);
        ui.update(&InputFrame { text: " 2".into(), ..InputFrame::default() }, &mut c, DT);
        assert_eq!(ui.field, "Mine 2");
        ui.update(&InputFrame { backspace: true, ..InputFrame::default() }, &mut c, DT);
        assert_eq!(ui.field, "Mine ");
        // Back leaves the name as it was.
        ui.update(&nav(|n| n.back = true), &mut c, DT);
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        assert_eq!(ui.shelf.name(key), Some("Mine"));
        // One delete empties a selected name, and an empty text gives the game its own back.
        rename(&mut ui, &mut c);
        ui.update(&InputFrame { backspace: true, ..InputFrame::default() }, &mut c, DT);
        assert_eq!(ui.field, "");
        ui.update(&nav(|n| n.confirm = true), &mut c, DT);
        assert_eq!(ui.shelf.name(key), None);
    }

    #[test]
    fn ui_game_menu_delete_row() {
        let mut cfg = Config::default();
        let lib = games(1);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        let ask = |ui: &mut Ui, c: &mut UiCtx| {
            open_menu(ui, c);
            ui.update(&nav(|n| n.up = true), c, DT);
            assert_eq!(ui.update(&nav(|n| n.confirm = true), c, DT), None);
            assert!(ui.menu.is_none());
            let m = ui.modal.as_ref().expect("the question");
            assert_eq!(m.title, Lang::En.tr(S::DeleteTitle));
            assert_eq!(m.sel, 1, "Cancel is selected");
            assert_eq!(m.confirm.as_ref().map(|q| q.no), Some(S::Cancel));
        };
        ask(&mut ui, &mut c);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), None, "Cancel does nothing");
        assert!(ui.modal.is_none());
        ask(&mut ui, &mut c);
        ui.update(&nav(|n| n.up = true), &mut c, DT);
        assert_eq!(ui.update(&nav(|n| n.confirm = true), &mut c, DT), Some(UiAction::DeleteRom(lib[0].path.clone())));
        for lang in [Lang::Ru, Lang::En] {
            let lines = delete_lines(lang, "Game 00.nes");
            assert_eq!(lines[0], "Game 00.nes");
            assert!(lines.join(" ").contains(lang.tr(S::DeleteForGood)));
            assert!(lines.iter().all(|l| l.chars().count() <= 27));
        }
        // A long file name is shown whole.
        let long = delete_lines(Lang::En, "Chip 'n Dale - Rescue Rangers 4P (Europe).nes");
        assert_eq!(long[..2], ["Chip 'n Dale - Rescue", "Rangers 4P (Europe).nes"], "broken between words");
        let one_word = delete_lines(Lang::En, "Chip_n_Dale_Rescue_Rangers_4P_Europe.nes");
        assert_eq!(one_word[..2].concat(), "Chip_n_Dale_Rescue_Rangers_4P_Europe.nes", "split, not cut");
    }

    #[test]
    fn ui_game_menu_pointer() {
        let mut cfg = Config::default();
        let lib = games(2);
        let key = key_of(&lib[0]);
        let mut ui = library(Purpose::Solo);
        let mut c = ctx(&mut cfg, &lib, None);
        let area = |ui: &Ui, want: Hit| ui.regions.iter().find(|(_, hit)| *hit == want).map(|(a, _)| *a).expect("region");
        // A tap on a row does it.
        open_menu(&mut ui, &mut c);
        redraw(&mut ui, &c);
        let row = area(&ui, Hit::MenuRow(0));
        assert_eq!(ui.update(&tap(row.x + 20, row.y + 4), &mut c, DT), None);
        assert!(ui.menu.is_none());
        assert!(ui.shelf.is_favourite(key));
        // A tap inside the box off the rows does nothing; one outside closes the menu.
        open_menu(&mut ui, &mut c);
        redraw(&mut ui, &c);
        let b = area(&ui, Hit::MenuBox);
        ui.update(&tap(b.x + 4, b.y + 4), &mut c, DT);
        assert!(ui.menu.is_some());
        ui.update(&tap(4, 4), &mut c, DT);
        assert!(ui.menu.is_none());
        assert_eq!(ui.stack.last(), Some(&Screen::Picker(Purpose::Solo)));
        assert!(ui.shelf.is_favourite(key), "closing does nothing to the game");
        // The hint bar says Select and Back while the menu is open.
        open_menu(&mut ui, &mut c);
        redraw(&mut ui, &c);
        let keys: Vec<Hit> = ui.regions.iter().filter(|(a, _)| a.y == 224).map(|(_, h)| *h).collect();
        assert_eq!(keys, vec![Hit::Key(Key::Confirm), Hit::Key(Key::Back)]);
    }

    /// The menu of a library game over the library (`/tmp/ebobo-gamemenu-<state>-<lang>.png`):
    /// on a plain game, on a favourite hidden one, the rename field with the built-in keyboard,
    /// and the delete question.
    #[test]
    #[ignore]
    fn ui_render_game_menu_snapshots() {
        let mut font = Font::new();
        let mut cfg = Config::default();
        let lib = library_with_builds();
        for (tag, lang) in [("ru", Lang::Ru), ("en", Lang::En)] {
            let mut ui = Ui::new(lang);
            ui.now_pinned = true;
            ui.now = 1_794_657_600 + 2 * 86_400;
            ui.stack = vec![Screen::Main, Screen::Picker(Purpose::Solo)];
            let mut c = ctx(&mut cfg, &lib, None);
            let mut shot = |ui: &mut Ui, c: &UiCtx, state: &str| {
                let mut canvas = Canvas::new();
                canvas.clear(BG);
                ui.draw(&mut canvas, &mut font, c, &InputFrame { hint_kind: Some(PadKind::PlayStation), ..InputFrame::default() });
                write_png(&format!("/tmp/ebobo-gamemenu-{state}-{tag}.png"), &canvas.px, 256, 240);
            };
            ui.update(&nav(|n| n.right = true), &mut c, DT);
            ui.update(&nav(|n| n.right = true), &mut c, DT);
            open_menu(&mut ui, &mut c);
            shot(&mut ui, &c, "plain");
            let key = ui.menu.as_ref().unwrap().key;
            ui.shelf.toggle_favourite(key);
            ui.shelf.set_hidden(key, true);
            ui.update(&nav(|n| n.down = true), &mut c, DT);
            shot(&mut ui, &c, "favhidden");
            ui.shelf.set_hidden(key, false);
            let mut confirm = nav(|n| n.confirm = true);
            confirm.pad.used = true;
            ui.update(&confirm, &mut c, DT);
            shot(&mut ui, &c, "rename");
            ui.update(&nav(|n| n.back = true), &mut c, DT);
            ui.ask_delete(lib[2].path.clone(), &lib[2].name);
            shot(&mut ui, &c, "delete");
        }
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

    /// The library of cards (`/tmp/ebobo-library-<case>.png`, the interface layer alone:
    /// pictures are holes): two builds of the same games with the older Chip 'n Dale build
    /// selected (its date on the detail line), Russian and English with PlayStation hints,
    /// English with Xbox and keyboard hints, a search, and the empty library while scanning.
    #[test]
    #[ignore]
    fn ui_render_library_snapshots() {
        let mut font = Font::new();
        let mut cfg = Config::default();
        let lib = library_with_builds();
        let cases = [
            ("ru", Lang::Ru, Some(PadKind::PlayStation), Purpose::Host, false, ""),
            ("en", Lang::En, Some(PadKind::PlayStation), Purpose::Solo, false, ""),
            ("xbox-en", Lang::En, Some(PadKind::Xbox), Purpose::Solo, false, ""),
            ("keys-en", Lang::En, None, Purpose::Direct, false, ""),
            ("keys-ru", Lang::Ru, None, Purpose::Host, false, ""),
            ("nintendo-ru", Lang::Ru, Some(PadKind::Nintendo), Purpose::Host, false, ""),
            ("search-ru", Lang::Ru, Some(PadKind::SteamDeck), Purpose::Solo, false, "con"),
            ("empty-ru", Lang::Ru, Some(PadKind::PlayStation), Purpose::Solo, true, ""),
            ("empty-en", Lang::En, None, Purpose::Solo, true, ""),
        ];
        for (name, lang, kind, purpose, empty, search) in cases {
            let mut ui = Ui::new(lang);
            ui.now_pinned = true;
            ui.now = 1_794_657_600 + 2 * 86_400;
            ui.stack = vec![Screen::Main, Screen::Picker(purpose)];
            let mut c = ctx(&mut cfg, if empty { &[] } else { &lib }, None);
            c.scanning = empty;
            ui.cards.set_search(search);
            if !empty && search.is_empty() {
                ui.update(&nav(|n| n.right = true), &mut c, DT);
                ui.update(&nav(|n| n.right = true), &mut c, DT);
            }
            let mut canvas = Canvas::new();
            canvas.clear(BG);
            ui.draw(&mut canvas, &mut font, &c, &InputFrame { hint_kind: kind, ..InputFrame::default() });
            write_png(&format!("/tmp/ebobo-library-{name}.png"), &canvas.px, 256, 240);
        }
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
            let game = (screen == Screen::Pause).then(|| GameView { rom_name: "Test Game (USA)", netplay: true, names: &names, stats: Some(&stats), notice: None, desync: false, stalled: false, save_slot: None, can_load: true });
            if game.is_some() {
                canvas.clear(rgb(0x5C, 0x94, 0xFC));
            }
            let c = UiCtx { library: &[], scanning: false, lobby: Some(&lob), game, pads: &pads, config: &mut cfg, version: "0.1.0" };
            ui.draw(&mut canvas, &mut font, &c, &hint(PadKind::PlayStation));
            write_png(&format!("/tmp/ebobo-ui-{name}.png"), &canvas.px, 256, 240);
        }
    }

    /// Online saves, Russian and English (`/tmp/ebobo-netsave-<name>-<lang>.png`): the host's
    /// lobby with the resume row (2 and 4 players, long names), the guest's read-only line,
    /// the netplay pause menu with Load enabled and disabled, and the load confirmation.
    #[test]
    #[ignore]
    fn ui_render_netsave_snapshots() {
        let mut font = Font::new();
        let mut cfg = Config::default();
        let pads = [PadInfo { name: "DualSense".into(), kind: PadKind::PlayStation, slot: Some(0) }];
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let options = vec![ResumeInfo { kind: ResumeKind::Last, saved_at: now - 59 * 60 }, ResumeInfo { kind: ResumeKind::Save, saved_at: now - 12 * 86_400 }];
        let long = ["Константин Ивано", "VeryLongNickname", "Евгения Петрова1", "Maximilian_Svens"];
        let four = |host: bool, can_start: bool| {
            let mut l = lobby(3, host, can_start);
            l.players.push(PlayerInfo { id: PeerId(3), ..l.players[1].clone() });
            for (p, name) in l.players.iter_mut().zip(long) {
                p.name = name.into();
            }
            l.rom_name = Some("Chip 'n Dale - Rescue Rangers (Europe)".into());
            l
        };
        let names: Vec<String> = long.iter().map(|s| s.to_string()).collect();
        let stats = NetStats::default();
        for (lang, code) in [(Lang::Ru, "ru"), (Lang::En, "en")] {
            let write = |ui: &mut Ui, font: &mut Font, c: &UiCtx, name: &str, game: bool| {
                let mut canvas = Canvas::new();
                if game {
                    canvas.clear(rgb(0x5C, 0x94, 0xFC));
                }
                ui.draw(&mut canvas, font, c, &hint(PadKind::PlayStation));
                write_png(&format!("/tmp/ebobo-netsave-{name}-{code}.png"), &canvas.px, 256, 240);
            };
            // Host, 2 players: "last moment" picked and focused (◀ ▶ shown).
            let two = lobby(2, true, true);
            let mut ui = Ui::new(lang);
            ui.set_resume_options(options.clone(), Some(0));
            ui.show_lobby();
            ui.sel[Screen::Lobby.slot()] = 0;
            let c = UiCtx { library: &[], scanning: false, lobby: Some(&two), game: None, pads: &pads, config: &mut cfg, version: "0.1.0" };
            write(&mut ui, &mut font, &c, "host2", false);
            // New game picked.
            ui.set_resume_options(options.clone(), None);
            write(&mut ui, &mut font, &c, "host2-new", false);
            // Host, 4 long names, the save picked, Start focused but blocked (reason line).
            let full = four(true, false);
            let mut ui = Ui::new(lang);
            ui.set_resume_options(options.clone(), Some(1));
            ui.show_lobby();
            ui.toast("Геймпад DualSense → игрок 2");
            let c = UiCtx { library: &[], scanning: false, lobby: Some(&full), game: None, pads: &pads, config: &mut cfg, version: "0.1.0" };
            write(&mut ui, &mut font, &c, "host4", false);
            // Guest, 4 long names, the host continues from the last moment.
            let mut guest = four(false, false);
            guest.me = Some(PeerId(1));
            guest.resume = Some(options[0]);
            let mut ui = Ui::new(lang);
            ui.show_lobby();
            let c = UiCtx { library: &[], scanning: false, lobby: Some(&guest), game: None, pads: &pads, config: &mut cfg, version: "0.1.0" };
            write(&mut ui, &mut font, &c, "guest4", false);
            // Netplay pause menu on "Load for everyone": enabled, disabled, then its question.
            for (name, can_load) in [("pause-load", true), ("pause-noload", false)] {
                let mut ui = Ui::new(lang);
                ui.show_pause();
                ui.sel[Screen::Pause.slot()] = 2;
                let game = GameView { stats: Some(&stats), ..net_game(&names, can_load) };
                let c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(game), pads: &pads, config: &mut cfg, version: "0.1.0" };
                write(&mut ui, &mut font, &c, name, true);
            }
            let mut ui = Ui::new(lang);
            ui.show_pause();
            ui.sel[Screen::Pause.slot()] = 2;
            let mut c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(net_game(&names, true)), pads: &pads, config: &mut cfg, version: "0.1.0" };
            ui.update(&nav(|n| n.confirm = true), &mut c, DT);
            assert!(ui.modal.is_some());
            write(&mut ui, &mut font, &c, "pause-confirm", true);
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
        let game = GameView { rom_name: "Test Game (USA)", netplay: true, names: &names, stats: Some(&stats), notice, desync: false, stalled: true, save_slot: None, can_load: false };
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
