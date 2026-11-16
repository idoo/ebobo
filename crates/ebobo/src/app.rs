//! Command line, SDL setup, main loop and state machine (menu, lobby, solo game, netplay
//! game). Owner: app slice.

mod cli;
mod logic;
mod netsave;
mod png;
mod rewind;
mod slots;
mod steam_input;
mod update;

/// Save-state slots per game.
pub const SAVE_SLOTS: u8 = slots::SLOTS;

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ebobo_art::{Art, Key, Kind};
use ebobo_console::{Console, Snapshot, System};
use ebobo_net::{
    GameStart, Lobby, LobbyConfig, LobbyMode, LobbyStatus, NetDiag, NetError, NetEvent, NetSession, NetStats, PeerId,
    Resume, ResumeInfo, ResumeKind, Stage,
    INPUT_LOAD, INPUT_RESET, INPUT_SAVE,
};

use crate::audio::Audio;
use crate::canvas::{self, Canvas, Font};
use crate::config::{self, Config};
use crate::enhance::{Enhancer, Mode};
use crate::i18n::Lang;
use crate::input::{Input, InputFrame, PadEvent, PadInfo, PointerEvent};
use crate::library::{self, Library, RomEntry};
use crate::trash::{self, Disposal};
use crate::ui::{GameView, Text, Ui, UiAction, UiCtx};
use crate::video::{Frame, GameFrame, PictureRef, Pictures, Video};

use cli::{Parsed, ResumeFrom, ScriptKey, Start};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BACKDROP: canvas::Color = [0x14, 0x13, 0x1A, 0xFF];
const MENU_FPS: f64 = 60.0;
const BATTERY_EVERY: Duration = Duration::from_secs(10);
/// Net stats (ping, routes) are refreshed this often, in frames.
const STATS_EVERY: u32 = 10;
/// GGRS not advancing this long during netplay: the game is dimmed (it waits for a peer).
const STALL_DIM_AFTER: Duration = Duration::from_millis(500);
/// A gap this long between two loop iterations in netplay means the machine slept.
const SLEEP_GAP: Duration = Duration::from_secs(2);
/// How long the "how to open the menu" notice stays after a game starts.
const MENU_HINT_SECS: f32 = 5.0;
/// The mouse cursor hides this long after the mouse last moved.
const CURSOR_IDLE: Duration = Duration::from_secs(2);
/// The "Deck buttons type keys" hint stays this long.
const KEYS_HINT_SECS: f32 = 8.0;

struct Solo {
    console: Console,
    name: String,
    /// Battery file and the bytes last written (or loaded).
    save_path: Option<PathBuf>,
    saved_ram: Vec<u8>,
    battery_at: Instant,
    /// Current save-state slot (0-based) and when each slot file was written (`None` = empty).
    slot: u8,
    slot_times: [Option<std::time::SystemTime>; slots::SLOTS as usize],
    /// Rewind history: a snapshot and the pads of the frame run right after it.
    rewind: rewind::Ring<(Snapshot, [u16; 4])>,
    /// Frames run since the last snapshot was pushed (one every `rewind::EVERY`).
    since_snapshot: u32,
    /// Rewind is held this tick (audio muted).
    rewinding: bool,
    /// Fast-forward is on (held, or latched in toggle mode) and the button state last tick.
    ff_on: bool,
    ff_held: bool,
}

impl Solo {
    fn slot_path(&self, slot: u8) -> PathBuf {
        slots::path(&config::data_dir().join("states"), &self.name, self.console.crc32(), slot)
    }
}

struct Net {
    console: Console,
    session: NetSession,
    me: PeerId,
    slots: Vec<PeerId>,
    names: Vec<String>,
    rom_name: String,
    /// Interrupted peers and when they will be dropped.
    interrupted: Vec<(PeerId, Instant)>,
    gone: Vec<PeerId>,
    desync: bool,
    /// "Reset / Save / Load for everyone" asked: the bit goes into the next local input word.
    reset_pending: bool,
    save_pending: bool,
    load_pending: bool,
    /// Writing an online save file failed this session (the player is told once).
    write_failed: bool,
    /// `--script` steps and the next one to press.
    script: Vec<(u64, ScriptKey)>,
    script_at: usize,
    stats: NetStats,
    frame: u32,
    /// Frames actually advanced this session (for `ebobo: frame N hash H` lines).
    advanced: u64,
    /// Last time `advance` produced a frame (session start until the first one).
    last_advance: Instant,
}

enum State {
    Menu,
    Lobby(Lobby),
    Solo(Box<Solo>),
    Net(Box<Net>),
}

struct App {
    _sdl: sdl2::Sdl,
    pump: sdl2::EventPump,
    input: Input,
    video: Video,
    audio: Option<Audio>,
    audio_paused: bool,
    library: Library,
    entries: Vec<RomEntry>,
    /// `Library::generation` of `entries`; re-cloned only when it changes.
    entries_gen: u64,
    pads: Vec<PadInfo>,
    font: Font,
    ui: Ui,
    canvas: Canvas,
    /// In-memory config (with command-line overrides).
    config: Config,
    /// `config` as of the last save/apply, to find what the user changed.
    config_before: Config,
    /// What is on disk (never contains command-line overrides).
    config_saved: Config,
    state: State,
    lobby_status: Option<LobbyStatus>,
    /// Interleaved stereo drained from the console this tick.
    samples: Vec<f32>,
    /// Reusable buffer for [`Console::drain_audio`].
    scratch: Vec<f32>,
    /// The console's last picture, RGBA, at its own size.
    frame: Vec<u8>,
    /// Size of the game picture in `frame` that `canvas` was last composed over; `None` in the menus.
    frame_size: Option<(u32, u32)>,
    /// Which layer every pixel of `frame` came from (filled only while an enhancement is on).
    layers: Vec<u8>,
    /// Magnifies and shades `frame` for display when `Config::enhance` is not `Off`.
    enhancer: Enhancer,
    title_rom: Option<String>,
    quit: bool,
    /// `--autostart N`: host starts once N players are in and the game can start.
    autostart: Option<usize>,
    /// Room code already printed to stdout.
    room_printed: bool,
    /// Running network check (Settings ▸ Advanced ▸ Network check).
    net_check: Option<mpsc::Receiver<NetDiag>>,
    /// Self-update from GitHub releases (checked on start unless `config.auto_update` is off).
    updater: update::Updater,
    /// When the real mouse last moved (cursor visibility).
    mouse_at: Option<Instant>,
    /// Steam Deck Desktop Mode: keeps the Deck's buttons a gamepad while the window has focus.
    steam_input: steam_input::SteamInput,
    keys_hint: steam_input::KeysHint,
    /// A game just started: say how to open the menu, once the last-used device is known.
    menu_hint: bool,
    /// `--resume`: what a host starts from instead of the default choice.
    resume_from: Option<ResumeFrom>,
    /// `--script`: F5 / F7 presses at advanced frames of every netplay game.
    script: Vec<(u64, ScriptKey)>,
    /// Host lobby: the saves this game can continue from (`Ui::set_resume_options` has the
    /// same list without the bytes).
    host_resume: Vec<Resume>,
}

