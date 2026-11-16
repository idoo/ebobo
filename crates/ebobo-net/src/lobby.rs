//! Lobby on top of the mesh: game info, ROM check and transfer, ready, the saved game the host
//! starts from (announce and transfer), start (`docs/PROTOCOL.md` section 4). Runs on a
//! background thread so the UI never blocks. Owner: lobby slice.

use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ebobo_console::Region;
use serde_json::{json, Value};

use crate::mesh::{Link, Mesh, MeshConfig, MeshEvent, MeshRole};
use crate::packet::{Packet, ROM_CHUNK_DATA, STATE_CHUNK_DATA};
use crate::transport::{PeerId, RouteKind};
use crate::{NetError, SavePolicy, MAX_PLAYERS, PROTOCOL_VERSION};

const TICK: Duration = Duration::from_millis(4);
const CTRL_RETRY: Duration = Duration::from_millis(200);
/// Lobby keepalive and RTT probe: 4 pings a second (~25 B each) give the auto input delay
/// steady RTT samples.
const PING_EVERY: Duration = Duration::from_millis(250);
/// RTT samples a peer needs before the auto input delay trusts `srtt + 2 * rttvar`.
const WARM_SAMPLES: u32 = 8;
/// ROM chunks in flight per guest: 64 KB a round trip. The host sends and the guest reads once a
/// `TICK`, so a round is the round trip plus up to two ticks: a 4 MB ROM (4096 chunks, 64 rounds)
/// takes 2.6-3.1 s at 40 ms and 9.6-10.1 s at 150 ms, the largest one (`MAX_ROM`) twice that. 64
/// datagrams of 1 KB also fit a default Linux UDP receive buffer (208 KB) between two guest ticks.
const ROM_WINDOW: u32 = 64;
/// No ack progress for this long: everything past the last acked byte is sent again. A transfer
/// has no deadline of its own; it runs for as long as the route lives.
const ROM_RETRY: Duration = Duration::from_millis(300);
/// The host hands the game over when every guest acked `start` or this passed. It starts counting
/// at the start, when every guest already holds the ROM, so the ROM's size does not matter to it.
const START_ACK_WAIT: Duration = Duration::from_secs(3);
/// Largest ROM a guest accepts over the wire.
const MAX_ROM: usize = 8 * 1024 * 1024;
/// Largest saved game (`Resume::state`) a host sends and a guest accepts. NES states are
/// 20-100 KB, Mega Drive states about 300 KB (5 round trips at `STATE_WINDOW`, well inside
/// `STATE_WAIT`); the cap keeps a broken peer from making a guest allocate without bound and fits
/// the chunk index (`u16`) and the ack bitmap (128 bytes) in one packet.
const MAX_STATE: usize = 1024 * 1024;
/// A start from a save is called off (`NetError::SaveTransfer` on the host, the lobby stays
/// usable) when some guest does not hold the whole state this long after the host pressed start.
const STATE_WAIT: Duration = Duration::from_secs(10);
/// Saved-game chunks in flight per guest (sent, not acked, not yet timed out): 64 KB, so a
/// typical state goes out in one or two round trips without flooding a slow uplink. A chunk
/// unacked for the peer's CTRL retransmit timeout is sent again.
const STATE_WINDOW: usize = 64;

/// A seat of `LobbyStatus::order` that nobody holds: a player left a game that goes on, and
/// the seats after it keep their controllers. Only a room that came back from a game has them.
pub const EMPTY_SEAT: PeerId = PeerId(0xFE);

pub struct LobbyConfig {
    /// Signaling server base URL.
    pub server: String,
    pub nickname: String,
    pub mode: LobbyMode,
    /// Relay every pair (skip hole punching).
    pub force_relay: bool,
    /// Host's GGRS input delay; `None` = auto from the worst RTT.
    pub input_delay: Option<u8>,
    /// Guest: returns the ROM file bytes whose PRG+CHR CRC-32 matches, from the local library.
    pub rom_lookup: Box<dyn Fn(u32) -> Option<Vec<u8>> + Send>,
}

pub enum LobbyMode {
    HostRoom { rom: Vec<u8>, rom_name: String },
    JoinRoom { code: String },
    HostDirect { port: u16, rom: Vec<u8>, rom_name: String },
    JoinDirect { addr: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Binding, STUN, contacting the server / the direct host.
    Connecting,
    /// In the room, waiting for players / ROM transfer / the host's start.
    Lobby,
    /// Start received or sent; `take_game` will return the game.
    Starting,
    /// See `LobbyStatus::error`.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RomState {
    Unknown,
    Have,
    Receiving { percent: u8 },
    Missing,
}

/// Which save the host starts the game from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeKind {
    /// The automatic save of the last online session of this game.
    Last,
    /// The save a player made with "Save for everyone".
    Save,
}

/// A saved game the host starts from instead of power-on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resume {
    pub kind: ResumeKind,
    /// When it was saved, Unix seconds; shown to every player in the lobby.
    pub saved_at: u64,
    /// `ebobo_console::Console::save_state` bytes for the host's ROM.
    pub state: Vec<u8>,
}

/// `Resume` without the bytes, for the lobby screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumeInfo {
    pub kind: ResumeKind,
    pub saved_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerInfo {
    pub id: PeerId,
    pub name: String,
    pub is_self: bool,
    /// `None` until the route to this player is decided (always `None` for self).
    pub route: Option<RouteKind>,
    pub rtt_ms: Option<u32>,
    pub rom: RomState,
    pub ready: bool,
    /// The server lost this player's connection; they may come back within 60 s (shown as
    /// "reconnecting" instead of disappearing). Always false for self.
    pub away: bool,
    /// Local players this device brings (seats the host gave it), at least 1.
    pub seats: u8,
}

/// Snapshot of the lobby for the UI; cheap to clone, refreshed by the lobby thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LobbyStatus {
    pub stage: Stage,
    /// Room code (room modes, once known).
    pub code: Option<String>,
    pub me: Option<PeerId>,
    pub is_host: bool,
    /// Every present player including self, sorted by id.
    pub players: Vec<PlayerInfo>,
    pub rom_name: Option<String>,
    /// Host: `start_game` would start now.
    pub can_start: bool,
    pub error: Option<NetError>,
    /// The save the host will start from; `None` = a new game. Guests learn it from the host.
    pub resume: Option<ResumeInfo>,
    /// Seat i (controller i+1) → peer; a peer appears once per local player. The host owns it,
    /// guests learn it from the host.
    pub order: Vec<PeerId>,
    /// Who may save in the game: the host's choice; guests learn it from the host.
    pub saves: SavePolicy,
    /// This room came back from a running game: Start continues it.
    pub regrouped: bool,
}

/// Everything the game loop needs once the host pressed start.
pub struct GameStart {
    /// The live mesh: routes, relay, in-game route health and roaming.
    pub link: Link,
    pub me: PeerId,
    /// `slots[i]` = peer id on controller i+1 of the console. A seat nobody plays (`idle`)
    /// carries the host's id: the host's device feeds it no presses.
    pub slots: Vec<PeerId>,
    /// Indexes into `slots` of the seats nobody plays (a player left a game that goes on).
    pub idle: Vec<usize>,
    /// Player names by slot.
    pub names: Vec<String>,
    /// The ROM file both sides run (host's own, or found/received by the guest).
    pub rom: Vec<u8>,
    pub rom_name: String,
    /// The delay the game starts with.
    pub input_delay: usize,
    /// The host left "Input delay" on Auto, so every peer adapts its own delay in game.
    pub delay_auto: bool,
    pub four_score: bool,
    /// Region the host announced (header region of its ROM); every peer must emulate it.
    pub region: Region,
    /// `ebobo_console::Console::save_state` bytes every peer loads before the first frame (the host's
    /// `Resume::state`); `None` = power-on.
    pub state: Option<Vec<u8>>,
    /// What `state` is (kind and age), as the host announced it with the start; `None` with it.
    pub resume: Option<ResumeInfo>,
    /// Who may save in this game, as the host set it in the room.
    pub saves: SavePolicy,
}

/// Handle to the lobby thread.
pub struct Lobby {
    status: Arc<Mutex<LobbyStatus>>,
    cmd: Sender<Cmd>,
    game: Receiver<GameStart>,
    thread: Option<JoinHandle<()>>,
}

impl Lobby {
    /// Spawns the lobby thread and returns immediately.
    pub fn start(cfg: LobbyConfig) -> Lobby {
        let (is_host, rom_name) = match &cfg.mode {
            LobbyMode::HostRoom { rom_name, .. } | LobbyMode::HostDirect { rom_name, .. } => {
                (true, Some(rom_name.clone()))
            }
            _ => (false, None),
        };
        let status = Arc::new(Mutex::new(LobbyStatus {
            stage: Stage::Connecting,
            code: None,
            me: None,
            is_host,
            players: Vec::new(),
            rom_name,
            can_start: false,
            error: None,
            resume: None,
            order: Vec::new(),
            saves: SavePolicy::Everyone,
            regrouped: false,
        }));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (game_tx, game_rx) = mpsc::channel();
        let shared = status.clone();
        let thread = std::thread::Builder::new()
            .name("ebobo-lobby".into())
            .spawn(move || run(cfg, shared, cmd_rx, game_tx))
            .expect("spawn lobby thread");
        Lobby { status, cmd: cmd_tx, game: game_rx, thread: Some(thread) }
    }

