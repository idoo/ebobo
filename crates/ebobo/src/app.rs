//! Command line, SDL setup, main loop and state machine (menu, lobby, solo game, netplay
//! game). Owner: app slice.

mod cli;
mod logic;
mod png;
mod rewind;
mod slots;
mod update;

/// Save-state slots per game.
pub const SAVE_SLOTS: u8 = slots::SLOTS;

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ebobo_core::{Nes, Region, Snapshot};
use ebobo_net::{
    GameStart, Lobby, LobbyConfig, LobbyMode, LobbyStatus, NetDiag, NetError, NetEvent, NetSession, NetStats, PeerId,
    Stage,
    INPUT_RESET,
};

use crate::audio::Audio;
use crate::canvas::{self, Canvas, Font};
use crate::config::{self, Config};
use crate::i18n::Lang;
use crate::input::{Input, InputFrame, PadInfo};
use crate::library::{self, Library, RomEntry};
use crate::ui::{GameView, Text, Ui, UiAction, UiCtx};
use crate::video::Video;

use cli::{Parsed, Start};

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

struct Solo {
    nes: Nes,
    name: String,
    /// Battery file and the bytes last written (or loaded).
    save_path: Option<PathBuf>,
    saved_ram: Vec<u8>,
    battery_at: Instant,
    /// Current save-state slot (0-based) and when each slot file was written (`None` = empty).
    slot: u8,
    slot_times: [Option<std::time::SystemTime>; slots::SLOTS as usize],
    /// Rewind history: a snapshot and the pads of the frame run right after it.
    rewind: rewind::Ring<(Snapshot, [u8; 4])>,
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
        slots::path(&config::data_dir().join("states"), &self.name, self.nes.info().crc32, slot)
    }
}

struct Net {
    nes: Nes,
    session: NetSession,
    me: PeerId,
    slots: Vec<PeerId>,
    names: Vec<String>,
    rom_name: String,
    /// Interrupted peers and when they will be dropped.
    interrupted: Vec<(PeerId, Instant)>,
    gone: Vec<PeerId>,
    desync: bool,
    reset_pending: bool,
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
    samples: Vec<f32>,
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
    app.begin(cli.start.clone());

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
        wait_until(deadline, app.in_game());
    }
    app.end_game();
    // Quitting with a downloaded update: install it (no relaunch) so the next start is current.
    app.updater.install_on_quit();
    if let Some(path) = &cli.screenshot {
        let png = png::encode_rgba(canvas::W as u32, canvas::H as u32, &app.canvas.px);
        if let Err(err) = std::fs::write(path, png) {
            eprintln!("ebobo: cannot write {}: {err}", path.display());
            return 1;
        }
    }
    0
}