/// Runs the app; returns the process exit code.
pub fn run(args: Vec<String>) -> i32 {
    let cli = match cli::parse(&args) {
        Ok(Parsed::Run(cli)) => cli,
        Ok(Parsed::Help) => {
            println!("ebobo {VERSION}\n{}", cli::USAGE);
            return 0;
        }
        Ok(Parsed::Version) => {
            println!("ebobo {VERSION}");
            return 0;
        }
        Err(err) => {
            eprintln!("ebobo: {err}\n{}", cli::USAGE);
            return 2;
        }
    };
    let mut app = match App::new(&cli) {
        Ok(app) => app,
        Err(err) => {
            eprintln!("ebobo: {err}");
            return 1;
        }
    };
    app.begin(cli.start.clone(), cli.library);

    let started = Instant::now();
    let exit_after = cli.exit_after.map(Duration::from_secs_f64);
    let mut last = Instant::now();
    let mut deadline = last;
    while !app.quit {
        let now = Instant::now();
        let dt = now - last;
        last = now;
        app.tick(dt);
        if exit_after.is_some_and(|limit| started.elapsed() >= limit) {
            app.quit = true;
        }
        if app.quit {
            break;
        }
        deadline = logic::next_deadline(deadline, Instant::now(), app.period());
        wait_until(deadline, app.in_game(), || app.net_poll());
    }
    app.end_game();
    // Quitting with a downloaded update: install it (no relaunch) so the next start is current.
    app.updater.install_on_quit();
    if let Some(path) = &cli.screenshot {
        // What the screen shows (effects included), else the plain canvas.
        let pictures = ArtPictures(app.ui.art());
        let game = shown_game(&mut app.enhancer, app.config.enhance, &app.frame, &app.layers, app.frame_size);
        let shot = app.video.capture_frame(&Frame { game, ui: &app.canvas, pictures: &pictures }, cli.screenshot_size);
        let (w, h, px) = match shot {
            Ok(shot) => shot,
            Err(err) => {
                eprintln!("ebobo: screenshot read-back failed ({err}), writing the 256x240 canvas");
                (canvas::W as u32, canvas::H as u32, app.canvas.px.clone())
            }
        };
        let png = png::encode_rgba(w, h, &px);
        if let Err(err) = std::fs::write(path, png) {
            eprintln!("ebobo: cannot write {}: {err}", path.display());
            return 1;
        }
    }
    0
}

/// Waits until `deadline`. In game: coarse sleep until ~1.5 ms before, then yield-spin for
/// accurate pacing. In menus/lobby: one plain sleep (no busy CPU). `poll` returns whether it
/// wants to run again during the wait: while it does (netplay), the coarse sleep is cut into
/// slices of at most 2 ms with a `poll` after each, so acks and ping replies leave at once;
/// otherwise (solo) the coarse sleep is one piece.
fn wait_until(deadline: Instant, precise: bool, mut poll: impl FnMut() -> bool) {
    const SPIN: Duration = Duration::from_micros(1500);
    const SLICE: Duration = Duration::from_millis(2);
    let mut sliced = precise && poll();
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let left = deadline - now;
        if !precise {
            thread::sleep(left);
            return;
        }
        if left > SPIN {
            if sliced {
                thread::sleep((left - SPIN).min(SLICE));
                sliced = poll();
            } else {
                thread::sleep(left - SPIN);
            }
        } else {
            thread::yield_now();
        }
    }
}

fn format_text(template: &str, name: &str, secs: u64) -> String {
    template.replace("{name}", name).replace("{secs}", &secs.to_string())
}

impl App {
    fn new(cli: &cli::Cli) -> Result<App, String> {
        let config_saved = Config::load();
        let mut config = config_saved.clone();
        cli.apply(&mut config);

        let sdl = sdl2::init()?;
        // Input sets the controller hints; it must open the game-controller subsystem first.
        let input = Input::new(&sdl)?;
        let pump = sdl.event_pump()?;
        let mut video = Video::new(&sdl, config.fullscreen, config.scale, config.screen)?;
        video.set_effect(config.effect);
        video.set_ambient(config.ambient);
        let lang = Lang::resolve(config.lang.as_deref());
        let art_dir = config::data_dir().join("art");
        let art = Art::with_loader(art_dir, config.online_art, Box::new(|path| library::load_rom_file(path).ok()));
        let shelf = ebobo_art::shelf::Shelf::load(config::data_dir().join("shelf.tsv"));
        let mut ui = Ui::with_library(lang, art, shelf);
        let audio = match Audio::new(&sdl) {
            Ok(mut audio) => {
                audio.set_volume(config.volume);
                Some(audio)
            }
            Err(err) => {
                eprintln!("ebobo: no audio: {err}");
                ui.toast(match lang {
                    Lang::Ru => "Звук недоступен",
                    Lang::En => "No sound device",
                });
                None
            }
        };
        let mut dirs = library::default_dirs();
        dirs.extend(config.rom_dirs.iter().cloned());
        let library = Library::new(dirs);
        let pads = input.pads();
        let mut updater = update::Updater::new();
        if config.auto_update {
            updater.start(false);
        }
        Ok(App {
            _sdl: sdl,
            pump,
            input,
            video,
            audio,
            audio_paused: false,
            entries_gen: library.generation(),
            entries: library.entries(),
            library,
            pads,
            font: Font::new(),
            ui,
            canvas: Canvas::new(),
            config_before: config.clone(),
            config,
            config_saved,
            state: State::Menu,
            lobby_status: None,
            samples: Vec::with_capacity(4096),
            scratch: Vec::with_capacity(2048),
            frame: Vec::new(),
            frame_size: None,
            layers: Vec::new(),
            enhancer: Enhancer::new(),
            title_rom: None,
            quit: false,
            autostart: cli.autostart,
            room_printed: false,
            net_check: None,
            updater,
            mouse_at: None,
            steam_input: steam_input::SteamInput::new(config::is_steam_deck()),
            keys_hint: steam_input::KeysHint::default(),
            menu_hint: false,
            resume_from: cli.resume,
            script: cli.script.clone(),
            host_resume: Vec::new(),
        })
    }