    pub fn status(&self) -> LobbyStatus {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Host only; ignored unless `status().can_start`.
    pub fn start_game(&self) {
        let _ = self.cmd.send(Cmd::StartGame);
    }

    /// Host only: the next `start_game` starts from this save (`None` = a new game). May be
    /// called any time before the start; every player sees the choice in `LobbyStatus::resume`.
    pub fn set_resume(&self, resume: Option<Resume>) {
        let _ = self.cmd.send(Cmd::SetResume(resume));
    }

    /// `Some` exactly once after the game started.
    pub fn take_game(&mut self) -> Option<GameStart> {
        self.game.try_recv().ok()
    }

    /// How many local players this device brings, 1..=4. The host applies it to its own seats;
    /// a guest asks the host, which gives as many as are free.
    pub fn set_seats(&self, n: u8) {
        let _ = self.cmd.send(Cmd::SetSeats(n));
    }

    /// Host only: swaps seat `seat` (index into `LobbyStatus::order`) with the one before it
    /// (`up`) or after it. Out of range does nothing.
    pub fn move_seat(&self, seat: usize, up: bool) {
        let _ = self.cmd.send(Cmd::MoveSeat(seat, up));
    }

    /// Host only: who may save in the game.
    pub fn set_saves(&self, p: SavePolicy) {
        let _ = self.cmd.send(Cmd::SetSaves(p));
    }

    /// Back from a running game to the room, on the same connections: `link` is the game's
    /// link (`NetSession::into_link`), `gone` the players who left during the game. The host
    /// passes the game's state as `resume`; Start then continues from it with whoever is in
    /// the room. A guest passes `None` and waits for the host as in any room.
    pub fn regroup(&self, link: Link, gone: Vec<PeerId>, resume: Option<Resume>) {
        let _ = self.cmd.send(Cmd::Regroup { link, gone, resume });
    }

    /// Leaves the room (BYE/leave to peers) and stops the thread.
    pub fn leave(self) {
        drop(self);
    }
}

impl Drop for Lobby {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Leave);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum Cmd {
    StartGame,
    SetResume(Option<Resume>),
    SetSeats(u8),
    MoveSeat(usize, bool),
    SetSaves(SavePolicy),
    Regroup { link: Link, gone: Vec<PeerId>, resume: Option<Resume> },
    Leave,
}

struct HostRom {
    bytes: Vec<u8>,
    name: String,
    crc: u32,
    region: Region,
    four_score: bool,
}

struct GameInfo {
    crc: u32,
    rom_name: String,
    region: Region,
}

struct RomRx {
    buf: Vec<u8>,
    got: Vec<bool>,
    next: u32,
}

struct ListEntry {
    id: PeerId,
    name: String,
    ready: bool,
    rom: RomState,
    seats: u8,
}

struct Pending {
    packet: Vec<u8>,
    first: Instant,
    last: Instant,
    resent: bool,
}

/// Number of `STATE_CHUNK_DATA` chunks a state of `len` bytes travels in.
fn state_chunks(len: usize) -> usize {
    len.div_ceil(STATE_CHUNK_DATA)
}

/// Chunk `index` of `state`: `STATE_CHUNK_DATA` bytes, the last one shorter.
fn state_chunk(state: &[u8], index: usize) -> &[u8] {
    let start = index * STATE_CHUNK_DATA;
    &state[start..(start + STATE_CHUNK_DATA).min(state.len())]
}

/// Bit `i` of a `StateAck` bitmap.
fn has_bit(bits: &[u8], i: usize) -> bool {
    bits.get(i / 8).is_some_and(|b| b >> (i % 8) & 1 == 1)
}

/// Host: one guest's progress through a saved-game transfer (selective repeat: the guest acks
/// every chunk it holds, the host resends only the missing ones).
struct StateTx {
    acked: Vec<bool>,
    /// When each chunk was last sent.
    sent: Vec<Option<Instant>>,
    left: usize,
}

impl StateTx {
    fn new(chunks: usize) -> StateTx {
        StateTx { acked: vec![false; chunks], sent: vec![None; chunks], left: chunks }
    }

    fn done(&self) -> bool {
        self.left == 0
    }

    /// Chunks to send now, lowest first: never sent, or unacked for `rto`; at most
    /// `STATE_WINDOW` in flight. Marks them sent at `now`.
    fn due(&mut self, now: Instant, rto: Duration) -> Vec<u16> {
        let fresh = |s: Option<Instant>| s.is_some_and(|t| now.duration_since(t) < rto);
        let mut in_flight = (0..self.acked.len()).filter(|&i| !self.acked[i] && fresh(self.sent[i])).count();
        let mut out = Vec::new();
        for i in 0..self.acked.len() {
            if in_flight >= STATE_WINDOW {
                break;
            }
            if self.acked[i] || fresh(self.sent[i]) {
                continue;
            }
            self.sent[i] = Some(now);
            out.push(i as u16);
            in_flight += 1;
        }
        out
    }

    /// Marks the chunks set in the guest's `StateAck` bitmap.
    fn on_ack(&mut self, have: &[u8]) {
        for (i, acked) in self.acked.iter_mut().enumerate() {
            if !*acked && has_bit(have, i) {
                *acked = true;
                self.left -= 1;
            }
        }
    }
}

/// Guest: the saved game arriving from the host, transfer `id`.
struct StateRx {
    id: u32,
    crc: u32,
    buf: Vec<u8>,
    got: Vec<bool>,
    left: usize,
    /// Complete and its CRC-32 matched.
    verified: bool,
}

impl StateRx {
    fn new(id: u32, total: usize, crc: u32) -> StateRx {
        let chunks = state_chunks(total);
        StateRx { id, crc, buf: vec![0; total], got: vec![false; chunks], left: chunks, verified: false }
    }

    /// Stores chunk `index`; false (nothing stored) when its index or size does not fit.
    fn put(&mut self, index: usize, data: &[u8]) -> bool {
        if index >= self.got.len() {
            return false;
        }
        let start = index * STATE_CHUNK_DATA;
        let end = (start + STATE_CHUNK_DATA).min(self.buf.len());
        if data.len() != end - start {
            return false;
        }
        if !self.got[index] {
            self.buf[start..end].copy_from_slice(data);
            self.got[index] = true;
            self.left -= 1;
        }
        true
    }

    /// `StateAck` bitmap of the chunks held.
    fn ack(&self) -> Vec<u8> {
        let mut bits = vec![0u8; self.got.len().div_ceil(8)];
        for (i, _) in self.got.iter().enumerate().filter(|(_, g)| **g) {
            bits[i / 8] |= 1 << (i % 8);
        }
        bits
    }
}

/// Host: a start from a save waiting for every guest to hold the state.
struct Offer {
    id: u32,
    state: Vec<u8>,
    crc: u32,
    /// Pressed start + `STATE_WAIT`.
    deadline: Instant,
    tx: BTreeMap<PeerId, StateTx>,
}

/// RFC 6298 smoothed RTT and RTT variation in ms. The lobby feeds it CTRL acks, the session
/// GGRS pings; both plan the input delay from `delay_rtt`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RttEstimator {
    /// Smoothed RTT (RFC 6298 SRTT); `None` before the first sample.
    pub(crate) srtt: Option<f64>,
    /// RTT variation (RFC 6298 RTTVAR).
    pub(crate) rttvar: f64,
    pub(crate) samples: u32,
}

impl RttEstimator {
    /// RFC 6298 section 2 update (alpha = 1/8, beta = 1/4) with one RTT sample in ms.
    pub(crate) fn add_sample(&mut self, sample: f64) {
        match self.srtt {
            Some(srtt) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (srtt - sample).abs();
                self.srtt = Some(0.875 * srtt + 0.125 * sample);
            }
            None => {
                self.rttvar = sample / 2.0;
                self.srtt = Some(sample);
            }
        }
        self.samples += 1;
    }

    /// True once `WARM_SAMPLES` samples are in and `srtt + 2 * rttvar` is trusted.
    pub(crate) fn is_warm(&self) -> bool {
        self.samples >= WARM_SAMPLES
    }

    /// RTT (ms) the input delay plans for: `srtt + 2 * rttvar` once warm. Before that RFC 6298's
    /// cold-start `rttvar = R / 2` would add a whole RTT, so it uses `srtt * 1.25` instead.
    pub(crate) fn delay_rtt(&self) -> Option<f64> {
        let srtt = self.srtt?;
        Some(if self.is_warm() { srtt + 2.0 * self.rttvar } else { srtt * 1.25 })
    }

    /// Retransmit timeout in ms: RFC 6298 `srtt + 4 * rttvar`, 0 before the first sample.
    fn rto_ms(&self) -> f64 {
        self.srtt.map_or(0.0, |r| r + 4.0 * self.rttvar)
    }
}

