//! Command line, SDL setup, main loop and state machine (menu, lobby, solo game, netplay
//! game). Owner: app slice.

mod cli;
mod logic;
mod moment;
mod netsave;
mod png;
mod rewind;
pub(crate) mod slots;
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
    Resume, ResumeInfo, ResumeKind, SavePolicy, Stage,
    INPUT_LOAD, INPUT_RESET, INPUT_SAVE,
};

use crate::audio::Audio;
use crate::canvas::{self, Canvas, Font};
use crate::config::{self, Config};
use crate::enhance::{Enhancer, Mode};
use crate::i18n::Lang;
use crate::input::{Input, InputFrame, PadEvent, PadInfo, PointerEvent};
use crate::library::{self, Library, RomEntry};
use crate::ui::{GameView, OpenFailure, Text, Ui, UiAction, UiCtx};
use crate::video::{Frame, GameFrame, PictureRef, Pictures, Video};

use cli::{Parsed, ResumeFrom, ScriptKey, Start};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BACKDROP: canvas::Color = [0x14, 0x13, 0x1A, 0xFF];
const MENU_FPS: f64 = 60.0;
const BATTERY_EVERY: Duration = Duration::from_secs(10);
/// A solo game's moment is written this often during play.
const MOMENT_EVERY: Duration = Duration::from_secs(30);
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
    /// Where this game's moment (the state to continue from next time) is written and when
    /// it last was; `None` = this run keeps no moment.
    moment: Option<PathBuf>,
    moment_at: Instant,
    /// Current save-state slot (0-based) and when each slot file was written (`None` = empty).
    slot: u8,
    slot_times: [Option<std::time::SystemTime>; slots::SLOTS as usize],
    /// Each slot's picture (`slots::THUMB_W` x `slots::THUMB_H` RGBA); `None` = empty slot or
    /// a save made before pictures existed.
    slot_thumbs: [Option<Vec<u8>>; slots::SLOTS as usize],
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
    /// The room this game came from. It lives as long as the game, so everyone can go back
    /// to it on the same connections; dropping it leaves the room.
    lobby: Lobby,
    me: PeerId,
    slots: Vec<PeerId>,
    /// Indexes into `slots` nobody plays: they carry the host's id and get no presses.
    idle: Vec<usize>,
    /// Who may save and load in this game (the room's setting).
    saves: SavePolicy,
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
    /// Solo games start from power-on and keep no moment (`--fresh`, or a timed run of a
    /// game named on the command line).
    fresh: bool,
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
    /// The window is not the front one: a solo game stands still until it is again.
    away: bool,
    /// `--autostart N`: host starts once N players are in and the game can start.
    autostart: Option<usize>,
    /// Room code already printed to stdout.
    room_printed: bool,
    /// Host: the game went back to the room for a newcomer, so it starts again by itself as
    /// soon as the room can start.
    auto_continue: bool,
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
    /// Players at this device in an online room (1-4): `--seats`, then the room's row.
    seats_here: u8,
    /// This run is the first of a new version over an older install: `begin` says what is new.
    whats_new: bool,
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
        // Asked first, once per run in every kind of start: the library and the shelf below
        // write the files a brand-new install is told apart by.
        let whats_new = crate::whatsnew::due(&config::data_dir(), VERSION);
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
            fresh: !logic::keeps_moment(cli.fresh, matches!(cli.start, Start::Solo(_)), cli.exit_after.is_some()),
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
            away: false,
            autostart: cli.autostart,
            room_printed: false,
            auto_continue: false,
            net_check: None,
            updater,
            mouse_at: None,
            steam_input: steam_input::SteamInput::new(config::is_steam_deck()),
            keys_hint: steam_input::KeysHint::default(),
            menu_hint: false,
            resume_from: cli.resume,
            script: cli.script.clone(),
            host_resume: Vec::new(),
            seats_here: cli.seats.unwrap_or(1),
            whats_new,
        })
    }

    /// Initial state from the command line; `library` opens the library over the main menu.
    /// With nothing to start, ebobo opens on the library when it already knows a game (from
    /// its cache), else on the main menu; a scan finishing later does not change the screen.
    fn begin(&mut self, start: Start, library: bool) {
        self.ui.show_main();
        if library || (matches!(start, Start::Menu) && !self.entries.is_empty()) {
            self.ui.show_library();
        }
        let file = |app: &mut App, path: PathBuf| match app.library.add_file(&path) {
            Ok(entry) => Some(entry),
            Err(err) => {
                let text = app.ui.open_failure(&path.display().to_string(), OpenFailure::File(&err));
                app.ui.error(&text);
                None
            }
        };
        match start {
            Start::Menu => {
                if self.whats_new {
                    self.ui.whats_new(VERSION);
                }
            }
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
        if let Some(focused) = frame.focus {
            // Leaving the window pauses a solo game: drop the sound still queued, once.
            if !focused && !self.away && matches!(self.state, State::Solo(_)) {
                if let Some(audio) = &mut self.audio {
                    audio.clear();
                }
            }
            self.away = !focused;
        }
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
                    Err(err) => {
                        let text = self.ui.open_failure(&path.display().to_string(), OpenFailure::File(&err));
                        self.ui.error(&text);
                    }
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
        if frame.screenshot {
            self.screenshot();
        }
        if frame.copy {
            if let Some(code) = self.lobby_status.as_ref().and_then(|l| l.code.as_deref()) {
                self.input.copy_text(code);
                self.ui.toast(self.ui.text(Text::CodeCopied));
            }
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
                self.away,
            );
            let action = self.ui.update(&frame, &mut ctx, dt);
            if let Some(action) = action {
                self.perform(action);
            }
        }

        self.advance(&frame);
        self.update_battery(false);
        if matches!(&self.state, State::Solo(s) if !s.rewinding && s.moment_at.elapsed() >= MOMENT_EVERY) {
            self.save_moment();
        }
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
    /// happens only while the user is idle at a start screen (never in a game or a lobby).
    fn poll_update(&mut self) {
        if let Some(blocker) = self.updater.poll() {
            self.ui.toast(self.ui.text(match blocker {
                update::Blocker::MoveToApplications => Text::UpdateMoveToApps,
                update::Blocker::RerunInstaller => Text::UpdateRerunInstaller,
                update::Blocker::DownloadAgain => Text::UpdateDownloadAgain,
            }));
        }
        if self.updater.take_changed() {
            self.ui.set_update(self.updater.updating_to().map(|v| format!("v{v}")));
        }
        if self.updater.is_ready() && matches!(self.state, State::Menu) && self.ui.idle_at_home() {
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
        // Back in the room for a newcomer: continue once it is ready (or was refused or left).
        if self.auto_continue && status.is_host && status.regrouped && status.can_start {
            lobby.start_game();
            self.auto_continue = false;
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
            // The room handle moves into the game: the room is not left.
            if let State::Lobby(lobby) = std::mem::replace(&mut self.state, State::Menu) {
                self.start_net(start, lobby);
            }
            return;
        }
        if status.stage == Stage::Failed {
            let err = status.error.clone().unwrap_or(NetError::Disconnected);
            self.end_game();
            self.ui.show_main();
            self.net_error(&err);
            return;
        }
        // A host stays in the lobby when a guest cannot play; it is told why (and an update
        // check starts when the versions differ).
        if status.is_host {
            let before = self.lobby_status.as_ref().and_then(|s| s.error.as_ref());
            if let Some(err) = logic::host_notice(before, status.error.as_ref()).cloned() {
                self.net_error(&err);
            }
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
                // A solo game returns to the list of games; an online game or a room to the
                // main menu.
                let solo = matches!(self.state, State::Solo(_));
                self.end_game();
                if solo {
                    self.ui.show_library();
                } else {
                    self.ui.show_main();
                }
            }
            UiAction::Resume => self.ui.hide(),
            UiAction::BackToRoom => self.regroup(false),
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
            UiAction::SetSeats(n) => {
                if let State::Lobby(lobby) = &self.state {
                    lobby.set_seats(n);
                    self.seats_here = n;
                }
            }
            UiAction::MoveSeat(seat, up) => {
                if let State::Lobby(lobby) = &self.state {
                    lobby.move_seat(seat, up);
                }
            }
            UiAction::SetSaves(policy) => {
                if let State::Lobby(lobby) = &self.state {
                    lobby.set_saves(policy);
                    self.config.online_saves = policy.as_str().to_string();
                    self.persist();
                }
            }
            UiAction::SelectSlot(slot) => {
                if let State::Solo(s) = &mut self.state {
                    s.slot = slot.min(slots::SLOTS - 1);
                }
            }
            UiAction::SaveState => match &mut self.state {
                State::Net(n) => {
                    if let Err(text) = n.press(ScriptKey::Save) {
                        self.ui.toast(self.ui.text(text));
                    }
                    self.ui.hide();
                }
                State::Solo(s) => {
                    let path = s.slot_path(s.slot);
                    let text = match slots::save(&s.console, &path) {
                        Ok(()) => {
                            let slot = s.slot as usize;
                            s.slot_times[slot] = slots::saved_at(&path);
                            // The picture now on screen goes next to the state. Without it the
                            // save still counts, and the slot must not keep its old picture.
                            let mut frame = Vec::new();
                            let (w, h) = s.console.frame_rgba(&mut frame);
                            let thumb = slots::shrink(&frame, w, h);
                            let thumb_path = slots::thumb_path(&path);
                            s.slot_thumbs[slot] = match slots::save_thumb(&thumb_path, &thumb) {
                                Ok(()) => Some(thumb),
                                Err(err) => {
                                    eprintln!("ebobo: save picture {}: {err}", thumb_path.display());
                                    let _ = std::fs::remove_file(&thumb_path);
                                    None
                                }
                            };
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
                    if let Err(text) = n.press(ScriptKey::Load) {
                        self.ui.toast(self.ui.text(text));
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
            UiAction::DeleteRom(path) => match std::fs::remove_file(&path) {
                Ok(()) => {
                    // Off the list at once; the rescan then finds another copy of the game, if
                    // there is one.
                    self.library.forget(&path);
                    self.library.rescan();
                    self.ui.toast(self.ui.text(Text::FileDeleted));
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
            Err(err) => return self.fail(&self.ui.open_failure(&entry.name, OpenFailure::File(&err))),
        };
        let mut console = match Console::new(&rom, logic::region_hint(&entry.name, entry.region)) {
            Ok(console) => console,
            Err(err) => return self.fail(&self.ui.open_failure(&entry.name, OpenFailure::Console(&err))),
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
        // Continue where the game was left, over the battery RAM just loaded.
        let moment = (!self.fresh).then(|| moment::path(&moment::dir(), &entry.name, console.crc32()));
        let mut continued = false;
        if let Some(path) = &moment {
            match slots::load(&mut console, path) {
                Ok(()) => continued = true,
                Err(slots::LoadError::Empty) => {}
                Err(err) => eprintln!("ebobo: moment {} not loaded: {err:?}", path.display()),
            }
        }
        let snapshot_bytes = console.encode_state(&console.snapshot()).len();
        let mut solo = Solo {
            console,
            name: entry.name,
            save_path,
            saved_ram,
            battery_at: Instant::now(),
            moment,
            moment_at: Instant::now(),
            slot: 0,
            slot_times: [None; slots::SLOTS as usize],
            slot_thumbs: Default::default(),
            rewind: rewind::Ring::new(rewind::BUDGET_BYTES, snapshot_bytes),
            since_snapshot: 0,
            rewinding: false,
            ff_on: false,
            ff_held: false,
        };
        for slot in 0..slots::SLOTS {
            let path = solo.slot_path(slot);
            solo.slot_times[slot as usize] = slots::saved_at(&path);
            solo.slot_thumbs[slot as usize] = slots::load_thumb(&slots::thumb_path(&path));
        }
        self.game_started(solo.console.system(), solo.console.crc32());
        self.input.reset_seats();
        self.pads = self.input.pads();
        self.state = State::Solo(Box::new(solo));
        self.ui.hide();
        self.menu_hint = true;
        self.video.power_on();
        if continued {
            self.ui.toast(self.ui.text(Text::Continued));
        }
    }

    /// A game that did not open: the list of games when there are any, else the main menu,
    /// with the error box on top.
    fn fail(&mut self, message: &str) {
        self.refresh_entries();
        if self.entries.is_empty() {
            self.ui.show_main();
        } else {
            self.ui.show_library();
        }
        self.ui.error(message);
    }

    fn host(&mut self, entry: RomEntry, direct: bool) {
        let rom = match library::load_rom_file(&entry.path) {
            Ok(rom) => rom,
            Err(err) => return self.fail(&self.ui.open_failure(&entry.name, OpenFailure::File(&err))),
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
            lobby.set_saves(SavePolicy::parse(Some(&self.config.online_saves)));
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
        // More than one player at this device: the host seats them, a guest asks the host.
        if self.seats_here != 1 {
            lobby.set_seats(self.seats_here);
        }
        self.lobby_status = Some(lobby.status());
        self.state = State::Lobby(lobby);
        self.ui.show_lobby();
    }

    /// Starts the netplay game, from `start.state` when the host continues a saved game.
    /// `lobby` is the room it starts from; when the game cannot start, the room is left.
    fn start_net(&mut self, start: GameStart, lobby: Lobby) {
        let resume = start.resume.map(|r| r.kind);
        self.auto_continue = false;
        if let Some(audio) = &mut self.audio {
            audio.clear();
        }
        let mut console = match Console::new(&start.rom, Some(start.region)) {
            Ok(console) => console,
            Err(err) => {
                start.link.close();
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
        let session = match NetSession::new(start.link, &start.slots, start.input_delay, start.delay_auto, fps, shared_save, start.saves) {
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
            lobby,
            me: start.me,
            slots: start.slots,
            idle: start.idle,
            saves: start.saves,
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
        let saved = slots::write(&path, rom);
        match saved {
            Ok(()) => self.library.rescan(),
            Err(err) => {
                eprintln!("ebobo: keeping {}: {err}", path.display());
            }
        }
    }

    /// Leaves whatever runs (battery written, online game kept as the last moment, BYE sent,
    /// the room left) and goes to the no-game state.
    fn end_game(&mut self) {
        self.save_moment();
        self.update_battery(true);
        match std::mem::replace(&mut self.state, State::Menu) {
            State::Lobby(lobby) => lobby.leave(),
            State::Net(net) => {
                net.keep_last_moment();
                let Net { session, lobby, .. } = *net;
                session.leave();
                lobby.leave();
            }
            State::Solo(_) | State::Menu => {}
        }
        self.lobby_status = None;
        self.auto_continue = false;
        if let Some(audio) = &mut self.audio {
            audio.clear();
        }
    }

    /// Takes the online game back to its room on the same connections (a newcomer knocked, or
    /// the host chose "Back to the room"). The host hands the room the game's current state
    /// to continue from, and keeps it as the last moment too, so nothing is lost if the room
    /// falls apart. `auto`: the host starts the game again by itself once the room can start.
    fn regroup(&mut self, auto: bool) {
        if !matches!(self.state, State::Net(_)) {
            return;
        }
        let State::Net(net) = std::mem::replace(&mut self.state, State::Menu) else { return };
        let Net { console, session, lobby, me, gone, saves, .. } = *net;
        let resume = (me == PeerId(0)).then(|| {
            let state = console.save_state();
            if saves != SavePolicy::Off {
                let dir = netsave::dir();
                match netsave::write(&dir, console.crc32(), ResumeKind::Last, &state) {
                    Ok(()) => println!("ebobo: last moment on regroup sha {}", netsave::sha16(&state)),
                    Err(err) => eprintln!("ebobo: cannot write {}: {err}", netsave::path(&dir, console.crc32(), ResumeKind::Last).display()),
                }
            }
            let saved_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            Resume { kind: ResumeKind::Last, saved_at, state }
        });
        self.auto_continue = auto && resume.is_some();
        lobby.regroup(session.into_link(), gone, resume);
        self.lobby_status = Some(lobby.status());
        self.state = State::Lobby(lobby);
        self.ui.show_lobby();
        if let Some(audio) = &mut self.audio {
            audio.clear();
        }
        println!("ebobo: back to the room");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }

    /// Writes the solo game's moment, the state it continues from next time; nothing when
    /// no solo game runs or this run keeps no moment.
    fn save_moment(&mut self) {
        let State::Solo(s) = &mut self.state else { return };
        s.moment_at = Instant::now();
        let Some(path) = &s.moment else { return };
        if let Err(err) = slots::save(&s.console, path) {
            eprintln!("ebobo: moment {} not written: {err}", path.display());
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
        let result = slots::write(path, ram);
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
        // A solo game stands still under the menu and while the window is not in front.
        let halted = ui_visible || (self.away && matches!(self.state, State::Solo(_)));
        match &mut self.state {
            State::Solo(s) => {
                s.rewinding = !halted && frame.rewind;
                if s.rewinding {
                    // One snapshot back (2 frames); re-run the frame after it to show it.
                    if let Some((snap, pads)) = s.rewind.step_back() {
                        s.console.restore(&snap);
                        s.console.set_fast_forward(false);
                        s.console.run_frame(pads);
                    }
                    s.console.clear_audio();
                    s.since_snapshot = 0;
                } else if !halted {
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
                // Going back to the room happens after the events: `n` is borrowed here.
                let mut regroup = None;
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
                        // A newcomer knocks: the host takes everyone back to the room. The
                        // host's call to go back reaches a guest as `Regroup`.
                        NetEvent::Knock { .. } if n.me == PeerId(0) => regroup = Some(true),
                        NetEvent::Regroup if n.me != PeerId(0) => regroup = Some(false),
                        NetEvent::Knock { .. } | NetEvent::Regroup => {}
                    }
                }
                if let Some(auto) = regroup {
                    self.regroup(auto);
                    return;
                }
                // A guest left alone has nobody to play with. The host plays on: the room is
                // open, and whoever left can join again with the same code.
                if n.me != PeerId(0) && n.slots.iter().all(|p| *p == n.me || n.gone.contains(p)) {
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
                // With saves off nothing of this game is written.
                let checkpoint = n.session.take_checkpoint().filter(|_| n.saves != SavePolicy::Off && netsave::last_due(n.advanced));
                if let Some(snapshot) = checkpoint {
                    match n.write_state(ResumeKind::Last, &snapshot) {
                        Ok(sha) => println!("ebobo: last moment sha {sha}"),
                        Err(()) => failed = true,
                    }
                }
                let _ = std::io::Write::flush(&mut std::io::stdout());
                if failed && !std::mem::replace(&mut n.write_failed, true) {
                    self.ui.toast(self.ui.text(Text::StateSaveFailed));
                }
                // Each request goes to the session once; it keeps the bit until GGRS takes an
                // input of this player.
                let mut request: u32 = 0;
                if std::mem::take(&mut n.reset_pending) {
                    request |= INPUT_RESET;
                }
                if std::mem::take(&mut n.save_pending) {
                    request |= INPUT_SAVE;
                }
                if std::mem::take(&mut n.load_pending) {
                    request |= INPUT_LOAD;
                }
                // One word per controller this device holds: an idle seat gets no presses,
                // the real ones this device's players in order; the request rides on the first.
                let locals = n.session.local_slots();
                let players = locals.iter().filter(|s| !n.idle.contains(s)).count();
                let mut real = if ui_visible { vec![0; players] } else { logic::net_words(&frame.pads, &frame.turbo, players) }.into_iter();
                let mut words: Vec<u32> = locals.iter().map(|s| if n.idle.contains(s) { 0 } else { real.next().unwrap_or(0) }).collect();
                if let Some(first) = locals.iter().position(|s| !n.idle.contains(s)) {
                    words[first] |= request;
                }
                match n.session.advance(&mut n.console, &words) {
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
                                if let Err(text) = n.press(key) {
                                    self.ui.toast(self.ui.text(text));
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
        self.pump_audio(halted);
    }

    /// `halted`: the menu is open, or a solo game waits for its window to come back.
    fn pump_audio(&mut self, halted: bool) {
        let Some(audio) = &mut self.audio else { return };
        let rewinding = matches!(&self.state, State::Solo(s) if s.rewinding);
        let playing = matches!(self.state, State::Solo(_) | State::Net(_));
        // Solo pauses audio with the game and mutes it while rewinding; netplay keeps playing
        // under the menu.
        let paused = !playing || (halted && matches!(self.state, State::Solo(_))) || rewinding;
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
            self.away,
        );
        self.ui.draw(&mut self.canvas, &mut self.font, &ctx, frame);
    }

    /// F12 / Back+North: the running game's frame ×3 (nearest) as PNG into the pictures folder,
    /// solo and online alike.
    fn screenshot(&mut self) {
        let (console, name) = match &self.state {
            State::Solo(s) => (&s.console, &s.name),
            State::Net(n) => (&n.console, &n.rom_name),
            State::Menu | State::Lobby(_) => return,
        };
        let k = 3;
        let mut rgba = Vec::new();
        let (w, h) = console.frame_rgba(&mut rgba);
        let mut big = Vec::new();
        logic::scale_nearest(&rgba, w, h, k, &mut big);
        let png = png::encode_rgba((w * k) as u32, (h * k) as u32, &big);
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dir = config::pictures_dir();
        let path = dir.join(logic::screenshot_file_name(name, secs));
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
    /// A peer's name, from its first seat somebody plays (an idle seat carries the host's id
    /// and no name).
    fn name_of(&self, peer: PeerId) -> &str {
        (0..self.slots.len())
            .find(|i| self.slots[*i] == peer && !self.idle.contains(i))
            .and_then(|i| self.names.get(i))
            .map(String::as_str)
            .unwrap_or("?")
    }

    /// Names of every remote player, comma separated, each once (a device may hold several
    /// seats).
    fn others(&self) -> String {
        let mut peers: Vec<PeerId> = Vec::new();
        for p in &self.slots {
            if *p != self.me && !peers.contains(p) {
                peers.push(*p);
            }
        }
        let names: Vec<&str> = peers.iter().map(|p| self.name_of(*p)).collect();
        names.join(", ")
    }

    /// Name of the player on controller `slot`, short enough for a toast; an idle seat names
    /// the peer it carries by that peer's first played seat.
    fn slot_name(&self, slot: usize) -> String {
        let name = match self.names.get(slot) {
            Some(name) if !self.idle.contains(&slot) => name.as_str(),
            _ => self.slots.get(slot).map_or("?", |p| self.name_of(*p)),
        };
        name.chars().take(12).collect()
    }

    /// F5 / F7 in a netplay game (menu, hotkey or `--script`): every peer saves / loads at the
    /// frame this player's input reaches. Refused with what to tell the player when the room
    /// does not let this player save, or for a load without a shared save.
    fn press(&mut self, key: ScriptKey) -> Result<(), Text> {
        if !self.saves.allows(self.me) {
            return Err(if self.saves == SavePolicy::Off { Text::SavesOff } else { Text::SavesHostOnly });
        }
        match key {
            ScriptKey::Save => self.save_pending = true,
            ScriptKey::Load if self.session.has_save() => self.load_pending = true,
            ScriptKey::Load => return Err(Text::NoQuickSave),
        }
        Ok(())
    }

    /// Leaving the game: this very frame becomes the last moment, so the next session
    /// continues here and not at a checkpoint up to 5 s old (a boss beaten in those seconds
    /// would be alive again). The newest frames ran on predicted inputs of the others, so
    /// unlike the checkpoints this file is this player's own view.
    fn keep_last_moment(&self) {
        if self.saves == SavePolicy::Off || !netsave::last_due(self.advanced) {
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
    away: bool,
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
            slot_thumb: s.slot_thumbs[s.slot as usize].as_deref(),
            can_load: false,
            can_save: true,
            host: false,
            away,
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
            slot_thumb: None,
            can_load: n.session.has_save(),
            can_save: n.saves.allows(n.me),
            host: n.me == PeerId(0),
            away: false,
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