    /// Initial state from the command line; `library` opens the library over the main menu.
    fn begin(&mut self, start: Start, library: bool) {
        self.ui.show_main();
        if library {
            self.ui.show_library();
        }
        let file = |app: &mut App, path: PathBuf| match app.library.add_file(&path) {
            Ok(entry) => Some(entry),
            Err(err) => {
                app.ui.error(&format!("{}: {err}", path.display()));
                None
            }
        };
        match start {
            Start::Menu => {}
            Start::Solo(path) => {
                if let Some(entry) = file(self, path) {
                    self.start_solo(entry);
                }
            }
            Start::Host(path) => {
                if let Some(entry) = file(self, path) {
                    self.host(entry, false);
                }
            }
            Start::Listen(path) => {
                if let Some(entry) = file(self, path) {
                    self.host(entry, true);
                }
            }
            Start::Join(code) => self.start_lobby(LobbyMode::JoinRoom { code }),
            Start::Connect(addr) => self.start_lobby(LobbyMode::JoinDirect { addr }),
        }
    }

    fn in_game(&self) -> bool {
        matches!(self.state, State::Solo(_) | State::Net(_))
    }

    /// Network work between two frames; false outside netplay (nothing to poll).
    fn net_poll(&mut self) -> bool {
        match &mut self.state {
            State::Net(n) => {
                n.session.poll();
                true
            }
            _ => false,
        }
    }

    fn period(&self) -> Duration {
        match &self.state {
            State::Solo(s) => logic::frame_period(s.console.frame_rate(), 0),
            State::Net(n) => logic::frame_period(n.console.frame_rate(), n.session.frames_ahead()),
            _ => logic::frame_period(MENU_FPS, 0),
        }
    }

    fn tick(&mut self, dt: Duration) {
        let video = &self.video;
        let frame = self.input.poll(&mut self.pump, dt, |pos| video.to_canvas(pos));
        // Finished box art: every tick draws the whole frame, so a new picture shows by itself.
        self.ui.art_mut().set_online(self.config.online_art);
        self.ui.art_mut().poll();
        self.ui.tick(&frame, dt);
        if std::mem::take(&mut self.menu_hint) && self.in_game() {
            let hint = if frame.hint_kind.is_some() { Text::MenuHintPad } else { Text::MenuHintKeys };
            self.ui.toast_secs(self.ui.text(hint), MENU_HINT_SECS);
        }
        if dt > SLEEP_GAP && matches!(self.state, State::Net(_)) {
            // Sleep/suspend: GGRS resyncs or times peers out by itself; the frame debt cap in
            // `next_deadline` prevents a catch-up burst.
            self.ui.toast(self.ui.text(Text::SleepGap));
        }
        self.steam_input.set_focus(self.video.focused());
        self.steam_input.tick(dt);
        if self.steam_input.deck_desktop() && self.keys_hint.update(frame.key_used, frame.pad.used, dt) {
            // Steam ignored the pin (or ebobo has no Steam shortcut): the buttons still type.
            self.ui.toast_secs(self.ui.text(Text::DeckKeys), KEYS_HINT_SECS);
            self.ui.toast_secs(self.ui.text(Text::DeckKeysFix), KEYS_HINT_SECS);
        }
        self.poll_net_check();
        self.poll_update();
        self.input.set_text_input(self.ui.wants_text_input());
        self.input.set_seating(self.in_game() && !self.ui.is_visible());
        if !frame.pad_events.is_empty() {
            self.pads = self.input.pads();
            if let State::Solo(s) = &mut self.state {
                if frame.seated >= 3 {
                    s.console.set_multitap(true);
                }
                if frame.gamepads >= 2 {
                    for event in &frame.pad_events {
                        if let PadEvent::Seated(pad) = event {
                            self.ui.toast_seated(pad);
                        }
                    }
                }
            }
            self.apply_sprite_limit();
        }
        self.refresh_entries();

        if frame.quit {
            self.quit = true;
            return;
        }
        if frame.fullscreen {
            self.toggle_fullscreen();
        }
        if let Some(path) = &frame.dropped_file {
            if matches!(self.state, State::Menu | State::Solo(_)) {
                match self.library.add_file(path) {
                    Ok(entry) => {
                        self.refresh_entries();
                        self.start_solo(entry);
                    }
                    Err(err) => self.ui.error(&format!("{}: {err}", path.display())),
                }
            } else {
                self.ui.toast(self.ui.text(Text::CantOpenRomOnline));
            }
        }

        // Menu toggle over a running game. A tap or a click on the game opens it too: in Game
        // Mode gamescope turns the Deck's touchscreen into mouse clicks.
        let mut menu_opened = false;
        let tapped = !self.ui.is_visible() && frame.pointer.iter().any(|e| matches!(e, PointerEvent::Up(_)));
        if self.in_game() && (frame.menu || tapped) {
            if !self.ui.is_visible() {
                self.ui.show_pause();
                menu_opened = true;
            } else if !frame.nav.back {
                self.ui.hide();
            }
        }
        if self.in_game() {
            // Solo: the current slot. Netplay: for everyone, F7 without asking.
            if frame.save_state {
                self.perform(UiAction::SaveState);
            }
            if frame.load_state {
                self.perform(UiAction::LoadState);
            }
        }
        if frame.screenshot && matches!(self.state, State::Solo(_)) {
            self.screenshot();
        }

        self.poll_lobby();

        if self.ui.is_visible() && !menu_opened {
            let notice = self.net_notice();
            let mut ctx = make_ctx(
                &self.state,
                &self.entries,
                self.library.is_scanning(),
                self.lobby_status.as_ref(),
                &self.pads,
                &mut self.config,
                notice,
            );
            let action = self.ui.update(&frame, &mut ctx, dt);
            if let Some(action) = action {
                self.perform(action);
            }
        }

        self.advance(&frame);
        self.update_battery(false);
        self.compose(&frame);
        if frame.mouse_moved {
            self.mouse_at = Some(Instant::now());
        }
        // Hidden during play (menus over a game keep it, they take clicks).
        let playing = self.in_game() && !self.ui.is_visible();
        self.video.set_cursor_visible(!playing && self.mouse_at.is_some_and(|t| t.elapsed() < CURSOR_IDLE));
        let pictures = ArtPictures(self.ui.art());
        let game = shown_game(&mut self.enhancer, self.config.enhance, &self.frame, &self.layers, self.frame_size);
        if let Err(err) = self.video.present_frame(&Frame { game, ui: &self.canvas, pictures: &pictures }) {
            eprintln!("ebobo: present: {err}");
        }
        self.update_title();
    }

    /// Self-update: blocker toasts, the main-menu status line, and the swap+relaunch, which
    /// happens only in the main menu (never during a game or in a lobby).
    fn poll_update(&mut self) {
        if let Some(blocker) = self.updater.poll() {
            self.ui.toast(self.ui.text(match blocker {
                update::Blocker::MoveToApplications => Text::UpdateMoveToApps,
                update::Blocker::RerunInstaller => Text::UpdateRerunInstaller,
            }));
        }
        if self.updater.take_changed() {
            self.ui.set_update(self.updater.updating_to().map(|v| format!("v{v}")));
        }
        if self.updater.is_ready() && matches!(self.state, State::Menu) && self.ui.on_main() {
            let args: Vec<String> = std::env::args().skip(1).collect();
            self.updater.apply(&args, true);
            self.ui.set_update(None);
        }
    }