struct Ctrl {
    next_seq: u32,
    unacked: BTreeMap<u32, Pending>,
    seen: HashSet<u32>,
    /// Control-message RTT (first transmissions only, Karn's rule); feeds the auto input delay.
    rtt: RttEstimator,
}

impl Ctrl {
    fn add_sample(&mut self, sample: f64) {
        self.rtt.add_sample(sample);
    }

    fn delay_rtt(&self) -> Option<f64> {
        self.rtt.delay_rtt()
    }

    /// Retransmit timeout: RFC 6298 `srtt + 4 * rttvar`, at least `CTRL_RETRY`, at most 1 s, so
    /// slow links are not retransmitted before their ack can arrive (which Karn's rule would
    /// discard as a sample).
    fn rto(&self) -> Duration {
        Duration::from_secs_f64(self.rtt.rto_ms() / 1000.0).clamp(CTRL_RETRY, Duration::from_secs(1))
    }
}

struct Xfer {
    next: u32,
    sent: u32,
    last_progress: Instant,
}

struct Peer {
    name: String,
    route: Option<RouteKind>,
    ctrl: Ctrl,
    rom: RomState,
    ready: bool,
    links: Vec<u8>,
    xfer: Option<Xfer>,
    sent_players: Option<String>,
    /// Host: refused for another ebobo version; never counts as ready.
    bad_version: bool,
    /// The server reported this peer's WebSocket dropped (`away`) and not yet `back`.
    away: bool,
    /// Host: refused because every seat was taken (`room_full`); not a player of this room.
    full: bool,
}

impl Peer {
    fn new() -> Peer {
        Peer {
            name: String::new(),
            route: None,
            ctrl: Ctrl { next_seq: 1, unacked: BTreeMap::new(), seen: HashSet::new(), rtt: RttEstimator::default() },
            rom: RomState::Unknown,
            ready: false,
            links: Vec::new(),
            xfer: None,
            sent_players: None,
            bad_version: false,
            away: false,
            full: false,
        }
    }
}

struct PendingStart {
    acks: Vec<(PeerId, u32)>,
    deadline: Instant,
    slots: Vec<PeerId>,
    idle: Vec<usize>,
    delay: usize,
    delay_auto: bool,
    four_score: bool,
    state: Option<Vec<u8>>,
    resume: Option<ResumeInfo>,
}

struct Worker {
    mesh: Option<Mesh>,
    shared: Arc<Mutex<LobbyStatus>>,
    last_status: Option<LobbyStatus>,
    game_tx: Sender<GameStart>,
    nickname: String,
    /// ebobo version announced (`core`, `ver`) and required of every peer.
    version: String,
    input_delay: Option<u8>,
    rom_lookup: Box<dyn Fn(u32) -> Option<Vec<u8>> + Send>,
    is_host: bool,
    stage: Stage,
    code: Option<String>,
    me: Option<PeerId>,
    error: Option<NetError>,
    peers: BTreeMap<PeerId, Peer>,
    last_ping: Instant,
    // Host.
    host_rom: Option<HostRom>,
    pending_start: Option<PendingStart>,
    resume: Option<Resume>,
    /// The saved-game transfer of a start from `resume` (no start goes out before it ends).
    offer: Option<Offer>,
    /// Id of the next `Offer`; a called-off transfer's late packets never match a new one.
    next_offer: u32,
    // Guest.
    game: Option<GameInfo>,
    rx: Option<RomRx>,
    rom: Option<Vec<u8>>,
    my_rom: RomState,
    sent_links: Option<Vec<u8>>,
    host_list: Option<Vec<ListEntry>>,
    /// The host's resume choice from its `players` message.
    announced: Option<ResumeInfo>,
    state_rx: Option<StateRx>,
    // Both.
    /// Seat i (controller i+1) → peer. The host owns it and starts with itself on seat 1; a
    /// guest copies it from the host's `players` message.
    order: Vec<PeerId>,
    /// Local players this device wants (`Lobby::set_seats`).
    want_seats: u8,
    /// Who may save in the game: the host's choice, or what it announced.
    saves: SavePolicy,
    /// This room came back from a running game (`regroup`): Start continues that game.
    regrouped: bool,
    /// The devices that played the interrupted game and are still here; their seats stay put.
    veterans: Vec<PeerId>,
}

/// One-way latency, in frames, that the input delay hides outright: 3 frames (50 ms at 60 fps)
/// go unnoticed under the fingers, and a link within them never rolls back.
const DELAY_COVERS: usize = 3;
/// One-way latency past `DELAY_COVERS`, in frames, left to prediction and rollback: corrections
/// of up to 6 frames (100 ms) stay small on screen, and more input lag would hurt more.
const ROLLBACK_COVERS: usize = 6;

/// Input delay from the worst peer's `RttEstimator::delay_rtt` (ms), 1..=6: the one-way latency
/// plus jitter in frames, of which the first `DELAY_COVERS` are delay, the next
/// `ROLLBACK_COVERS` are left to rollback, and the rest are delay again (a far link would
/// otherwise run out of the prediction window and stall). The split is the usual one for
/// rollback games (3 frames of delay, 6-8 of rollback). The lobby's start value and the
/// session's in-game target both use it.
pub(crate) fn auto_input_delay(worst_rtt_ms: f64, frame_ms: f64) -> usize {
    let latency = (worst_rtt_ms / 2.0 / frame_ms).ceil() as usize;
    (latency.min(DELAY_COVERS) + latency.saturating_sub(DELAY_COVERS + ROLLBACK_COVERS)).clamp(1, 6)
}