/// Waits until `deadline`. In game: coarse sleep until ~1.5 ms before, then yield-spin for
/// accurate pacing. In menus/lobby: one plain sleep (no busy CPU).
fn wait_until(deadline: Instant, precise: bool) {
    const SPIN: Duration = Duration::from_micros(1500);
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
            thread::sleep(left - SPIN);
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
        let video = Video::new(&sdl, config.fullscreen, config.scale, config.screen)?;
        let lang = Lang::resolve(config.lang.as_deref());
        let mut ui = Ui::new(lang);
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
            samples: Vec::with_capacity(2048),
            title_rom: None,
            quit: false,
            autostart: cli.autostart,
            room_printed: false,
            net_check: None,
            updater,
        })
    }

    /// Initial state from the command line.
    fn begin(&mut self, start: Start) {
        self.ui.show_main();
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

    fn period(&self) -> Duration {
        match &self.state {
            State::Solo(s) => logic::frame_period(s.nes.frame_rate(), 0),
            State::Net(n) => logic::frame_period(n.nes.frame_rate(), n.session.frames_ahead()),
            _ => logic::frame_period(MENU_FPS, 0),
        }
    }

    fn tick(&mut self, dt: Duration) {
        let frame = self.input.poll(&mut self.pump, dt);
        self.ui.tick(&frame, dt);
        if dt > SLEEP_GAP && matches!(self.state, State::Net(_)) {
            // Sleep/suspend: GGRS resyncs or times peers out by itself; the frame debt cap in
            // `next_deadline` prevents a catch-up burst.
            self.ui.toast(self.ui.text(Text::SleepGap));
        }
        self.poll_net_check();
        self.poll_update();
        self.input.set_text_input(self.ui.wants_text_input());
        if !frame.pad_events.is_empty() {
            self.pads = self.input.pads();
            if let State::Solo(s) = &mut self.state {
                if frame.gamepads >= 3 {
                    s.nes.set_four_score(true);
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

        // Menu toggle over a running game.
        let mut menu_opened = false;
        if self.in_game() && frame.menu {
            if !self.ui.is_visible() {
                self.ui.show_pause();
                menu_opened = true;
            } else if !frame.nav.back {
                self.ui.hide();
            }
        }
        if let State::Solo(_) = self.state {
            if frame.save_state {
                self.perform(UiAction::SaveState);
            }
            if frame.load_state {
                self.perform(UiAction::LoadState);
            }
            if frame.screenshot {
                self.screenshot();
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
            );
            let action = self.ui.update(&frame, &mut ctx, dt);
            if let Some(action) = action {
                self.perform(action);
            }
        }

        self.advance(&frame);
        self.update_battery(false);
        self.compose(&frame);
        if let Err(err) = self.video.present(&self.canvas.px) {
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
                        s.nes.reset();
                        if let Some(audio) = &mut self.audio {
                            audio.clear();
                        }
                    }
                    State::Net(n) => n.reset_pending = true,
                    _ => {}
                }
                self.ui.hide();
            }
            UiAction::SelectSlot(slot) => {
                if let State::Solo(s) = &mut self.state {
                    s.slot = slot.min(slots::SLOTS - 1);
                }
            }
            UiAction::SaveState => {
                if let State::Solo(s) = &mut self.state {
                    let path = s.slot_path(s.slot);
                    let text = match slots::save(&s.nes, &path) {
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
            }
            UiAction::LoadState => {
                if let State::Solo(s) = &mut self.state {
                    let path = s.slot_path(s.slot);
                    let text = match slots::load(&mut s.nes, &path) {
                        Ok(()) => {
                            s.rewind.clear();
                            s.since_snapshot = 0;
                            s.nes.clear_audio();
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
            }
            UiAction::ToggleFullscreen => self.toggle_fullscreen(),
            UiAction::ConfigChanged => self.apply_config(),
            UiAction::Rescan => self.library.rescan(),
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
        let nes = match &mut self.state {
            State::Solo(s) => &mut s.nes,
            State::Net(n) => &mut n.nes,
            _ => return,
        };
        nes.set_sprite_limit(self.config.sprite_limit.applies(nes.four_score()));
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
        let mut nes = match Nes::new(&rom, logic::region_hint(&entry.name, entry.region)) {
            Ok(nes) => nes,
            Err(err) => return self.fail(&format!("{}: {err}", entry.name)),
        };
        let four_score = self.input.pads().len() >= 3 || nes.info().four_score;
        nes.set_four_score(four_score);
        nes.set_sprite_limit(self.config.sprite_limit.applies(four_score));
        let mut save_path = None;
        let mut saved_ram = Vec::new();
        if nes.battery_ram().is_some() {
            let path = config::saves_dir().join(logic::save_file_name(&entry.name, nes.info().crc32));
            if let Ok(bytes) = std::fs::read(&path) {
                nes.load_battery_ram(&bytes);
            }
            saved_ram = nes.battery_ram().map(<[u8]>::to_vec).unwrap_or_default();
            save_path = Some(path);
        }
        let mut solo = Solo {
            nes,
            name: entry.name,
            save_path,
            saved_ram,
            battery_at: Instant::now(),
            slot: 0,
            slot_times: [None; slots::SLOTS as usize],
            rewind: rewind::Ring::new(rewind::CAPACITY),
            since_snapshot: 0,
            rewinding: false,
            ff_on: false,
            ff_held: false,
        };
        for slot in 0..slots::SLOTS {
            solo.slot_times[slot as usize] = slots::saved_at(&solo.slot_path(slot));
        }
        self.state = State::Solo(Box::new(solo));
        self.ui.hide();
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
        self.start_lobby(if direct {
            LobbyMode::HostDirect { port: self.config.direct_port, rom, rom_name }
        } else {
            LobbyMode::HostRoom { rom, rom_name }
        });
    }

    fn start_lobby(&mut self, mode: LobbyMode) {
        self.end_game();
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

    fn start_net(&mut self, start: GameStart) {
        self.end_game();
        let mut nes = match Nes::new(&start.rom, Some(start.region)) {
            Ok(nes) => nes,
            Err(err) => {
                self.ui.show_main();
                return self.net_error(&NetError::BadRom(err.to_string()));
            }
        };
        // PeerId(0) is the host, who already has the file.
        if start.me != PeerId(0) {
            self.keep_received_rom(&start.rom_name, nes.info().crc32, &start.rom);
        }
        nes.set_four_score(start.four_score);
        nes.set_sprite_limit(self.config.sprite_limit.applies(start.four_score));
        let fps = if start.region == Region::Pal { 50 } else { 60 };
        let session = match NetSession::new(start.transport, &start.slots, start.input_delay, fps) {
            Ok(session) => session,
            Err(err) => {
                self.ui.show_main();
                return self.net_error(&err);
            }
        };
        self.state = State::Net(Box::new(Net {
            nes,
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
            frame: 0,
            advanced: 0,
            last_advance: Instant::now(),
        }));
        self.ui.hide();
    }

    /// Keeps a ROM the host sent in this player's own library, so the game is there next time
    /// and for solo play.
    fn keep_received_rom(&self, name: &str, crc32: u32, rom: &[u8]) {
        if self.library.find_crc(crc32).is_some() {
            return;
        }
        let dir = config::data_dir().join("roms");
        let mut path = dir.join(logic::received_rom_file_name(name, None));
        if path.exists() {
            path = dir.join(logic::received_rom_file_name(name, Some(crc32)));
        }
        let tmp = path.with_extension("tmp");
        let saved = std::fs::create_dir_all(&dir)
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

    /// Leaves whatever runs (battery written, BYE sent) and goes to the no-game state.
    fn end_game(&mut self) {
        self.update_battery(true);
        match std::mem::replace(&mut self.state, State::Menu) {
            State::Lobby(lobby) => lobby.leave(),
            State::Net(net) => net.session.leave(),
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
        let Some(ram) = s.nes.battery_ram() else { return };
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
                        s.nes.restore(&snap);
                        s.nes.set_fast_forward(false);
                        s.nes.run_frame(pads);
                    }
                    s.nes.clear_audio();
                    s.since_snapshot = 0;
                } else if !ui_visible {
                    s.ff_on = logic::fast_forward(self.config.ff_toggle, frame.fast_forward, s.ff_held, s.ff_on);
                    s.ff_held = frame.fast_forward;
                    let n = if s.ff_on { 3 } else { 1 };
                    for i in 0..n {
                        if s.since_snapshot == 0 {
                            s.rewind.push((s.nes.snapshot(), frame.pads));
                        }
                        s.since_snapshot = (s.since_snapshot + 1) % rewind::EVERY;
                        // Only the last frame of a fast-forward tick draws pixels and sound.
                        s.nes.set_fast_forward(i + 1 < n);
                        s.nes.run_frame(frame.pads);
                    }
                }
            }
            State::Net(n) => {
                n.session.poll();
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
                let mut local: u16 = 0;
                if !ui_visible {
                    local = frame.pads.iter().fold(0u16, |acc, p| acc | *p as u16);
                }
                if n.reset_pending {
                    local |= INPUT_RESET;
                }
                match n.session.advance(&mut n.nes, local) {
                    Ok(outcome) => {
                        if outcome.advanced {
                            n.reset_pending = false;
                            n.advanced += 1;
                            n.last_advance = Instant::now();
                            if n.advanced % 300 == 0 {
                                println!("ebobo: frame {} hash {:016x}", n.advanced, n.nes.state_hash());
                                let _ = std::io::Write::flush(&mut std::io::stdout());
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
                    n.stats = n.session.stats();
                }
            }
            State::Menu | State::Lobby(_) => {}
        }
        self.pump_audio(ui_visible);
    }

    fn pump_audio(&mut self, ui_visible: bool) {
        let Some(audio) = &mut self.audio else { return };
        let rewinding = matches!(&self.state, State::Solo(s) if s.rewinding);
        let nes = match &mut self.state {
            State::Solo(s) => Some(&mut s.nes),
            State::Net(n) => Some(&mut n.nes),
            _ => None,
        };
        // Solo pauses audio with the game and mutes it while rewinding; netplay keeps playing
        // under the menu.
        let paused = nes.is_none() || (ui_visible && matches!(self.state, State::Solo(_))) || rewinding;
        if paused != self.audio_paused {
            audio.set_paused(paused);
            if rewinding {
                audio.clear();
            }
            self.audio_paused = paused;
        }
        let (nes, stretch) = match &mut self.state {
            State::Solo(s) if !paused => (&mut s.nes, 0.0),
            State::Net(n) => {
                let stretch = logic::frame_stretch(n.session.frames_ahead());
                (&mut n.nes, stretch)
            }
            _ => return,
        };
        nes.set_sample_rate(audio.sample_rate() * audio.rate_factor() * (1.0 + stretch));
        self.samples.clear();
        nes.drain_audio(&mut self.samples);
        audio.push(&self.samples);
    }

    fn compose(&mut self, frame: &InputFrame) {
        match &self.state {
            State::Solo(s) => s.nes.frame_rgba(&mut self.canvas.px),
            State::Net(n) => n.nes.frame_rgba(&mut self.canvas.px),
            State::Menu | State::Lobby(_) => self.canvas.clear(BACKDROP),
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
        let (w, h, k) = (ebobo_core::WIDTH, ebobo_core::HEIGHT, 3);
        let mut rgba = vec![0u8; w * h * 4];
        s.nes.frame_rgba(&mut rgba);
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
            local_slot: None,
            stats: None,
            notice: None,
            desync: false,
            stalled: false,
            save_slot: Some((s.slot, s.slot_times[s.slot as usize])),
        }),
        State::Net(n) => Some(GameView {
            rom_name: &n.rom_name,
            netplay: true,
            names: &n.names,
            local_slot: Some(n.session.local_slot()),
            stats: Some(&n.stats),
            notice,
            desync: n.desync,
            stalled: n.last_advance.elapsed() > STALL_DIM_AFTER,
            save_slot: None,
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