    /// Shows a netplay failure; a version mismatch also triggers an update check.
    fn net_error(&mut self, err: &NetError) {
        if *err == NetError::Version && self.config.auto_update {
            self.updater.start(true);
        }
        self.ui.net_error(err);
    }

    fn poll_net_check(&mut self) {
        let Some(rx) = &self.net_check else { return };
        match rx.try_recv() {
            Ok(diag) => {
                self.ui.set_net_diag(diag);
                self.net_check = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.net_check = None;
                self.ui.set_net_diag(NetDiag {
                    server: Err("check failed".into()),
                    websocket: Err("check failed".into()),
                    public_addr: None,
                    local_ip: None,
                    nat: ebobo_net::NatKind::UdpBlocked,
                });
            }
        }
    }

    /// Runs `ebobo_net::diagnose` on a background thread (blocking, up to ~6 s).
    fn start_net_check(&mut self) {
        if self.net_check.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let server = self.config.server.clone();
        // A thread that could not start drops `tx`: the poll then reports the check as failed.
        let _ = thread::Builder::new().name("ebobo-netcheck".into()).spawn(move || {
            let _ = tx.send(ebobo_net::diagnose(&server));
        });
        self.net_check = Some(rx);
    }

    /// Re-clones the library list only when it changed since the last copy.
    fn refresh_entries(&mut self) {
        let generation = self.library.generation();
        if generation != self.entries_gen {
            self.entries = self.library.entries();
            self.entries_gen = generation;
        }
    }