fn run(cfg: LobbyConfig, shared: Arc<Mutex<LobbyStatus>>, cmds: Receiver<Cmd>, game_tx: Sender<GameStart>) {
    let LobbyConfig { server, nickname, mode, force_relay, input_delay, rom_lookup } = cfg;
    let (role, host_rom) = match mode {
        LobbyMode::HostRoom { rom, rom_name } => (MeshRole::HostRoom { code: None }, Some((rom, rom_name))),
        LobbyMode::JoinRoom { code } => (MeshRole::JoinRoom { code }, None),
        LobbyMode::HostDirect { port, rom, rom_name } => (MeshRole::HostDirect { port }, Some((rom, rom_name))),
        LobbyMode::JoinDirect { addr } => (MeshRole::JoinDirect { addr }, None),
    };
    let mut w = Worker::new(shared, game_tx, nickname.clone(), input_delay, rom_lookup, host_rom.is_some());
    if let Some((bytes, name)) = host_rom {
        match ebobo_console::identify(&bytes) {
            Some(id) => w.host_rom = Some(HostRom { bytes, name, crc: id.crc32, region: id.region, four_score: id.multitap }),
            None => {
                w.fail(NetError::BadRom("not a ROM of a console ebobo knows".into()));
                w.publish();
                idle(&cmds);
                return;
            }
        }
    }
    match Mesh::open(MeshConfig { server, nickname, role, force_relay }) {
        Ok(mesh) => w.mesh = Some(mesh),
        Err(e) => {
            w.fail(e);
            w.publish();
            idle(&cmds);
            return;
        }
    }
    loop {
        loop {
            match cmds.try_recv() {
                Ok(Cmd::StartGame) => w.start_game(),
                Ok(Cmd::SetResume(resume)) => w.set_resume(resume),
                Ok(Cmd::SetSeats(n)) => w.set_seats(n),
                Ok(Cmd::MoveSeat(seat, up)) => w.move_seat(seat, up),
                Ok(Cmd::SetSaves(p)) => w.set_saves(p),
                Ok(Cmd::Regroup { link, gone, resume }) => w.regroup(link, gone, resume),
                Ok(Cmd::Leave) | Err(TryRecvError::Disconnected) => {
                    w.leave();
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let Some(mesh) = w.mesh.as_mut() else {
            // The game has the mesh (or the room failed): nothing to drive until the game
            // gives it back or the room is left.
            w.publish();
            match idle(&cmds) {
                Some((link, gone, resume)) => {
                    w.regroup(link, gone, resume);
                    continue;
                }
                None => return,
            }
        };
        for ev in mesh.poll() {
            w.on_event(ev);
        }
        if w.mesh.is_some() {
            w.timers(Instant::now());
        }
        w.publish();
        std::thread::sleep(TICK);
    }
}

/// Waits, with nothing to drive, for the game to give the mesh back (`Some`: what
/// `Lobby::regroup` passed) or for `Leave` / the handle being dropped (`None`).
fn idle(cmds: &Receiver<Cmd>) -> Option<(Link, Vec<PeerId>, Option<Resume>)> {
    while let Ok(cmd) = cmds.recv() {
        match cmd {
            Cmd::Regroup { link, gone, resume } => return Some((link, gone, resume)),
            Cmd::Leave => return None,
            _ => {}
        }
    }
    None
}

fn route_str(r: Option<RouteKind>) -> Value {
    match r {
        Some(RouteKind::Direct) => json!("direct"),
        Some(RouteKind::Relay) => json!("relay"),
        None => Value::Null,
    }
}

fn rom_json(r: RomState) -> (&'static str, u8) {
    match r {
        RomState::Unknown => ("unknown", 0),
        RomState::Have => ("have", 100),
        RomState::Receiving { percent } => ("receiving", percent),
        RomState::Missing => ("missing", 0),
    }
}

/// The ebobo version this build plays with. Tests impersonate an older build with a nickname
/// starting `OldVer`.
fn local_version(nickname: &str) -> String {
    if cfg!(test) && nickname.starts_with("OldVer") {
        return "0.0.0-oldver".into();
    }
    env!("CARGO_PKG_VERSION").to_string()
}

/// A peer may play with us only on the same lobby protocol and the same ebobo version.
fn same_version(proto: Option<u64>, ver: Option<&str>, mine: &str) -> bool {
    proto == Some(u64::from(PROTOCOL_VERSION)) && ver == Some(mine)
}

fn resume_json(r: Option<ResumeInfo>) -> Value {
    match r {
        Some(r) => json!({
            "kind": match r.kind { ResumeKind::Last => "last", ResumeKind::Save => "save" },
            "saved_at": r.saved_at,
        }),
        None => Value::Null,
    }
}

/// `resume` of a `players` message; `None` for null (a new game) or anything unknown.
fn resume_of(v: &Value) -> Option<ResumeInfo> {
    let kind = match v["kind"].as_str()? {
        "last" => ResumeKind::Last,
        "save" => ResumeKind::Save,
        _ => return None,
    };
    Some(ResumeInfo { kind, saved_at: v["saved_at"].as_u64()? })
}

fn error_of(msg: &str) -> NetError {
    match msg {
        "version" => NetError::Version,
        "rom_crc" => NetError::RomCrc,
        "rom_missing" => NetError::RomMissing,
        "host_left" => NetError::HostLeft,
        "save" => NetError::SaveTransfer,
        "room_full" => NetError::RoomFull,
        other => NetError::Io(other.to_string()),
    }
}

/// A seat is free while the room has fewer than `MAX_PLAYERS` seats or one of them is empty.
fn has_free_seat(order: &[PeerId]) -> bool {
    order.len() < MAX_PLAYERS || order.contains(&EMPTY_SEAT)
}

/// `id` takes the first empty seat, else a new one at the end.
fn take_seat(order: &mut Vec<PeerId>, id: PeerId) {
    match order.iter().position(|p| *p == EMPTY_SEAT) {
        Some(i) => order[i] = id,
        None => order.push(id),
    }
}

/// Host, after every seat change: an empty seat with a newcomer's seat (anyone not in
/// `veterans`) after it takes the first such seat, so newcomers close gaps while the players of
/// the interrupted game keep their controllers; empty seats at the end are dropped.
fn settle(order: &mut Vec<PeerId>, veterans: &[PeerId]) {
    for i in 0..order.len() {
        if order[i] != EMPTY_SEAT {
            continue;
        }
        let newcomer = (i + 1..order.len()).find(|&j| order[j] != EMPTY_SEAT && !veterans.contains(&order[j]));
        if let Some(j) = newcomer {
            order[i] = order[j];
            order[j] = EMPTY_SEAT;
        }
    }
    while order.last() == Some(&EMPTY_SEAT) {
        order.pop();
    }
}

impl Worker {
    fn new(
        shared: Arc<Mutex<LobbyStatus>>,
        game_tx: Sender<GameStart>,
        nickname: String,
        input_delay: Option<u8>,
        rom_lookup: Box<dyn Fn(u32) -> Option<Vec<u8>> + Send>,
        is_host: bool,
    ) -> Worker {
        Worker {
            mesh: None,
            shared,
            last_status: None,
            game_tx,
            version: local_version(&nickname),
            nickname,
            input_delay,
            rom_lookup,
            is_host,
            stage: Stage::Connecting,
            code: None,
            me: None,
            error: None,
            peers: BTreeMap::new(),
            last_ping: Instant::now(),
            host_rom: None,
            pending_start: None,
            resume: None,
            offer: None,
            next_offer: 1,
            game: None,
            rx: None,
            rom: None,
            my_rom: RomState::Unknown,
            sent_links: None,
            host_list: None,
            announced: None,
            state_rx: None,
            order: if is_host { vec![PeerId(0)] } else { Vec::new() },
            want_seats: 1,
            saves: SavePolicy::Everyone,
            regrouped: false,
            veterans: Vec::new(),
        }
    }

    fn fail(&mut self, e: NetError) {
        self.stage = Stage::Failed;
        self.error = Some(e);
        self.pending_start = None;
        self.offer = None;
    }

    /// Host: the save the next start uses. Ignored while a start is under way (its transfer or
    /// its start acks), so every guest gets the state the start announces.
    fn set_resume(&mut self, resume: Option<Resume>) {
        if self.is_host && self.offer.is_none() && self.pending_start.is_none() {
            self.resume = resume;
        }
    }

    /// Peers that play in this room: everyone but the guests the host refused for lack of a seat.
    fn seated(&self) -> impl Iterator<Item = (&PeerId, &Peer)> + '_ {
        self.peers.iter().filter(|(_, p)| !p.full)
    }

    /// Seats `id` holds.
    fn seats_of(&self, id: PeerId) -> usize {
        self.order.iter().filter(|p| **p == id).count()
    }

    /// Host: a guest it has not seated yet gets one seat. With no seat free it is told
    /// `room_full` (it fails with `NetError::RoomFull`) and is neither a player nor waited for.
    fn admit(&mut self, id: PeerId) {
        if !self.is_host || self.order.contains(&id) || self.peers.get(&id).is_some_and(|p| p.full) {
            return;
        }
        if has_free_seat(&self.order) {
            take_seat(&mut self.order, id);
            settle(&mut self.order, &self.veterans);
            return;
        }
        self.send_ctrl(id, json!({"t": "error", "msg": "room_full"}));
        let p = self.peers.entry(id).or_insert_with(Peer::new);
        p.full = true;
        p.ready = false;
        p.xfer = None;
    }

    /// Host: `peer` gets up to `n` seats (1..=`MAX_PLAYERS`): new ones (first empty seats, then
    /// at the end) while seats are free, extra ones are given up from its last. A regrouped
    /// room keeps a given-up seat as `EMPTY_SEAT`, so nobody else's controller changes. Ignored
    /// once `start` went out.
    fn resize_seats(&mut self, peer: PeerId, n: u8) {
        if self.pending_start.is_some() {
            return;
        }
        let n = usize::from(n).clamp(1, MAX_PLAYERS);
        let mut have = self.seats_of(peer);
        while have < n && has_free_seat(&self.order) {
            take_seat(&mut self.order, peer);
            have += 1;
        }
        while have > n {
            if let Some(last) = self.order.iter().rposition(|p| *p == peer) {
                if self.regrouped {
                    self.order[last] = EMPTY_SEAT;
                } else {
                    self.order.remove(last);
                }
            }
            have -= 1;
        }
        settle(&mut self.order, &self.veterans);
    }

    /// This device's local players: the host changes its own seats, a guest asks the host
    /// (once it heard the host's `game`; before that `on_game` asks).
    fn set_seats(&mut self, n: u8) {
        self.want_seats = n.clamp(1, MAX_PLAYERS as u8);
        if self.is_host {
            self.resize_seats(self.me.unwrap_or(PeerId(0)), self.want_seats);
        } else if self.game.is_some() && self.stage == Stage::Lobby {
            self.send_ctrl(PeerId(0), json!({"t": "seats", "n": self.want_seats}));
        }
    }

    /// Host: swaps seat `seat` with the one before it (`up`) or after it, empty seats included.
    fn move_seat(&mut self, seat: usize, up: bool) {
        if !self.is_host || self.pending_start.is_some() {
            return;
        }
        let other = if up { seat.checked_sub(1) } else { seat.checked_add(1) };
        if let Some(other) = other.filter(|o| seat.max(*o) < self.order.len()) {
            self.order.swap(seat, other);
            settle(&mut self.order, &self.veterans);
        }
    }

    /// Host: who may save in the game. Ignored once `start` went out.
    fn set_saves(&mut self, p: SavePolicy) {
        if self.is_host && self.pending_start.is_none() {
            self.saves = p;
        }
    }

    fn send_ctrl(&mut self, to: PeerId, msg: Value) -> u32 {
        let Some(mesh) = self.mesh.as_mut() else { return 0 };
        let peer = self.peers.entry(to).or_insert_with(Peer::new);
        let seq = peer.ctrl.next_seq;
        peer.ctrl.next_seq += 1;
        let text = msg.to_string();
        let packet = Packet::Ctrl { seq, json: text.as_bytes() }.to_vec();
        mesh.send(to, &packet);
        let now = Instant::now();
        peer.ctrl.unacked.insert(seq, Pending { packet, first: now, last: now, resent: false });
        seq
    }

    fn routed(&self) -> Vec<PeerId> {
        self.peers.iter().filter(|(_, p)| p.route.is_some()).map(|(id, _)| *id).collect()
    }

    fn on_event(&mut self, ev: MeshEvent) {
        match ev {
            MeshEvent::Ready { me, code } => {
                self.me = Some(me);
                if code.is_some() {
                    self.code = code;
                }
                if self.stage == Stage::Connecting {
                    self.stage = Stage::Lobby;
                }
            }
            MeshEvent::PeerJoined { id, name } => {
                let p = self.peers.entry(id).or_insert_with(Peer::new);
                if !name.is_empty() {
                    p.name = name;
                }
                self.admit(id);
            }
            MeshEvent::RouteUp { id, kind } => {
                self.peers.entry(id).or_insert_with(Peer::new).route = Some(kind);
                self.welcome(id);
            }
            MeshEvent::RouteChanged { id, kind } => {
                if let Some(p) = self.peers.get_mut(&id) {
                    p.route = Some(kind);
                }
            }
            MeshEvent::PeerLeft { id } => self.peer_left(id),
            MeshEvent::PeerAway { id } | MeshEvent::PeerBack { id } => {
                if let Some(p) = self.peers.get_mut(&id) {
                    p.away = matches!(ev, MeshEvent::PeerAway { .. });
                }
            }
            MeshEvent::Packet { from, data } => self.on_packet(from, &data),
            MeshEvent::Failed(e) => self.fail(e),
        }
    }

    /// Host: a guest whose route is up gets a seat and, unless the room is full, the game.
    fn welcome(&mut self, id: PeerId) {
        if !self.is_host {
            return;
        }
        self.admit(id);
        if !self.peers.get(&id).is_some_and(|p| p.full) {
            self.send_game(id);
        }
    }

    fn send_game(&mut self, to: PeerId) {
        let Some(hr) = &self.host_rom else { return };
        let msg = json!({
            "t": "game",
            "proto": PROTOCOL_VERSION,
            "core": self.version,
            "name": self.nickname,
            "rom": {"crc32": format!("{:08x}", hr.crc), "size": hr.bytes.len(), "name": hr.name},
            "region": hr.region.code(),
            "four_score": hr.four_score,
        });
        self.send_ctrl(to, msg);
    }

    fn peer_left(&mut self, id: PeerId) {
        self.peers.remove(&id);
        if self.is_host {
            // A regrouped room keeps the seats of a player who left, so the others keep theirs.
            if self.regrouped {
                for seat in self.order.iter_mut().filter(|p| **p == id) {
                    *seat = EMPTY_SEAT;
                }
            } else {
                self.order.retain(|p| *p != id);
            }
            settle(&mut self.order, &self.veterans);
        }
        if let Some(list) = &mut self.host_list {
            list.retain(|e| e.id != id);
        }
        if let Some(ps) = &mut self.pending_start {
            ps.acks.retain(|(p, _)| *p != id);
        }
        if let Some(o) = &mut self.offer {
            o.tx.remove(&id);
        }
        if !self.is_host && id == PeerId(0) && self.stage != Stage::Starting {
            self.fail(NetError::HostLeft);
        }
    }

    fn on_packet(&mut self, from: PeerId, data: &[u8]) {
        match Packet::parse(data) {
            Some(Packet::Ctrl { seq, json }) => {
                if let Some(mesh) = self.mesh.as_mut() {
                    mesh.send(from, &Packet::CtrlAck { seq }.to_vec());
                }
                let peer = self.peers.entry(from).or_insert_with(Peer::new);
                if !peer.ctrl.seen.insert(seq) {
                    return;
                }
                if let Ok(v) = serde_json::from_slice::<Value>(json) {
                    self.on_ctrl(from, &v);
                }
            }
            Some(Packet::CtrlAck { seq }) => {
                let Some(p) = self.peers.get_mut(&from) else { return };
                if let Some(pend) = p.ctrl.unacked.remove(&seq) {
                    // Karn's rule: an ack of a retransmitted message is ambiguous.
                    if !pend.resent {
                        p.ctrl.add_sample(pend.first.elapsed().as_secs_f64() * 1000.0);
                    }
                }
            }
            Some(Packet::RomChunk { offset, total, data }) => self.on_chunk(from, offset, total, data),
            Some(Packet::RomAck { next }) => self.on_rom_ack(from, next),
            Some(Packet::StateChunk { id, total, crc, index, data }) => {
                self.on_state_chunk(from, id, total, crc, index, data)
            }
            Some(Packet::StateAck { id, have }) => {
                if let Some(tx) = self.offer.as_mut().filter(|o| o.id == id).and_then(|o| o.tx.get_mut(&from)) {
                    tx.on_ack(have);
                }
            }
            _ => {}
        }
    }

    fn on_ctrl(&mut self, from: PeerId, v: &Value) {
        let t = v["t"].as_str().unwrap_or("");
        match t {
            "ping" => {}
            "leave" => self.peer_left(from),
            "error" => {
                let e = error_of(v["msg"].as_str().unwrap_or(""));
                if self.is_host {
                    if let Some(p) = self.peers.get_mut(&from) {
                        p.rom = RomState::Missing;
                        p.ready = false;
                        p.xfer = None;
                    }
                    // A guest that cannot take the saved game calls the start off at once.
                    if e == NetError::SaveTransfer {
                        self.offer = None;
                    }
                    self.error = Some(e);
                } else if from == PeerId(0) {
                    self.fail(e);
                }
            }
            _ if self.is_host => self.host_ctrl(from, t, v),
            _ if from == PeerId(0) => self.guest_ctrl(t, v),
            _ => {}
        }
    }

    fn host_ctrl(&mut self, from: PeerId, t: &str, v: &Value) {
        if t == "rom" && !same_version(v["proto"].as_u64(), v["ver"].as_str(), &self.version) {
            self.refuse_version(from);
            return;
        }
        if t == "seats" {
            if !self.peers.get(&from).is_none_or(|p| p.bad_version || p.full) {
                self.resize_seats(from, v["n"].as_u64().unwrap_or(1).clamp(1, MAX_PLAYERS as u64) as u8);
            }
            return;
        }
        let p = self.peers.entry(from).or_insert_with(Peer::new);
        if p.bad_version || p.full {
            return;
        }
        match t {
            "rom" => {
                if let Some(name) = v["name"].as_str().filter(|n| !n.is_empty()) {
                    p.name = name.to_string();
                }
                if v["have"].as_bool().unwrap_or(false) {
                    p.rom = RomState::Have;
                    p.xfer = None;
                } else {
                    p.rom = RomState::Receiving { percent: 0 };
                    p.xfer.get_or_insert(Xfer { next: 0, sent: 0, last_progress: Instant::now() });
                }
            }
            "ready" => {
                p.ready = true;
                p.links = v["links"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as u8).collect())
                    .unwrap_or_default();
            }
            _ => {}
        }
    }

    /// Host: a guest runs another version. It is told (`error version`, it fails with
    /// `NetError::Version`) and can never be ready, so the game cannot start with it.
    fn refuse_version(&mut self, from: PeerId) {
        self.send_ctrl(from, json!({"t": "error", "msg": "version"}));
        let p = self.peers.entry(from).or_insert_with(Peer::new);
        p.bad_version = true;
        p.ready = false;
        p.rom = RomState::Missing;
        p.xfer = None;
        self.error = Some(NetError::Version);
    }

    fn guest_ctrl(&mut self, t: &str, v: &Value) {
        match t {
            "game" => self.on_game(v),
            "players" => {
                let Some(arr) = v["list"].as_array() else { return };
                let list = arr
                    .iter()
                    .filter_map(|e| {
                        let id = PeerId(e["id"].as_u64()? as u8);
                        let rom = match e["rom"].as_str() {
                            Some("have") => RomState::Have,
                            Some("missing") => RomState::Missing,
                            Some("receiving") => {
                                RomState::Receiving { percent: e["pct"].as_u64().unwrap_or(0).min(100) as u8 }
                            }
                            _ => RomState::Unknown,
                        };
                        Some(ListEntry {
                            id,
                            name: e["name"].as_str().unwrap_or("").to_string(),
                            ready: e["ready"].as_bool().unwrap_or(false),
                            rom,
                            seats: e["seats"].as_u64().unwrap_or(1).clamp(1, MAX_PLAYERS as u64) as u8,
                        })
                    })
                    .collect();
                self.host_list = Some(list);
                self.announced = resume_of(&v["resume"]);
                if let Some(order) = v["order"].as_array() {
                    self.order = order.iter().filter_map(Value::as_u64).map(|x| PeerId(x as u8)).collect();
                }
                self.saves = SavePolicy::parse(v["saves"].as_str());
            }
            // The host calls the players of a running game back to the room. The game's link
            // acts on it; a copy that reaches this lobby means the lobby is already back.
            "regroup" => {}
            "start" => {
                if self.stage != Stage::Lobby {
                    return;
                }
                let (Some(rom), Some(game)) = (self.rom.clone(), self.game.as_ref()) else {
                    self.fail(NetError::RomMissing);
                    return;
                };
                let slots: Vec<PeerId> = v["slots"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_u64).map(|x| PeerId(x as u8)).collect())
                    .unwrap_or_default();
                let idle: Vec<usize> = v["idle"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as usize).collect())
                    .unwrap_or_default();
                let delay = v["delay"].as_u64().unwrap_or(2) as usize;
                let delay_auto = v["auto"].as_bool().unwrap_or(false);
                let four = v["four_score"].as_bool().unwrap_or(slots.len() > 2);
                let (rom_name, region) = (game.rom_name.clone(), game.region);
                // The policy travels with the start: the `players` message that changed it may
                // still be on its way.
                self.saves = SavePolicy::parse(v["saves"].as_str());
                let state = match v.get("state").filter(|s| !s.is_null()) {
                    None => {
                        self.state_rx = None;
                        None
                    }
                    Some(s) => match self.take_state(s) {
                        Some(bytes) => Some(bytes),
                        None => {
                            self.refuse_state();
                            return;
                        }
                    },
                };
                // The kind travels with the start: the `players` message that announced it may
                // still be on its way.
                let resume = state.as_ref().and_then(|_| resume_of(&v["state"]));
                self.hand_over(slots, idle, delay, delay_auto, four, rom, rom_name, region, state, resume);
            }
            _ => {}
        }
    }

    fn on_game(&mut self, v: &Value) {
        let host = PeerId(0);
        if !same_version(v["proto"].as_u64(), v["core"].as_str(), &self.version) {
            self.send_ctrl(host, json!({"t": "error", "msg": "version"}));
            self.fail(NetError::Version);
            return;
        }
        let Some(crc) = v["rom"]["crc32"].as_str().and_then(|s| u32::from_str_radix(s, 16).ok()) else {
            self.fail(NetError::BadRom("game message without crc32".into()));
            return;
        };
        if let Some(name) = v["name"].as_str().filter(|n| !n.is_empty()) {
            self.peers.entry(host).or_insert_with(Peer::new).name = name.to_string();
        }
        let Some(region) = v["region"].as_u64().and_then(|c| u8::try_from(c).ok()).and_then(Region::from_code) else {
            self.fail(NetError::BadRom("game message without a known region".into()));
            return;
        };
        let first = self.game.is_none();
        self.game = Some(GameInfo { crc, rom_name: v["rom"]["name"].as_str().unwrap_or("").to_string(), region });
        // `set_seats` before the host was heard could not ask; ask now.
        if first && self.want_seats != 1 {
            self.send_ctrl(host, json!({"t": "seats", "n": self.want_seats}));
        }
        if self.rom.is_some() {
            return;
        }
        let found = (self.rom_lookup)(crc).filter(|b| ebobo_console::identify(b).is_some_and(|i| i.crc32 == crc));
        let have = found.is_some();
        if let Some(bytes) = found {
            self.rom = Some(bytes);
            self.my_rom = RomState::Have;
        } else {
            self.rx = None;
            self.my_rom = RomState::Receiving { percent: 0 };
        }
        let name = self.nickname.clone();
        let ver = self.version.clone();
        self.send_ctrl(host, json!({"t": "rom", "have": have, "name": name, "proto": PROTOCOL_VERSION, "ver": ver}));
    }

    /// Guest: the verified state the start's `state` announcement (`id`, `size`, `crc32`) names,
    /// or `None` when it is not fully here.
    fn take_state(&mut self, s: &Value) -> Option<Vec<u8>> {
        let id = s["id"].as_u64()?;
        let size = s["size"].as_u64()?;
        let crc = s["crc32"].as_str().and_then(|c| u32::from_str_radix(c, 16).ok())?;
        let rx = self.state_rx.take()?;
        (rx.verified && u64::from(rx.id) == id && rx.buf.len() as u64 == size && rx.crc == crc).then_some(rx.buf)
    }

    /// Guest: the host's saved game cannot be used; the host is told (it calls the start off).
    fn refuse_state(&mut self) {
        self.send_ctrl(PeerId(0), json!({"t": "error", "msg": "save"}));
        self.state_rx = None;
        self.fail(NetError::SaveTransfer);
    }

    /// Guest: one chunk of the saved game. Every chunk is answered with the full bitmap of held
    /// chunks (duplicates too), so a lost ack is repaired by the next one. A state over
    /// `MAX_STATE`, or one whose CRC-32 does not match once complete, is refused.
    fn on_state_chunk(&mut self, from: PeerId, id: u32, total: u32, crc: u32, index: u16, data: &[u8]) {
        if self.is_host || from != PeerId(0) || self.stage != Stage::Lobby {
            return;
        }
        let total = total as usize;
        if total == 0 || total > MAX_STATE {
            self.refuse_state();
            return;
        }
        if self.state_rx.as_ref().is_none_or(|rx| rx.id != id) {
            self.state_rx = Some(StateRx::new(id, total, crc));
        }
        let Some(rx) = self.state_rx.as_mut() else { return };
        if rx.buf.len() != total || rx.crc != crc || !rx.put(index as usize, data) {
            return;
        }
        if rx.left == 0 && !rx.verified {
            if ebobo_console::crc32(&[&rx.buf]) != rx.crc {
                self.refuse_state();
                return;
            }
            rx.verified = true;
        }
        let ack = Packet::StateAck { id, have: &rx.ack() }.to_vec();
        if let Some(mesh) = self.mesh.as_mut() {
            mesh.send(from, &ack);
        }
    }

    fn on_chunk(&mut self, from: PeerId, offset: u32, total: u32, data: &[u8]) {
        if self.is_host || from != PeerId(0) || self.game.is_none() {
            return;
        }
        let Some(mesh) = self.mesh.as_mut() else { return };
        if let Some(rom) = &self.rom {
            // Late duplicate after completion: confirm everything.
            mesh.send(from, &Packet::RomAck { next: rom.len() as u32 }.to_vec());
            return;
        }
        let total = total as usize;
        if total == 0 || total > MAX_ROM {
            return;
        }
        let rx = self.rx.get_or_insert_with(|| RomRx {
            buf: vec![0; total],
            got: vec![false; total.div_ceil(ROM_CHUNK_DATA)],
            next: 0,
        });
        let off = offset as usize;
        if rx.buf.len() != total || off % ROM_CHUNK_DATA != 0 || off + data.len() > total {
            return;
        }
        let idx = off / ROM_CHUNK_DATA;
        if !rx.got[idx] {
            rx.buf[off..off + data.len()].copy_from_slice(data);
            rx.got[idx] = true;
        }
        while (rx.next as usize) < total && rx.got[rx.next as usize / ROM_CHUNK_DATA] {
            rx.next = (rx.next as usize + ROM_CHUNK_DATA).min(total) as u32;
        }
        let next = rx.next;
        mesh.send(from, &Packet::RomAck { next }.to_vec());
        self.my_rom = RomState::Receiving { percent: (next as usize * 100 / total) as u8 };
        if next as usize != total {
            return;
        }
        let bytes = self.rx.take().map(|r| r.buf).unwrap_or_default();
        let crc = self.game.as_ref().map(|g| g.crc);
        if ebobo_console::identify(&bytes).is_some_and(|i| Some(i.crc32) == crc) {
            self.rom = Some(bytes);
            self.my_rom = RomState::Have;
            let name = self.nickname.clone();
            let ver = self.version.clone();
            self.send_ctrl(from, json!({"t": "rom", "have": true, "name": name, "proto": PROTOCOL_VERSION, "ver": ver}));
        } else {
            self.my_rom = RomState::Missing;
            self.send_ctrl(from, json!({"t": "error", "msg": "rom_crc"}));
            self.fail(NetError::RomCrc);
        }
    }

    fn on_rom_ack(&mut self, from: PeerId, next: u32) {
        let Some(total) = self.host_rom.as_ref().map(|h| h.bytes.len() as u32) else { return };
        let Some(p) = self.peers.get_mut(&from) else { return };
        let Some(x) = p.xfer.as_mut() else { return };
        let next = next.min(total);
        if next > x.next {
            x.next = next;
            x.sent = x.sent.max(next);
            x.last_progress = Instant::now();
            p.rom = RomState::Receiving { percent: (u64::from(next) * 100 / u64::from(total.max(1))) as u8 };
        }
    }

    fn timers(&mut self, now: Instant) {
        // Reliable CTRL retransmits.
        if let Some(mesh) = self.mesh.as_mut() {
            for (id, p) in &mut self.peers {
                let rto = p.ctrl.rto();
                for pend in p.ctrl.unacked.values_mut() {
                    if now.duration_since(pend.last) >= rto {
                        mesh.send(*id, &pend.packet);
                        pend.last = now;
                        pend.resent = true;
                    }
                }
            }
        }
        if self.stage == Stage::Failed {
            return;
        }
        if now.duration_since(self.last_ping) >= PING_EVERY {
            self.last_ping = now;
            for id in self.routed() {
                self.send_ctrl(id, json!({"t": "ping"}));
            }
        }
        if self.is_host {
            self.drive_rom_transfers(now);
            self.broadcast_players();
            self.drive_offer(now);
            self.check_start(now);
        } else if self.rom.is_some() && self.stage == Stage::Lobby {
            let links: Vec<u8> = self.routed().iter().map(|p| p.0).collect();
            let host_routed = self.peers.get(&PeerId(0)).is_some_and(|p| p.route.is_some());
            if host_routed && self.sent_links.as_ref() != Some(&links) {
                self.send_ctrl(PeerId(0), json!({"t": "ready", "links": links}));
                self.sent_links = Some(links);
            }
        }
    }

    fn drive_rom_transfers(&mut self, now: Instant) {
        let (Some(mesh), Some(hr)) = (self.mesh.as_mut(), self.host_rom.as_ref()) else { return };
        let total = hr.bytes.len() as u32;
        let window = ROM_WINDOW * ROM_CHUNK_DATA as u32;
        for (id, p) in &mut self.peers {
            let Some(x) = p.xfer.as_mut() else { continue };
            if x.next >= total {
                continue;
            }
            if x.sent > x.next && now.duration_since(x.last_progress) >= ROM_RETRY {
                x.sent = x.next;
                x.last_progress = now;
            }
            while x.sent < total && x.sent < x.next.saturating_add(window) {
                let end = (x.sent as usize + ROM_CHUNK_DATA).min(total as usize);
                let data = &hr.bytes[x.sent as usize..end];
                mesh.send(*id, &Packet::RomChunk { offset: x.sent, total, data }.to_vec());
                x.sent = end as u32;
            }
        }
    }

    /// Host: sends the saved game to every guest (late joiners included) and starts once all
    /// hold it and the room is still startable; calls the start off at `Offer::deadline`.
    fn drive_offer(&mut self, now: Instant) {
        let Some(o) = self.offer.as_mut() else { return };
        let chunks = state_chunks(o.state.len());
        for (id, p) in &self.peers {
            if p.route.is_some() && !p.full {
                o.tx.entry(*id).or_insert_with(|| StateTx::new(chunks));
            }
        }
        if let Some(mesh) = self.mesh.as_mut() {
            for (id, tx) in &mut o.tx {
                let rto = self.peers.get(id).map_or(CTRL_RETRY, |p| p.ctrl.rto());
                for index in tx.due(now, rto) {
                    let data = state_chunk(&o.state, index as usize);
                    let total = o.state.len() as u32;
                    mesh.send(*id, &Packet::StateChunk { id: o.id, total, crc: o.crc, index, data }.to_vec());
                }
            }
        }
        let (done, expired) = (o.tx.values().all(StateTx::done), now >= o.deadline);
        if done && self.startable() {
            if let Some(o) = self.offer.take() {
                self.launch(Some((o.id, o.crc, o.state)));
            }
        } else if expired {
            self.offer = None;
            self.error = Some(NetError::SaveTransfer);
        }
    }

    fn rtt_ms(p: &Peer) -> Option<u32> {
        p.ctrl.rtt.srtt.map(|r| r.round() as u32)
    }

    fn broadcast_players(&mut self) {
        let Some(me) = self.me else { return };
        let mut list = vec![json!({
            "id": me.0, "name": self.nickname, "route": "self", "rtt": 0, "ready": true,
            "rom": "have", "pct": 100, "seats": self.seats_of(me),
        })];
        for (id, p) in self.seated() {
            let (rom, pct) = rom_json(p.rom);
            list.push(json!({
                "id": id.0, "name": p.name, "route": route_str(p.route), "rtt": Self::rtt_ms(p),
                "ready": p.ready, "rom": rom, "pct": pct, "seats": self.seats_of(*id),
            }));
        }
        let order: Vec<u8> = self.order.iter().map(|p| p.0).collect();
        let text = json!({
            "t": "players", "list": list, "resume": resume_json(self.resume_info()),
            "order": order, "saves": self.saves.as_str(),
        });
        let key = text.to_string();
        let targets: Vec<PeerId> = self
            .peers
            .iter()
            .filter(|(_, p)| !p.full && p.route.is_some() && p.sent_players.as_ref() != Some(&key))
            .map(|(id, _)| *id)
            .collect();
        for id in targets {
            self.send_ctrl(id, text.clone());
            if let Some(p) = self.peers.get_mut(&id) {
                p.sent_players = Some(key.clone());
            }
        }
    }

    fn can_start(&self) -> bool {
        self.pending_start.is_none() && self.offer.is_none() && self.startable()
    }

    /// Host: every guest is routed, ready and linked to everyone, regardless of a start under way.
    fn startable(&self) -> bool {
        if !self.is_host || self.stage != Stage::Lobby || self.me.is_none() {
            return false;
        }
        let ids: Vec<u8> = self.seated().map(|(p, _)| p.0).collect();
        let me = self.me.map_or(0, |m| m.0);
        !ids.is_empty()
            && ids.iter().all(|id| self.order.contains(&PeerId(*id)))
            && self.seated().all(|(id, p)| {
                p.route.is_some()
                    && p.ready
                    && ids.iter().chain([&me]).filter(|&&o| o != id.0).all(|o| p.links.contains(o))
            })
    }

    /// Host: starts now, or from `resume` once every guest holds the state (`drive_offer`). A
    /// state over `MAX_STATE` is refused with `NetError::SaveTransfer` and nothing is sent.
    fn start_game(&mut self) {
        if !self.can_start() {
            return;
        }
        if self.error == Some(NetError::SaveTransfer) {
            self.error = None;
        }
        let Some(resume) = &self.resume else {
            self.launch(None);
            return;
        };
        if resume.state.is_empty() || resume.state.len() > MAX_STATE {
            self.error = Some(NetError::SaveTransfer);
            return;
        }
        let (id, chunks) = (self.next_offer, state_chunks(resume.state.len()));
        self.next_offer = self.next_offer.wrapping_add(1);
        self.offer = Some(Offer {
            id,
            crc: ebobo_console::crc32(&[&resume.state]),
            state: resume.state.clone(),
            deadline: Instant::now() + STATE_WAIT,
            tx: self.seated().map(|(p, _)| (*p, StateTx::new(chunks))).collect(),
        });
    }

    /// Host: sends `start` (with the `state` announcement `(id, crc, bytes)` of a finished
    /// transfer); hands over once every guest acked it or `START_ACK_WAIT` passed. The room
    /// stays open: a device that joins during the game knocks (`Link::take_knocks`).
    fn launch(&mut self, state: Option<(u32, u32, Vec<u8>)>) {
        let (Some(me), Some(hr)) = (self.me, self.host_rom.as_ref()) else { return };
        let frame_ms = if hr.region.is_pal() { 20.0 } else { 16.64 };
        let header_four = hr.four_score;
        let worst = self.seated().filter_map(|(_, p)| p.ctrl.delay_rtt()).fold(0.0, f64::max);
        let delay = match self.input_delay {
            Some(d) => d as usize,
            None => auto_input_delay(worst, frame_ms),
        };
        // An empty seat is played by nobody: it goes out as the host's id, listed in `idle`.
        let idle: Vec<usize> = (0..self.order.len()).filter(|&i| self.order[i] == EMPTY_SEAT).collect();
        let slots: Vec<PeerId> = self.order.iter().map(|&p| if p == EMPTY_SEAT { me } else { p }).collect();
        let four_score = slots.len() > 2 || header_four;
        let slot_ids: Vec<u8> = slots.iter().map(|p| p.0).collect();
        let delay_auto = self.input_delay.is_none();
        let mut msg = json!({
            "t": "start", "slots": slot_ids, "delay": delay, "auto": delay_auto, "four_score": four_score,
            "saves": self.saves.as_str(),
        });
        if !idle.is_empty() {
            msg["idle"] = json!(idle);
        }
        let resume = state.as_ref().and_then(|_| self.resume_info());
        if let Some((id, crc, bytes)) = &state {
            let mut announce = json!({"id": id, "size": bytes.len(), "crc32": format!("{crc:08x}")});
            if let Some(r) = resume {
                announce["kind"] = resume_json(Some(r))["kind"].clone();
                announce["saved_at"] = json!(r.saved_at);
            }
            msg["state"] = announce;
        }
        let guests: Vec<PeerId> = self.seated().map(|(p, _)| *p).collect();
        let acks = guests.into_iter().map(|g| (g, self.send_ctrl(g, msg.clone()))).collect();
        let state = state.map(|(_, _, bytes)| bytes);
        self.pending_start = Some(PendingStart {
            acks,
            deadline: Instant::now() + START_ACK_WAIT,
            slots,
            idle,
            delay,
            delay_auto,
            four_score,
            state,
            resume,
        });
    }

    fn check_start(&mut self, now: Instant) {
        let Some(ps) = &self.pending_start else { return };
        let all_acked = ps
            .acks
            .iter()
            .all(|(id, seq)| self.peers.get(id).is_none_or(|p| !p.ctrl.unacked.contains_key(seq)));
        if !all_acked && now < ps.deadline {
            return;
        }
        let Some(ps) = self.pending_start.take() else { return };
        let Some(hr) = self.host_rom.as_ref() else { return };
        let (rom, name, region) = (hr.bytes.clone(), hr.name.clone(), hr.region);
        self.hand_over(ps.slots, ps.idle, ps.delay, ps.delay_auto, ps.four_score, rom, name, region, ps.state, ps.resume);
    }

    fn name_of(&self, id: PeerId) -> String {
        if Some(id) == self.me {
            return self.nickname.clone();
        }
        if let Some(e) = self.host_list.iter().flatten().find(|e| e.id == id && !e.name.is_empty()) {
            return e.name.clone();
        }
        self.peers.get(&id).map(|p| p.name.clone()).unwrap_or_default()
    }

    /// Player names by slot: the second and later seats of one device are "<name> 2", "<name> 3",
    /// "<name> 4". An `idle` slot (nobody's) has no name and does not count for the numbering.
    fn slot_names(&self, slots: &[PeerId], idle: &[usize]) -> Vec<String> {
        let real = |i: usize| !idle.contains(&i);
        slots
            .iter()
            .enumerate()
            .map(|(i, id)| {
                if !real(i) {
                    return String::new();
                }
                match (0..i).filter(|&j| real(j) && slots[j] == *id).count() {
                    0 => self.name_of(*id),
                    before => format!("{} {}", self.name_of(*id), before + 1),
                }
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn hand_over(
        &mut self,
        slots: Vec<PeerId>,
        idle: Vec<usize>,
        delay: usize,
        delay_auto: bool,
        four_score: bool,
        rom: Vec<u8>,
        rom_name: String,
        region: Region,
        state: Option<Vec<u8>>,
        resume: Option<ResumeInfo>,
    ) {
        let (Some(me), Some(mesh)) = (self.me, self.mesh.take()) else { return };
        let names = self.slot_names(&slots, &idle);
        let game = GameStart {
            link: mesh.into_link(),
            me,
            slots,
            idle,
            names,
            rom,
            rom_name,
            input_delay: delay,
            delay_auto,
            four_score,
            region,
            state,
            resume,
            saves: self.saves,
        };
        let _ = self.game_tx.send(game);
        self.stage = Stage::Starting;
    }

    /// Back from the game: the mesh is this room's again and everyone still here keeps the
    /// seat it played on. `gone` are the devices that left during the game. The host continues
    /// from `resume` (the game's state) and calls the other players back with `regroup`.
    fn regroup(&mut self, link: Link, gone: Vec<PeerId>, resume: Option<Resume>) {
        if self.mesh.is_some() {
            return;
        }
        let mut veterans = vec![PeerId(0)];
        for id in &self.order {
            if *id != EMPTY_SEAT && !veterans.contains(id) {
                veterans.push(*id);
            }
        }
        veterans.retain(|id| !gone.contains(id));
        self.veterans = veterans;
        let mesh = link.into_mesh();
        let routes = mesh.routed();
        self.mesh = Some(mesh);
        self.regrouped = true;
        self.stage = Stage::Lobby;
        self.error = None;
        self.pending_start = None;
        self.offer = None;
        self.state_rx = None;
        for id in &gone {
            // Forgotten completely: a new process may come back under the same id, and its
            // CTRL sequence numbers start again from zero.
            self.peers.remove(id);
            if let Some(list) = &mut self.host_list {
                list.retain(|e| e.id != *id);
            }
            if self.is_host {
                for seat in self.order.iter_mut().filter(|s| *s == id) {
                    *seat = EMPTY_SEAT;
                }
            }
        }
        for (id, kind) in routes {
            self.peers.entry(id).or_insert_with(Peer::new).route = Some(kind);
            // A device that is not a veteran knocked during the game: it joins like any guest.
            if self.is_host && !self.veterans.contains(&id) {
                self.welcome(id);
            }
        }
        if self.is_host {
            settle(&mut self.order, &self.veterans);
            self.resume = resume;
            let guests: Vec<PeerId> =
                self.veterans.iter().copied().filter(|id| self.peers.contains_key(id)).collect();
            for id in guests {
                if let Some(p) = self.peers.get_mut(&id) {
                    // Ready again only after its fresh `ready`: its lobby is back then. The
                    // room's list goes out again too, it may have changed during the game.
                    p.ready = false;
                    p.sent_players = None;
                }
                // Not acked while that guest is in the game, so the CTRL retransmit repeats
                // it until its lobby is back.
                self.send_ctrl(id, json!({"t": "regroup"}));
            }
        } else {
            // The links are announced again (they now include any newcomer), and what the
            // host said about the last start no longer holds.
            self.sent_links = None;
            self.announced = None;
            if gone.contains(&PeerId(0)) {
                self.fail(NetError::HostLeft);
            }
        }
    }

    fn leave(&mut self) {
        for id in self.routed() {
            self.send_ctrl(id, json!({"t": "leave"}));
        }
        if let Some(mesh) = self.mesh.take() {
            mesh.close();
        }
    }

    fn snapshot(&self) -> LobbyStatus {
        let mut players = Vec::new();
        if let Some(me) = self.me {
            let self_rom = if self.is_host { RomState::Have } else { self.my_rom };
            let self_ready = self.is_host || (self.rom.is_some() && self.sent_links.is_some());
            players.push(PlayerInfo {
                id: me,
                name: self.nickname.clone(),
                is_self: true,
                route: None,
                rtt_ms: None,
                rom: self_rom,
                ready: self_ready,
                away: false,
                seats: self.seats_of(me).max(1) as u8,
            });
            match (&self.host_list, self.is_host) {
                (Some(list), false) => {
                    for e in list.iter().filter(|e| e.id != me) {
                        let p = self.peers.get(&e.id);
                        players.push(PlayerInfo {
                            id: e.id,
                            name: if e.name.is_empty() { self.name_of(e.id) } else { e.name.clone() },
                            is_self: false,
                            route: p.and_then(|p| p.route),
                            rtt_ms: p.and_then(Self::rtt_ms),
                            rom: e.rom,
                            ready: e.ready,
                            away: p.is_some_and(|p| p.away),
                            seats: e.seats,
                        });
                    }
                }
                _ => {
                    for (id, p) in self.seated() {
                        let host_seen_by_guest = !self.is_host && *id == PeerId(0);
                        players.push(PlayerInfo {
                            id: *id,
                            name: p.name.clone(),
                            is_self: false,
                            route: p.route,
                            rtt_ms: Self::rtt_ms(p),
                            rom: if host_seen_by_guest { RomState::Have } else { p.rom },
                            ready: if host_seen_by_guest { true } else { p.ready },
                            away: p.away,
                            seats: self.seats_of(*id).max(1) as u8,
                        });
                    }
                }
            }
            players.sort_by_key(|p| p.id);
        }
        LobbyStatus {
            stage: self.stage,
            code: self.code.clone(),
            me: self.me,
            is_host: self.is_host,
            players,
            rom_name: match (&self.host_rom, &self.game) {
                (Some(h), _) => Some(h.name.clone()),
                (None, Some(g)) => Some(g.rom_name.clone()),
                _ => None,
            },
            can_start: self.can_start(),
            error: self.error.clone(),
            resume: self.resume_info(),
            order: self.order.clone(),
            saves: self.saves,
            regrouped: self.regrouped,
        }
    }

    /// The save the game will start from: the host's own choice, or the one it announced.
    fn resume_info(&self) -> Option<ResumeInfo> {
        if self.is_host {
            self.resume.as_ref().map(|r| ResumeInfo { kind: r.kind, saved_at: r.saved_at })
        } else {
            self.announced
        }
    }

    fn publish(&mut self) {
        let snap = self.snapshot();
        if self.last_status.as_ref() == Some(&snap) {
            return;
        }
        *self.shared.lock().unwrap_or_else(|e| e.into_inner()) = snap.clone();
        self.last_status = Some(snap);
    }
}

#[cfg(test)]
#[path = "lobby_tests.rs"]
mod tests;
