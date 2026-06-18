//! Menus, lobby screen, pause menu, on-screen display and toasts, drawn on the 256x240 canvas.
//! Gamepad-first (Steam Deck), keyboard works everywhere. Owner: ui slice (also owns i18n.rs).

use std::time::{Duration, SystemTime};

use ebobo_net::{
    normalize_code, LobbyStatus, NatKind, NetDiag, NetError, NetStats, RomState, RouteKind, Stage, CODE_ALPHABET, CODE_LEN,
};

use crate::canvas::{Canvas, Color, Font};
use crate::config::{is_steam_deck, Config, SpriteLimit};
use crate::i18n::{Lang, S};
use crate::input::{InputFrame, Nav, PadEvent, PadInfo, PadKind};
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
    ToggleFullscreen,
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
    /// Slot this machine controls (netplay).
    pub local_slot: Option<usize>,
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

const PICKER_ROWS: usize = 22;
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
        }
    }

    pub fn set_lang(&mut self, lang: Lang) {
        self.lang = lang;
    }

    /// Main menu (no game running).
    pub fn show_main(&mut self) {
        self.stack = vec![Screen::Main];
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
    }

    /// Pause menu over the running game (solo pauses; netplay keeps running underneath).
    pub fn show_pause(&mut self) {
        self.stack = vec![Screen::Pause];
        self.sel[Screen::Pause.slot()] = 0;
    }

    /// Hides menus (game view: only OSD and toasts are drawn).
    pub fn hide(&mut self) {
        self.stack.clear();
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
        self.toasts.push((message.to_string(), TOAST_SECS));
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
        })
    }

    /// Result of the network check started by `UiAction::RunNetCheck`.
    pub fn set_net_diag(&mut self, diag: NetDiag) {
        self.diag = Some(diag);
        self.diag_running = false;
    }

    pub fn update(&mut self, input: &InputFrame, ctx: &mut UiCtx, dt: Duration) -> Option<UiAction> {
        self.clock += dt.as_secs_f32();
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
        match top {
            Some(Screen::Main) => self.draw_main(canvas, font, ctx),
            Some(Screen::Picker(p)) => self.draw_picker(canvas, font, ctx, p),
            Some(Screen::Code) => self.draw_code(canvas, font),
            Some(Screen::ByIp) => self.draw_by_ip(canvas, font, ctx),
            Some(Screen::Field(f)) => self.draw_field(canvas, font, f),
            Some(Screen::Settings) => self.draw_settings(canvas, font, ctx),
            Some(Screen::Advanced) => self.draw_advanced(canvas, font, ctx),
            Some(Screen::NetCheck) => self.draw_net_check(canvas, font),
            Some(Screen::Lobby) => self.draw_lobby(canvas, font, ctx),
            Some(Screen::Pause) => self.draw_pause(canvas, font, ctx),
            None => {
                if let Some(game) = &ctx.game {
                    self.draw_osd(canvas, font, game);
                }
                if input.menu_hold > 0.0 {
                    self.draw_hold(canvas, font, input.menu_hold);
                }
            }
        }
        if let Some(modal) = &self.modal {
            self.draw_modal(canvas, font, modal);
        }
        self.draw_toasts(canvas, font, visible);
        if visible {
            self.draw_hints(canvas, font, ctx, top);
        }
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
    }

    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.stack.pop();
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
    }

    fn open_field(&mut self, field: Field, prefill: String) {
        self.field = prefill;
        self.push(Screen::Field(field));
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
        let delay = match cfg.input_delay {
            None => self.t(S::Auto).to_string(),
            Some(d) => d.to_string(),
        };
        vec![
            Item::value(self.t(S::Nick), trunc(&cfg.nickname, 11), false),
            Item::value(self.t(S::Fullscreen), onoff(cfg.fullscreen), true),
            Item::value(self.t(S::Scale), format!("{}x", cfg.scale), true),
            Item::value(self.t(S::Volume), format!("{}%", (cfg.volume * 100.0).round() as u32), true),
            Item::value(self.t(S::InputDelay), delay, true),
            Item::value(self.t(S::Language), if self.lang == Lang::Ru { "Русский" } else { "English" }.to_string(), true),
            Item::new(self.t(S::Advanced)),
        ]
    }

    fn advanced_items(&self, cfg: &Config) -> Vec<Item> {
        let server = cfg.server.trim_start_matches("https://").trim_start_matches("http://");
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
        let confirm = self.menu_step(Screen::Settings, 7, nav);
        let row = self.sel[Screen::Settings.slot()];
        let delta: i32 = if nav.right || confirm.is_some() { 1 } else if nav.left { -1 } else { 0 };
        let cfg = &mut *ctx.config;
        match row {
            0 if confirm.is_some() => {
                let nick = cfg.nickname.clone();
                self.open_field(Field::Nick, nick);
                None
            }
            6 if confirm.is_some() => {
                self.sel[Screen::Advanced.slot()] = 0;
                self.push(Screen::Advanced);
                None
            }
            0 | 6 => None,
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
                let steps = ((cfg.volume * 10.0).round() as i32 + delta).clamp(0, 10);
                let volume = steps as f32 / 10.0;
                let changed = volume != cfg.volume;
                cfg.volume = volume;
                changed.then_some(UiAction::ConfigChanged)
            }
            4 => {
                let i = DELAYS.iter().position(|d| *d == cfg.input_delay).unwrap_or(0) as i32;
                let next = DELAYS[(i + delta).rem_euclid(DELAYS.len() as i32) as usize];
                cfg.input_delay = next;
                Some(UiAction::ConfigChanged)
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
        let confirm = self.menu_step(Screen::Advanced, 6, nav);
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

    /// Menu rows from `y0`, `step` px apart; selected row gets the accent bar and `>`.
    fn draw_rows(c: &mut Canvas, f: &mut Font, items: &[Item], sel: usize, y0: i32, step: i32) {
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
            if let Some(v) = &it.value {
                let vx = 236 - f.width(v, 1);
                f.draw(c, vx, y, v, if selected { WHITE } else { SEC }, 1);
                if it.arrows && selected {
                    tri(c, vx - 7, y, WHITE, false);
                    tri(c, 240, y, WHITE, true);
                }
            }
        }
    }

    fn draw_main(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx) {
        f.draw_centered(c, 32, "EBOBO", ACCENT, 2);
        f.draw_centered(c, 56, &format!("v{}", ctx.version), SEC, 1);
        if let Some(v) = &self.update {
            f.draw_centered(c, 70, &self.t(S::UpdatingTo).replace("{name}", v), ACCENT, 1);
        }
        let items: Vec<Item> = MAIN_ITEMS.iter().map(|s| Item::new(self.t(*s))).collect();
        Self::draw_rows(c, f, &items, self.sel[Screen::Main.slot()], 88, 16);
    }

    fn draw_picker(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, purpose: Purpose) {
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
            Self::draw_rows(c, f, &[Item::new(self.t(S::Rescan))], 0, 136, 16);
            return;
        }
        let top = self.picker_top.min(n);
        for row in 0..PICKER_ROWS {
            let i = top + row;
            if i > n {
                break;
            }
            let y = 24 + row as i32 * 9;
            let selected = i == sel;
            if selected {
                c.fill_rect(8, y - 1, 240, 9, ACCENT);
                f.draw(c, 10, y, ">", WHITE, 1);
            }
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
            let track = PICKER_ROWS as i32 * 9;
            let h = (track * PICKER_ROWS as i32 / (n as i32 + 1)).max(4);
            let y = 24 + (track - h) * top as i32 / (n + 1 - PICKER_ROWS) as i32;
            c.fill_rect(250, 23, 1, track, DIS_BAR);
            c.fill_rect(250, y - 1, 1, h, SEC);
        }
    }

    fn draw_code(&self, c: &mut Canvas, f: &mut Font) {
        f.draw_centered(c, 32, self.t(S::EnterCode), WHITE, 1);
        f.draw_centered(c, 48, self.t(S::CodeHint), SEC, 1);
        let (bw, gap) = (28, 8);
        let x0 = (256 - (bw * CODE_LEN as i32 + gap * (CODE_LEN as i32 - 1))) / 2;
        let y = 84;
        for i in 0..CODE_LEN {
            let x = x0 + i as i32 * (bw + gap);
            c.fill_rect(x, y, bw, 32, PANEL);
            if i == self.code_cur {
                c.rect(x, y, bw, 32, ACCENT);
                c.rect(x + 1, y + 1, bw - 2, 30, ACCENT);
                up_tri(c, x + bw / 2 - 3, y - 8, SEC, true);
                up_tri(c, x + bw / 2 - 3, y + 36, SEC, false);
            } else {
                c.rect(x, y, bw, 32, DIS);
            }
            if let Some(ch) = self.code[i] {
                let s = (CODE_ALPHABET[ch as usize] as char).to_string();
                f.draw(c, x + (bw - 16) / 2, y + 8, &s, WHITE, 2);
            }
        }
        let complete = self.code.iter().all(Option::is_some);
        let mut join = Item::new(self.t(S::Join));
        join.disabled = !complete;
        Self::draw_rows(c, f, &[join], 0, 144, 16);
        if is_steam_deck() {
            f.draw_centered(c, 176, self.t(S::DeckKeyboard), SEC, 1);
        }
    }

    fn draw_by_ip(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx) {
        f.draw_centered(c, 32, self.t(S::ByIp), WHITE, 1);
        let items = [
            Item::new(&format!("{} ({})", self.t(S::WaitForPlayers), ctx.config.direct_port)),
            Item::new(self.t(S::ConnectTo)),
        ];
        Self::draw_rows(c, f, &items, self.sel[Screen::ByIp.slot()], 72, 16);
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
        if is_steam_deck() {
            f.draw_centered(c, 104, self.t(S::DeckKeyboard), SEC, 1);
        }
    }

    fn draw_settings(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx) {
        f.draw_centered(c, 8, self.t(S::Settings), WHITE, 1);
        let items = self.settings_items(ctx.config);
        Self::draw_rows(c, f, &items, self.sel[Screen::Settings.slot()], 28, 14);
        f.draw(c, 16, 148, self.t(S::Gamepads), SEC, 1);
        if ctx.pads.is_empty() {
            f.draw(c, 24, 160, self.t(S::NoPads), DIS, 1);
        }
        for (i, pad) in ctx.pads.iter().take(4).enumerate() {
            let y = 160 + i as i32 * 10;
            let col = SLOT[pad.slot.min(3)];
            f.draw(c, 24, y, &(pad.slot + 1).to_string(), col, 1);
            f.draw(c, 40, y, &trunc(&pad.name, 25), WHITE, 1);
        }
    }

    fn draw_advanced(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx) {
        f.draw_centered(c, 8, self.t(S::Advanced), WHITE, 1);
        let items = self.advanced_items(ctx.config);
        Self::draw_rows(c, f, &items, self.sel[Screen::Advanced.slot()], 28, 14);
    }

    fn draw_lobby(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx) {
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
            Self::draw_rows(c, f, &[start], 0, 172, 16);
            if let Some(reason) = block {
                f.draw_centered(c, 192, self.t(reason), WARN, 1);
            }
        } else {
            f.draw_centered(c, 172, self.t(S::WaitHost), SEC, 1);
        }
    }

    fn draw_pause(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx) {
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
        Self::draw_rows(c, f, &items, self.sel[Screen::Pause.slot()].min(items.len() - 1), y0, 16);
        if !netplay {
            let kind = ctx.pads.first().map(|p| p.kind);
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

    fn draw_modal(&self, c: &mut Canvas, f: &mut Font, m: &Modal) {
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
        }
    }

    fn draw_toasts(&self, c: &mut Canvas, f: &mut Font, menus: bool) {
        let base = if menus { 214 } else { 224 };
        for (i, (msg, _)) in self.toasts.iter().rev().enumerate() {
            let line = trunc(msg, 30);
            let w = f.width(&line, 1);
            let y = base - i as i32 * 14;
            c.fill_rect((256 - w) / 2 - 4, y - 3, w + 8, 14, SHADE);
            f.draw_centered(c, y, &line, WHITE, 1);
        }
    }

    fn draw_hints(&self, c: &mut Canvas, f: &mut Font, ctx: &UiCtx, top: Option<Screen>) {
        let entries: Vec<(Glyph, S)> = if self.modal.is_some() {
            vec![(Glyph::Confirm, S::Ok)]
        } else {
            match top {
                None => return,
                Some(Screen::Main) => vec![(Glyph::Confirm, S::Select)],
                Some(Screen::Picker(_)) => vec![(Glyph::Confirm, S::Select), (Glyph::Back, S::Back), (Glyph::LeftRight, S::Page)],
                Some(Screen::Code) => vec![(Glyph::Confirm, S::Join), (Glyph::Back, S::Back)],
                Some(Screen::Field(_)) => vec![(Glyph::Confirm, S::Ok), (Glyph::Back, S::Back)],
                Some(Screen::Settings) if self.sel[Screen::Settings.slot()] == 6 => {
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
        let kind = ctx.pads.first().map(|p| p.kind);
        let y = 231;
        let mut x = 8;
        for (glyph, s) in entries {
            let label = self.t(s);
            let gw = glyph_width(f, kind, glyph);
            if x + gw + 4 + f.width(label, 1) > 250 {
                break;
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
                PadEvent::Connected(p) => {
                    format!("{} {} → {} {}", self.t(S::PadConnected), trunc(&p.name, 12), self.t(S::PlayerN), p.slot + 1)
                }
                PadEvent::Disconnected(p) => {
                    format!("{} {} {}", self.t(S::PadConnected), trunc(&p.name, 12), self.t(S::PadDisconnected))
                }
            };
            self.toast(&msg);
        }
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
                bitmap(c, x, y, &PS_CROSS, 8, rgb(0x7C, 0xA8, 0xF0));
            } else {
                bitmap(c, x, y, &PS_CIRCLE, 8, rgb(0xF0, 0x6E, 0x6E));
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
            for _ in 0..3 {
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
        ui.sel[Screen::Settings.slot()] = 5;
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
        let solo = |slot| GameView { rom_name: "X", netplay: false, names: &[], local_slot: None, stats: None, notice: None, desync: false, stalled: false, save_slot: Some((slot, None)) };
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
        let game = GameView { rom_name: "X", netplay: true, names: &names, local_slot: Some(0), stats: None, notice: None, desync: false, stalled: false, save_slot: None };
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
        assert!(ui.settings_items(&cfg).len() <= 7);
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings];
        ui.sel[Screen::Settings.slot()] = 6;
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
    fn ui_sprite_limit_cycles_in_advanced() {
        let mut cfg = Config::default();
        let mut ui = Ui::new(Lang::Ru);
        ui.stack = vec![Screen::Settings, Screen::Advanced];
        ui.sel[Screen::Advanced.slot()] = 4;
        let items = ui.advanced_items(&cfg);
        assert_eq!(items.len(), 6);
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
        let pads = [PadInfo { name: "DualSense".into(), kind: PadKind::PlayStation, slot: 0 }];
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
            let game = (screen == Screen::Pause).then(|| GameView { rom_name: "Test Game (USA)", netplay: true, names: &names, local_slot: Some(0), stats: Some(&stats), notice: None, desync: false, stalled: false, save_slot: None });
            if game.is_some() {
                canvas.clear(rgb(0x5C, 0x94, 0xFC));
            }
            let c = UiCtx { library: &[], scanning: false, lobby: Some(&lob), game, pads: &pads, config: &mut cfg, version: "0.1.0" };
            ui.draw(&mut canvas, &mut font, &c, &InputFrame::default());
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
        let pads = [PadInfo { name: "DualSense".into(), kind: PadKind::PlayStation, slot: 0 }];
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
                ui.draw(&mut canvas, &mut font, &c, &InputFrame::default());
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
        let game = GameView { rom_name: "Test Game (USA)", netplay: true, names: &names, local_slot: Some(0), stats: Some(&stats), notice, desync: false, stalled: true, save_slot: None };
        let c = UiCtx { library: &[], scanning: false, lobby: None, game: Some(game), pads: &pads, config: &mut cfg, version: "0.1.0" };
        ui.draw(&mut canvas, &mut font, &c, &InputFrame::default());
        write_png("/tmp/ebobo-ui-game-wait.png", &canvas.px, 256, 240);
    }
}