    fn poll_lobby(&mut self) {
        let State::Lobby(lobby) = &mut self.state else {
            self.lobby_status = None;
            return;
        };
        let status = lobby.status();
        if !self.room_printed && status.is_host {
            if let Some(code) = &status.code {
                println!("ebobo: room {code}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                self.room_printed = true;
            }
        }
        if let Some(n) = self.autostart {
            if status.is_host && status.stage == Stage::Lobby && status.can_start && status.players.len() >= n {
                lobby.start_game();
                self.autostart = None;
            }
        }
        if let Some(start) = lobby.take_game() {
            let routes: Vec<String> = status
                .players
                .iter()
                .filter(|p| !p.is_self)
                .map(|p| match p.route {
                    Some(ebobo_net::RouteKind::Direct) => "direct",
                    Some(_) => "relay",
                    None => "unknown",
                })
                .map(String::from)
                .collect();
            println!("ebobo: game started slots={:?} me={:?} route={}", start.slots, start.me, routes.join(","));
            let _ = std::io::Write::flush(&mut std::io::stdout());
            self.lobby_status = None;
            self.start_net(start);
            return;
        }
        if status.stage == Stage::Failed {
            let err = status.error.clone().unwrap_or(NetError::Disconnected);
            self.end_game();
            self.ui.show_main();
            self.net_error(&err);
            return;
        }
        self.lobby_status = Some(status);
    }

    fn net_notice(&self) -> Option<String> {
        let State::Net(n) = &self.state else { return None };
        let (peer, until) = n.interrupted.first()?;
        let secs = until.saturating_duration_since(Instant::now()).as_secs_f64().ceil() as u64;
        Some(format_text(self.ui.text(Text::PeerInterrupted), n.name_of(*peer), secs))
    }

    fn perform(&mut self, action: UiAction) {
        match action {
            UiAction::PlaySolo(entry) => self.start_solo(entry),
            UiAction::HostRoom(entry) => self.host(entry, false),
            UiAction::HostDirect(entry) => self.host(entry, true),
            UiAction::JoinRoom(code) => self.start_lobby(LobbyMode::JoinRoom { code }),
            UiAction::JoinDirect(addr) => {
                self.config.last_join_addr = addr.clone();
                self.persist();
                self.start_lobby(LobbyMode::JoinDirect { addr });
            }
            UiAction::StartLobbyGame => {
                if let State::Lobby(lobby) = &self.state {
                    lobby.start_game();
                }
            }
            UiAction::LeaveLobby | UiAction::LeaveGame => {
                self.end_game();
                self.ui.show_main();
            }
            UiAction::Resume => self.ui.hide(),
            UiAction::Reset => {
                match &mut self.state {
                    State::Solo(s) => {
                        s.console.reset();
                        if let Some(audio) = &mut self.audio {
                            audio.clear();
                        }
                    }
                    State::Net(n) => n.reset_pending = true,
                    _ => {}
                }
                self.ui.hide();
            }
            UiAction::PickResume(pick) => {
                if let State::Lobby(lobby) = &self.state {
                    lobby.set_resume(pick.and_then(|i| self.host_resume.get(i)).cloned());
                }
            }
            UiAction::SelectSlot(slot) => {
                if let State::Solo(s) = &mut self.state {
                    s.slot = slot.min(slots::SLOTS - 1);
                }
            }
            UiAction::SaveState => match &mut self.state {
                State::Net(n) => {
                    n.press(ScriptKey::Save);
                    self.ui.hide();
                }
                State::Solo(s) => {
                    let path = s.slot_path(s.slot);
                    let text = match slots::save(&s.console, &path) {
                        Ok(()) => {
                            s.slot_times[s.slot as usize] = slots::saved_at(&path);
                            self.ui.hide();
                            Text::StateSaved
                        }
                        Err(err) => {
                            eprintln!("ebobo: save state {}: {err}", path.display());
                            Text::StateSaveFailed
                        }
                    };
                    self.ui.toast(self.ui.text(text));
                }
                State::Menu | State::Lobby(_) => {}
            },
            UiAction::LoadState => match &mut self.state {
                State::Net(n) => {
                    if !n.press(ScriptKey::Load) {
                        self.ui.toast(self.ui.text(Text::NoQuickSave));
                    }
                    self.ui.hide();
                }
                State::Solo(s) => {
                    let path = s.slot_path(s.slot);
                    let text = match slots::load(&mut s.console, &path) {
                        Ok(()) => {
                            s.rewind.clear();
                            s.since_snapshot = 0;
                            s.console.clear_audio();
                            if let Some(audio) = &mut self.audio {
                                audio.clear();
                            }
                            self.ui.hide();
                            Text::StateLoaded
                        }
                        Err(err) => {
                            if let slots::LoadError::Io(e) = &err {
                                eprintln!("ebobo: load state {}: {e}", path.display());
                            }
                            err.text()
                        }
                    };
                    self.ui.toast(self.ui.text(text));
                }
                State::Menu | State::Lobby(_) => {}
            },
            UiAction::ConfigChanged => self.apply_config(),
            UiAction::Rescan => self.library.rescan(),
            UiAction::DeleteRom(path, how) => match trash::dispose(&path, how) {
                Ok(()) => {
                    // Off the list at once; the rescan then finds another copy of the game, if
                    // there is one.
                    self.library.forget(&path);
                    self.library.rescan();
                    self.ui.toast(self.ui.text(match how {
                        Disposal::Trash => Text::MovedToTrash,
                        Disposal::Permanent => Text::FileDeleted,
                    }));
                }
                Err(err) => {
                    eprintln!("ebobo: cannot delete {}: {err}", path.display());
                    let text = format!("{}: {err}", self.ui.text(Text::CantDelete));
                    self.ui.error(&text);
                }
            },
            UiAction::RunNetCheck => self.start_net_check(),
            UiAction::Quit => self.quit = true,
        }
    }

    fn toggle_fullscreen(&mut self) {
        let on = !self.video.is_fullscreen();
        self.video.set_fullscreen(on);
        self.config.fullscreen = on;
        self.persist();
    }

    fn apply_config(&mut self) {
        if self.config.fullscreen != self.video.is_fullscreen() {
            self.video.set_fullscreen(self.config.fullscreen);
        }
        if self.config.scale != self.config_before.scale {
            self.video.set_scale(self.config.scale);
        }
        self.video.set_screen_mode(self.config.screen);
        self.video.set_effect(self.config.effect);
        self.video.set_ambient(self.config.ambient);
        if let Some(audio) = &mut self.audio {
            audio.set_volume(self.config.volume);
        }
        if self.config.lang != self.config_before.lang {
            self.ui.set_lang(Lang::resolve(self.config.lang.as_deref()));
        }
        self.apply_sprite_limit();
        self.persist();
    }

    /// Local display setting: each peer may differ, emulated state does not change.
    fn apply_sprite_limit(&mut self) {
        let console = match &mut self.state {
            State::Solo(s) => &mut s.console,
            State::Net(n) => &mut n.console,
            _ => return,
        };
        let limit = self.config.sprite_limit.applies(console.multitap());
        if let Some(nes) = console.as_nes_mut() {
            nes.set_sprite_limit(limit);
        }
    }

    /// Saves what changed since the last save, leaving command-line overrides out.
    fn persist(&mut self) {
        logic::merge_changes(&mut self.config_saved, &self.config_before, &self.config);
        self.config_before = self.config.clone();
        self.config_saved.save();
    }

    fn start_solo(&mut self, entry: RomEntry) {
        self.end_game();
        let rom = match library::load_rom_file(&entry.path) {
            Ok(rom) => rom,
            Err(err) => return self.fail(&format!("{}: {err}", entry.name)),
        };
        let mut console = match Console::new(&rom, logic::region_hint(&entry.name, entry.region)) {
            Ok(console) => console,
            Err(err) => return self.fail(&format!("{}: {err}", entry.name)),
        };
        let multitap = ebobo_console::identify(&rom).map(|id| id.multitap).unwrap_or(false);
        console.set_multitap(multitap);
        if let Some(nes) = console.as_nes_mut() {
            nes.set_sprite_limit(self.config.sprite_limit.applies(multitap));
        }
        let mut save_path = None;
        let mut saved_ram = Vec::new();
        if console.battery_ram().is_some() {
            let path = config::saves_dir().join(logic::save_file_name(&entry.name, console.crc32()));
            if let Ok(bytes) = std::fs::read(&path) {
                console.load_battery_ram(&bytes);
            }
            saved_ram = console.battery_ram().map(<[u8]>::to_vec).unwrap_or_default();
            save_path = Some(path);
        }
        let snapshot_bytes = console.encode_state(&console.snapshot()).len();
        let mut solo = Solo {
            console,
            name: entry.name,
            save_path,
            saved_ram,
            battery_at: Instant::now(),
            slot: 0,
            slot_times: [None; slots::SLOTS as usize],
            rewind: rewind::Ring::new(rewind::BUDGET_BYTES, snapshot_bytes),
            since_snapshot: 0,
            rewinding: false,
            ff_on: false,
            ff_held: false,
        };
        for slot in 0..slots::SLOTS {
            solo.slot_times[slot as usize] = slots::saved_at(&solo.slot_path(slot));
        }
        self.game_started(solo.console.system(), solo.console.crc32());
        self.input.reset_seats();
        self.pads = self.input.pads();
        self.state = State::Solo(Box::new(solo));
        self.ui.hide();
        self.menu_hint = true;
        self.video.power_on();
    }

    fn fail(&mut self, message: &str) {
        self.ui.show_main();
        self.ui.error(message);
    }

    fn host(&mut self, entry: RomEntry, direct: bool) {
        let rom = match library::load_rom_file(&entry.path) {
            Ok(rom) => rom,
            Err(err) => return self.fail(&format!("{}: {err}", entry.name)),
        };
        let rom_name = entry.name;
        // What this game can continue from; a scratch machine of the header region (the one
        // the lobby announces) checks that each file loads.
        let options = match Console::new(&rom, None) {
            Ok(mut scratch) => netsave::candidates(&netsave::dir(), &mut scratch),
            Err(_) => Vec::new(),
        };
        let pick = netsave::pick(&options, self.resume_from);
        self.start_lobby(if direct {
            LobbyMode::HostDirect { port: self.config.direct_port, rom, rom_name }
        } else {
            LobbyMode::HostRoom { rom, rom_name }
        });
        if let State::Lobby(lobby) = &self.state {
            lobby.set_resume(pick.map(|i| options[i].clone()));
        }
        let infos = options.iter().map(|r| ResumeInfo { kind: r.kind, saved_at: r.saved_at }).collect();
        self.ui.set_resume_options(infos, pick);
        self.host_resume = options;
    }

    fn start_lobby(&mut self, mode: LobbyMode) {
        self.end_game();
        self.host_resume = Vec::new();
        self.ui.set_resume_options(Vec::new(), None);
        let lib = self.library.clone();
        let lobby = Lobby::start(LobbyConfig {
            server: self.config.server.clone(),
            nickname: self.config.nickname.clone(),
            mode,
            force_relay: self.config.force_relay,
            input_delay: self.config.input_delay,
            rom_lookup: Box::new(move |crc| {
                lib.find_crc(crc).and_then(|e| library::load_rom_file(&e.path).ok())
            }),
        });
        self.lobby_status = Some(lobby.status());
        self.state = State::Lobby(lobby);
        self.ui.show_lobby();
    }

    /// Starts the netplay game, from `start.state` when the host continues a saved game.
    fn start_net(&mut self, start: GameStart) {
        let resume = start.resume.map(|r| r.kind);
        self.end_game();
        let mut console = match Console::new(&start.rom, Some(start.region)) {
            Ok(console) => console,
            Err(err) => {
                self.ui.show_main();
                return self.net_error(&NetError::BadRom(err.to_string()));
            }
        };
        // PeerId(0) is the host, who already has the file.
        if start.me != PeerId(0) {
            self.keep_received_rom(&start.rom_name, console.crc32(), &start.rom);
        }
        console.set_multitap(start.four_score);
        if let Some(nes) = console.as_nes_mut() {
            nes.set_sprite_limit(self.config.sprite_limit.applies(start.four_score));
        }
        let fps = if start.region.is_pal() { 50 } else { 60 };
        // A continued game: every peer loads the host's bytes before frame 0, and that state is
        // the shared save "Load for everyone" returns to until somebody saves.
        let shared_save = match &start.state {
            None => None,
            Some(state) => match console.load_state(state) {
                Ok(()) => {
                    // The adapter is part of the saved machine; this game's player count
                    // decides, as for a game from power-on (a save made by two players can be
                    // continued by four).
                    console.set_multitap(start.four_score);
                    let kind = resume.unwrap_or(ResumeKind::Save);
                    println!("ebobo: resumed {} sha {}", netsave::kind_name(kind), netsave::sha16(state));
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    let text = if kind == ResumeKind::Last { Text::ResumedLast } else { Text::ResumedSave };
                    self.ui.toast_secs(self.ui.text(text), MENU_HINT_SECS);
                    Some(console.snapshot())
                }
                Err(err) => {
                    eprintln!("ebobo: the host's saved game does not load: {err}");
                    start.link.close();
                    self.ui.show_main();
                    self.ui.error(self.ui.text(slots::LoadError::State(err).text()));
                    return;
                }
            },
        };
        let session = match NetSession::new(start.link, &start.slots, start.input_delay, start.delay_auto, fps, shared_save) {
            Ok(session) => session,
            Err(err) => {
                self.ui.show_main();
                return self.net_error(&err);
            }
        };
        self.game_started(console.system(), console.crc32());
        self.input.reset_seats();
        self.pads = self.input.pads();
        self.state = State::Net(Box::new(Net {
            console,
            stats: session.stats(),
            session,
            me: start.me,
            slots: start.slots,
            names: start.names,
            rom_name: start.rom_name,
            interrupted: Vec::new(),
            gone: Vec::new(),
            desync: false,
            reset_pending: false,
            save_pending: false,
            load_pending: false,
            write_failed: false,
            script: self.script.clone(),
            script_at: 0,
            frame: 0,
            advanced: 0,
            last_advance: Instant::now(),
        }));
        self.ui.hide();
        self.menu_hint = true;
        self.video.power_on();
    }

    /// A game starts (solo, hosted or joined): the shelf notes it as played now and the pads
    /// carry that system's buttons.
    fn game_started(&mut self, system: System, crc32: u32) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        self.ui.shelf_mut().played(Key { system, crc32 }, now);
        self.input.set_system(system);
    }

    /// Keeps a ROM the host sent in this player's own library, so the game is there next time
    /// and for solo play. It goes to `<data dir>/roms/<crc32>/` under the host's name: a newer
    /// build of a game the player already has is listed beside the old one, newest first.
    fn keep_received_rom(&self, name: &str, crc32: u32, rom: &[u8]) {
        if self.library.find_crc(crc32).is_some() {
            return;
        }
        let path = logic::received_rom_path(&config::data_dir().join("roms"), crc32, name);
        let tmp = path.with_extension("tmp");
        let saved = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&tmp, rom))
            .and_then(|()| std::fs::rename(&tmp, &path));
        match saved {
            Ok(()) => self.library.rescan(),
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                eprintln!("ebobo: keeping {}: {err}", path.display());
            }
        }
    }

    /// Leaves whatever runs (battery written, online game kept as the last moment, BYE sent)
    /// and goes to the no-game state.
    fn end_game(&mut self) {
        self.update_battery(true);
        match std::mem::replace(&mut self.state, State::Menu) {
            State::Lobby(lobby) => lobby.leave(),
            State::Net(net) => {
                net.keep_last_moment();
                net.session.leave();
            }
            State::Solo(_) | State::Menu => {}
        }
        self.lobby_status = None;
        if let Some(audio) = &mut self.audio {
            audio.clear();
        }
    }

    fn update_battery(&mut self, force: bool) {
        let State::Solo(s) = &mut self.state else { return };
        let Some(path) = &s.save_path else { return };
        if !force && s.battery_at.elapsed() < BATTERY_EVERY {
            return;
        }
        s.battery_at = Instant::now();
        let Some(ram) = s.console.battery_ram() else { return };
        if ram == s.saved_ram.as_slice() {
            return;
        }
        let result = std::fs::create_dir_all(config::saves_dir()).and_then(|_| std::fs::write(path, ram));
        match result {
            Ok(()) => {
                s.saved_ram.clear();
                s.saved_ram.extend_from_slice(ram);
            }
            Err(err) => eprintln!("ebobo: cannot write {}: {err}", path.display()),
        }
    }

    fn advance(&mut self, frame: &InputFrame) {
        let ui_visible = self.ui.is_visible();
        match &mut self.state {
            State::Solo(s) => {
                s.rewinding = !ui_visible && frame.rewind;
                if s.rewinding {
                    // One snapshot back (2 frames); re-run the frame after it to show it.
                    if let Some((snap, pads)) = s.rewind.step_back() {
                        s.console.restore(&snap);
                        s.console.set_fast_forward(false);
                        s.console.run_frame(pads);
                    }
                    s.console.clear_audio();
                    s.since_snapshot = 0;
                } else if !ui_visible {
                    s.ff_on = logic::fast_forward(self.config.ff_toggle, frame.fast_forward, s.ff_held, s.ff_on);
                    s.ff_held = frame.fast_forward;
                    let n = if s.ff_on { 3 } else { 1 };
                    let pads = frame.solo_pads();
                    for i in 0..n {
                        if s.since_snapshot == 0 {
                            s.rewind.push((s.console.snapshot(), pads));
                        }
                        s.since_snapshot = (s.since_snapshot + 1) % rewind::EVERY;
                        // Only the last frame of a fast-forward tick draws pixels and sound.
                        s.console.set_fast_forward(i + 1 < n);
                        s.console.run_frame(pads);
                    }
                }
            }
            State::Net(n) => {
                n.session.poll();
                // Who made the save `take_saved` holds: its `Saved` event comes with it.
                let mut saver = None;
                for event in n.session.events() {
                    match event {
                        NetEvent::Interrupted { peer, disconnect_in_ms } => {
                            n.interrupted.retain(|(p, _)| *p != peer);
                            n.interrupted.push((peer, Instant::now() + Duration::from_millis(disconnect_in_ms)));
                        }
                        NetEvent::Resumed { peer } => n.interrupted.retain(|(p, _)| *p != peer),
                        NetEvent::Disconnected { peer } => {
                            n.interrupted.retain(|(p, _)| *p != peer);
                            if !n.gone.contains(&peer) {
                                n.gone.push(peer);
                                let text = format_text(self.ui.text(Text::PeerLeft), n.name_of(peer), 0);
                                self.ui.toast(&text);
                            }
                        }
                        NetEvent::Desync { .. } => {
                            if !n.desync {
                                n.desync = true;
                                self.ui.toast(self.ui.text(Text::Desync));
                            }
                        }
                        NetEvent::Saved { slot } => {
                            saver = Some(slot);
                            let text = format_text(self.ui.text(Text::NetSaved), &n.slot_name(slot), 0);
                            self.ui.toast(&text);
                        }
                        NetEvent::Loaded { slot } => {
                            println!("ebobo: net load by slot {slot}");
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                            let text = format_text(self.ui.text(Text::NetLoaded), &n.slot_name(slot), 0);
                            self.ui.toast(&text);
                        }
                        NetEvent::Synchronizing { .. } | NetEvent::Synchronized { .. } => {}
                    }
                }
                if n.slots.iter().all(|p| *p == n.me || n.gone.contains(p)) {
                    let text = format_text(self.ui.text(Text::PeerLeft), &n.others(), 0);
                    self.end_game();
                    self.ui.show_main();
                    self.ui.error(&text);
                    return;
                }
                // Confirmed saves and checkpoints are identical on every peer; each writes its
                // own copy so any of them can host the game next time.
                let mut failed = false;
                if let (Some(slot), Some(snapshot)) = (saver, n.session.take_saved()) {
                    match n.write_state(ResumeKind::Save, &snapshot) {
                        Ok(sha) => println!("ebobo: net save by slot {slot} sha {sha}"),
                        Err(()) => failed = true,
                    }
                }
                if let Some(snapshot) = n.session.take_checkpoint().filter(|_| netsave::last_due(n.advanced)) {
                    match n.write_state(ResumeKind::Last, &snapshot) {
                        Ok(sha) => println!("ebobo: last moment sha {sha}"),
                        Err(()) => failed = true,
                    }
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
                if failed && !std::mem::replace(&mut n.write_failed, true) {
                    self.ui.toast(self.ui.text(Text::StateSaveFailed));
                }
                let mut local: u32 = 0;
                if !ui_visible {
                    local = u32::from(logic::net_input(&frame.pads, &frame.turbo));
                }
                // Each request goes to the session once; it keeps the bit until GGRS takes an
                // input of this player.
                if std::mem::take(&mut n.reset_pending) {
                    local |= INPUT_RESET;
                }
                if std::mem::take(&mut n.save_pending) {
                    local |= INPUT_SAVE;
                }
                if std::mem::take(&mut n.load_pending) {
                    local |= INPUT_LOAD;
                }
                match n.session.advance(&mut n.console, local) {
                    Ok(outcome) => {
                        if outcome.advanced {
                            n.advanced += 1;
                            n.last_advance = Instant::now();
                            if n.advanced % 300 == 0 {
                                println!("ebobo: frame {} hash {:016x}", n.advanced, n.console.state_hash());
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                            }
                            while let Some(&(at, key)) = n.script.get(n.script_at) {
                                if at > n.advanced {
                                    break;
                                }
                                n.script_at += 1;
                                if !n.press(key) {
                                    self.ui.toast(self.ui.text(Text::NoQuickSave));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        self.end_game();
                        self.ui.show_main();
                        self.net_error(&err);
                        return;
                    }
                }
                n.frame = n.frame.wrapping_add(1);
                if n.frame % STATS_EVERY == 0 {
                    let stats = n.session.stats();
                    if stats.input_delay != n.stats.input_delay {
                        let ping = stats.peers.iter().map(|p| p.ping_ms).max().unwrap_or(0);
                        println!("ebobo: input delay {} -> {} (ping {ping} ms)", n.stats.input_delay, stats.input_delay);
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    n.stats = stats;
                }
            }
            State::Menu | State::Lobby(_) => {}
        }
        self.pump_audio(ui_visible);
    }

    fn pump_audio(&mut self, ui_visible: bool) {
        let Some(audio) = &mut self.audio else { return };
        let rewinding = matches!(&self.state, State::Solo(s) if s.rewinding);
        let playing = matches!(self.state, State::Solo(_) | State::Net(_));
        // Solo pauses audio with the game and mutes it while rewinding; netplay keeps playing
        // under the menu.
        let paused = !playing || (ui_visible && matches!(self.state, State::Solo(_))) || rewinding;
        if paused != self.audio_paused {
            audio.set_paused(paused);
            if rewinding {
                audio.clear();
            }
            self.audio_paused = paused;
        }
        let (console, stretch) = match &mut self.state {
            State::Solo(s) if !paused => (&mut s.console, 0.0),
            State::Net(n) => {
                let stretch = logic::frame_stretch(n.session.frames_ahead());
                (&mut n.console, stretch)
            }
            _ => return,
        };
        console.set_sample_rate(audio.sample_rate() * audio.rate_factor() * (1.0 + stretch));
        self.samples.clear();
        console.drain_audio(&mut self.samples, &mut self.scratch);
        audio.push(&self.samples);
    }

    fn compose(&mut self, frame: &InputFrame) {
        // With a game the canvas starts transparent (the game layer shows through where the
        // interface draws nothing); the menus have their backdrop.
        self.frame_size = match &self.state {
            State::Solo(s) => Some(s.console.frame_rgba(&mut self.frame)),
            State::Net(n) => Some(n.console.frame_rgba(&mut self.frame)),
            State::Menu | State::Lobby(_) => None,
        }
        .map(|(w, h)| (w as u32, h as u32));
        // Layers only for the frame that is shown, and only when an enhancement uses them.
        if self.config.enhance != Mode::Off {
            match &self.state {
                State::Solo(s) => _ = s.console.frame_layers(&mut self.layers),
                State::Net(n) => _ = n.console.frame_layers(&mut self.layers),
                State::Menu | State::Lobby(_) => {}
            }
        }
        match self.frame_size {
            Some(_) => self.canvas.clear_transparent(),
            None => self.canvas.clear(BACKDROP),
        }
        let notice = self.net_notice();
        let ctx = make_ctx(
            &self.state,
            &self.entries,
            self.library.is_scanning(),
            self.lobby_status.as_ref(),
            &self.pads,
            &mut self.config,
            notice,
        );
        self.ui.draw(&mut self.canvas, &mut self.font, &ctx, frame);
    }

    /// F12 / Back+North: the game frame ×3 (nearest) as PNG into the pictures folder.
    fn screenshot(&mut self) {
        let State::Solo(s) = &self.state else { return };
        let k = 3;
        let mut rgba = Vec::new();
        let (w, h) = s.console.frame_rgba(&mut rgba);
        let mut big = Vec::new();
        logic::scale_nearest(&rgba, w, h, k, &mut big);
        let png = png::encode_rgba((w * k) as u32, (h * k) as u32, &big);
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = config::pictures_dir();
        let path = dir.join(logic::screenshot_file_name(&s.name, secs));
        match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, png)) {
            Ok(()) => {
                println!("ebobo: screenshot {}", path.display());
                let text = format_text(self.ui.text(Text::ScreenshotSaved), &config::tilde(&path), 0);
                self.ui.toast(&text);
            }
            Err(err) => {
                eprintln!("ebobo: cannot write {}: {err}", path.display());
                self.ui.toast(self.ui.text(Text::ScreenshotFailed));
            }
        }
    }

    fn update_title(&mut self) {
        let rom = match &self.state {
            State::Solo(s) => Some(s.name.as_str()),
            State::Net(n) => Some(n.rom_name.as_str()),
            _ => None,
        };
        if rom == self.title_rom.as_deref() {
            return;
        }
        let title = match rom {
            Some(name) => format!("ebobo — {name}"),
            None => "ebobo".to_string(),
        };
        self.video.set_title(&title);
        self.title_rom = rom.map(str::to_string);
    }
}

impl Net {
    fn name_of(&self, peer: PeerId) -> &str {
        self.slots
            .iter()
            .position(|p| *p == peer)
            .and_then(|i| self.names.get(i))
            .map(String::as_str)
            .unwrap_or("?")
    }

    /// Names of every remote player, comma separated.
    fn others(&self) -> String {
        let names: Vec<&str> = self.slots.iter().filter(|p| **p != self.me).map(|p| self.name_of(*p)).collect();
        names.join(", ")
    }

    /// Name of the player on NES controller `slot`, short enough for a toast.
    fn slot_name(&self, slot: usize) -> String {
        self.names.get(slot).map_or("?", String::as_str).chars().take(12).collect()
    }

    /// F5 / F7 in a netplay game (menu, hotkey or `--script`): every peer saves / loads at the
    /// frame this player's input reaches. A load without a shared save is refused (`false`).
    fn press(&mut self, key: ScriptKey) -> bool {
        match key {
            ScriptKey::Save => self.save_pending = true,
            ScriptKey::Load if self.session.has_save() => self.load_pending = true,
            ScriptKey::Load => return false,
        }
        true
    }

    /// Leaving the game: this very frame becomes the last moment, so the next session
    /// continues here and not at a checkpoint up to 5 s old (a boss beaten in those seconds
    /// would be alive again). The newest frames ran on predicted inputs of the others, so
    /// unlike the checkpoints this file is this player's own view.
    fn keep_last_moment(&self) {
        if !netsave::last_due(self.advanced) {
            return;
        }
        if let Ok(sha) = self.write_state(ResumeKind::Last, &self.console.snapshot()) {
            println!("ebobo: last moment on leave sha {sha}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }

    /// Writes an online save file of this ROM; `Ok` holds the first 16 hex digits of its
    /// SHA-256. The first failure of a session is printed (later ones every 5 s would flood).
    fn write_state(&self, kind: ResumeKind, snapshot: &Snapshot) -> Result<String, ()> {
        let bytes = self.console.encode_state(snapshot);
        let dir = netsave::dir();
        match netsave::write(&dir, self.console.crc32(), kind, &bytes) {
            Ok(()) => Ok(netsave::sha16(&bytes)),
            Err(err) => {
                if !self.write_failed {
                    eprintln!("ebobo: cannot write {}: {err}", netsave::path(&dir, self.console.crc32(), kind).display());
                }
                Err(())
            }
        }
    }
}

/// The interface's pictures (box art on the library's cards) for `Video`, by `Placed::id`:
/// `(system as u64) << 32 | crc32`.
struct ArtPictures<'a>(&'a Art);

impl Pictures for ArtPictures<'_> {
    fn picture(&self, id: u64) -> Option<PictureRef<'_>> {
        let system = match id >> 32 {
            0 => System::Nes,
            1 => System::MegaDrive,
            _ => return None,
        };
        let (picture, kind) = self.0.get(Key { system, crc32: id as u32 })?;
        let stamp = match kind {
            Kind::BoxArt => 1,
            Kind::TitleScreen => 2,
        };
        Some(PictureRef { w: picture.w, h: picture.h, rgba: &picture.rgba, stamp })
    }
}

/// The game's picture as the screen shows it: `frame` (`size` when a game is on screen), run
/// through the enhancer with `layers` unless `mode` is `Off`, which shows it as it is.
fn shown_game<'a>(
    enhancer: &'a mut Enhancer,
    mode: Mode,
    frame: &'a [u8],
    layers: &[u8],
    size: Option<(u32, u32)>,
) -> Option<GameFrame<'a>> {
    let (w, h) = size?;
    if mode == Mode::Off || layers.len() < (w * h) as usize {
        return Some(GameFrame { rgba: frame, w, h, scale: 1 });
    }
    let e = enhancer.apply(mode, frame, layers, w as usize, h as usize);
    Some(GameFrame { rgba: e.rgba, w: e.w as u32, h: e.h as u32, scale: e.scale })
}

fn make_ctx<'a>(
    state: &'a State,
    entries: &'a [RomEntry],
    scanning: bool,
    lobby: Option<&'a LobbyStatus>,
    pads: &'a [PadInfo],
    config: &'a mut Config,
    notice: Option<String>,
) -> UiCtx<'a> {
    let game = match state {
        State::Solo(s) => Some(GameView {
            rom_name: &s.name,
            netplay: false,
            names: &[],
            stats: None,
            notice: None,
            desync: false,
            stalled: false,
            save_slot: Some((s.slot, s.slot_times[s.slot as usize])),
            can_load: false,
        }),
        State::Net(n) => Some(GameView {
            rom_name: &n.rom_name,
            netplay: true,
            names: &n.names,
            stats: Some(&n.stats),
            notice,
            desync: n.desync,
            stalled: n.last_advance.elapsed() > STALL_DIM_AFTER,
            save_slot: None,
            can_load: n.session.has_save(),
        }),
        State::Menu | State::Lobby(_) => None,
    };
    UiCtx { library: entries, scanning, lobby, game, pads, config, version: VERSION }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_format_text_placeholders() {
        assert_eq!(format_text("{name}: lost, {secs} s", "Sasha", 7), "Sasha: lost, 7 s");
        assert_eq!(format_text("{name} left", "A, B", 0), "A, B left");
    }
}
