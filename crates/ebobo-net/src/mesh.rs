//! Rendezvous and routes: signaling WebSocket, candidate exchange, hole punching, route
//! decisions, relay fallback, direct-IP mode (`docs/PROTOCOL.md` sections 1-2). Owner: mesh
//! slice.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::packet::Packet;
use crate::stun;
use crate::transport::{self, PeerId, RelayHandle, RelayOut, Route, RouteKind, Source, Transport};
use crate::{normalize_code, random_code, random_token, NetError, DEFAULT_DIRECT_PORT};

#[derive(Clone, Debug)]
pub(crate) struct MeshConfig {
    /// Signaling server base URL, e.g. `https://ebobo-signal.example.workers.dev`.
    pub(crate) server: String,
    pub(crate) nickname: String,
    pub(crate) role: MeshRole,
    /// Skip punching, relay every pair.
    pub(crate) force_relay: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum MeshRole {
    /// Create a room; `None` picks random codes and retries on `room_taken`.
    HostRoom { code: Option<String> },
    JoinRoom { code: String },
    /// Listen on UDP `port` for one guest (no server).
    HostDirect { port: u16 },
    /// `addr` = `host:port` or `ip:port` of a direct-mode host.
    JoinDirect { addr: String },
}

#[derive(Debug)]
pub(crate) enum MeshEvent {
    /// Connected to the server (room modes) or socket bound (direct modes). `code` is the room
    /// code in room modes.
    Ready { me: PeerId, code: Option<String> },
    PeerJoined { id: PeerId, name: String },
    /// The pair's route is decided.
    RouteUp { id: PeerId, kind: RouteKind },
    /// A decided pair moved to another route: a dead direct route fell back to the relay, a
    /// fallen-back pair returned to direct, or a direct peer roamed to a new address.
    RouteChanged { id: PeerId, kind: RouteKind },
    PeerLeft { id: PeerId },
    /// The server lost this peer's WebSocket; it may rejoin within the grace period.
    PeerAway { id: PeerId },
    /// An away peer rejoined (also sent for every non-away peer listed in a `welcome`).
    PeerBack { id: PeerId },
    /// Any non-punch packet from a routed peer (CTRL, ROM, GGRS, ...).
    Packet { from: PeerId, data: Vec<u8> },
    /// Fatal for the whole mesh (server refused, host left, direct timeout).
    Failed(NetError),
}

const PUNCH_EVERY: Duration = Duration::from_millis(50);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(1);
/// Lobby (1 Hz keepalives): a direct route silent this long gets punched again...
const ROUTE_REPUNCH_AFTER: Duration = Duration::from_secs(3);
/// ...and silent this long moves to the relay (decided by the lower id, sent as `route`).
const ROUTE_DEAD_AFTER: Duration = Duration::from_secs(5);
/// In game (GGRS ~60 packets/s): a direct route silent this long is re-punched and every
/// packet to that peer is mirrored over the relay until the direct address is heard again.
const GAME_MIRROR_AFTER: Duration = Duration::from_secs(1);
/// In game: a direct route silent this long moves to the relay (lower id decides, `route`).
const GAME_RELAY_AFTER: Duration = Duration::from_secs(3);
/// A decided direct peer that punches from a new address with its token roams there once
/// its current address has been silent this long (late pairing punches never flap a route).
const ROAM_AFTER: Duration = Duration::from_secs(1);
/// In game: a pair that fell back from a direct route to the relay probes the direct path
/// this often (PUNCH to the old address, the candidates and where the peer's punches come from).
const RETURN_PROBE_EVERY: Duration = Duration::from_secs(1);
/// In game: no probing for this long after a pair's first fallback; every further fallback
/// of the pair doubles it (`return_wait`)...
const RETURN_FIRST_WAIT: Duration = Duration::from_secs(5);
/// ...up to this cap.
const RETURN_MAX_WAIT: Duration = Duration::from_secs(60);
/// The lower id returns to direct once it has this many PUNCH_ACKs from one address...
const RETURN_ACKS: usize = 3;
/// ...counted at most one per this interval (one per probe round, not one per probed address)...
const RETURN_ACK_SPACING: Duration = Duration::from_millis(500);
/// ...all inside this window (three rounds at `RETURN_PROBE_EVERY` plus slack).
const RETURN_WINDOW: Duration = Duration::from_secs(4);
/// A direct route in use this long before it fell back resets the pair's backoff.
const RETURN_STABLE_AFTER: Duration = Duration::from_secs(30);
/// WebSocket reconnect: backoff steps (the last repeats) and the give-up time after a drop.
const WS_BACKOFF_MS: [u64; 4] = [500, 1000, 2000, 4000];
const WS_REJOIN_WINDOW: Duration = Duration::from_secs(60);
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Signaling text queued while reconnecting; relay binary frames are dropped instead.
const WS_QUEUE_MAX: usize = 256;
const RELAY_AFTER: Duration = Duration::from_millis(3000);
const RELAY_AFTER_SYMMETRIC: Duration = Duration::from_millis(1500);
const DIRECT_TIMEOUT: Duration = Duration::from_secs(10);
const STUN_TIMEOUT: Duration = Duration::from_millis(1500);
const WS_READ_TIMEOUT: Duration = Duration::from_millis(5);
const WS_PING_EVERY: Duration = Duration::from_secs(20);
const HOST_CODE_ATTEMPTS: usize = 5;
const CLOSE_HOST_LEFT: u16 = 4000;

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

/// Pairing state for one room peer.
struct PeerState {
    /// Their announced candidates and session token (`None` until `cands` arrives).
    cands: Vec<SocketAddr>,
    token: Option<u64>,
    relay_only: bool,
    sent_cands: bool,
    /// Source of their PUNCH (their address as seen by us).
    seen_from: Option<SocketAddr>,
    /// Source of their PUNCH_ACK (a path that works both ways).
    confirmed: Option<SocketAddr>,
    punch_started: Option<Instant>,
    last_punch: Option<Instant>,
    decided: bool,
    /// Last packet seen on the decided direct route (route health).
    last_rx: Option<Instant>,
    /// When the current direct route was adopted (backoff reset after `RETURN_STABLE_AFTER`).
    direct_since: Option<Instant>,
    /// In game: the pair fell back from a direct route to the relay at this time, so it probes
    /// the direct path and may return. `None` for pairs relayed by decision.
    fell_back_at: Option<Instant>,
    /// In-game fallbacks since the direct route last held `RETURN_STABLE_AFTER`; sets the wait.
    fallbacks: u32,
    /// The direct address the route had before the fallback.
    direct_addr: Option<SocketAddr>,
    /// Return probing: source of the peer's latest token-matched PUNCH (it may have moved)...
    probe_seen: Option<SocketAddr>,
    /// ...and of its PUNCH_ACKs, with their arrival times (one per `RETURN_ACK_SPACING`).
    probe_ack_from: Option<SocketAddr>,
    probe_acks: Vec<Instant>,
    /// Just returned to direct: every packet is also mirrored over the relay until the peer is
    /// heard on the direct route (it switches when our `route` arrives, after the mirrored packets).
    returning: bool,
}

impl PeerState {
    fn new() -> PeerState {
        PeerState {
            cands: Vec::new(),
            token: None,
            relay_only: false,
            sent_cands: false,
            seen_from: None,
            confirmed: None,
            punch_started: None,
            last_punch: None,
            decided: false,
            last_rx: None,
            direct_since: None,
            fell_back_at: None,
            fallbacks: 0,
            direct_addr: None,
            probe_seen: None,
            probe_ack_from: None,
            probe_acks: Vec::new(),
            returning: false,
        }
    }
}

/// Probing pause after a pair's `fallbacks`-th in-game fallback: 5, 10, 20, 40, then 60 s.
fn return_wait(fallbacks: u32) -> Duration {
    RETURN_FIRST_WAIT.saturating_mul(1 << fallbacks.saturating_sub(1).min(4)).min(RETURN_MAX_WAIT)
}

enum Mode {
    Room {
        text_rx: Receiver<String>,
        close_rx: Receiver<(u16, String)>,
        ws_out: Sender<RelayOut>,
    },
    HostDirect,
    /// Fixed UDP routes, no server, no punching (`Link::udp`).
    Fixed,
    JoinDirect {
        target: SocketAddr,
        deadline: Instant,
        last_punch: Option<Instant>,
    },
}

pub(crate) struct Mesh {
    transport: Transport,
    mode: Mode,
    me: PeerId,
    code: Option<String>,
    token: u64,
    my_cands: Vec<SocketAddr>,
    symmetric: bool,
    force_relay: bool,
    peers: HashMap<PeerId, PeerState>,
    routes: HashMap<PeerId, Route>,
    events: Vec<MeshEvent>,
    /// Direct modes emit `Ready` from the first poll.
    ready_pending: bool,
    failed: bool,
    last_keepalive: Instant,
    /// A `welcome` arrived (later ones are rejoins after a WebSocket drop).
    welcomed: bool,
    /// Handed to the game (`Link`): game health thresholds and relay mirroring.
    in_game: bool,
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Link>();
};

impl Mesh {
    /// Binds the UDP socket, runs STUN, connects the WebSocket (room modes). Blocking, at
    /// most ~6 s. Server refusals map to `NetError::{RoomTaken, NoRoom, RoomFull, RoomLocked}`.
    pub(crate) fn open(cfg: MeshConfig) -> Result<Mesh, NetError> {
        let bind_port = match cfg.role {
            MeshRole::HostDirect { port } => port,
            _ => 0,
        };
        let udp = transport::bind_udp(bind_port).map_err(|e| NetError::Io(e.to_string()))?;
        let now = Instant::now();
        let base = |transport: Transport, mode: Mode, me: PeerId| Mesh::base(transport, mode, me, cfg.force_relay, now);
        match &cfg.role {
            MeshRole::HostDirect { .. } => {
                let transport = Transport::new(udp, None);
                Ok(base(transport, Mode::HostDirect, PeerId(0)))
            }
            MeshRole::JoinDirect { addr } => {
                let target = resolve_direct(addr)?;
                let transport = Transport::new(udp, None);
                let mode = Mode::JoinDirect { target, deadline: now + DIRECT_TIMEOUT, last_punch: None };
                Ok(base(transport, mode, PeerId(1)))
            }
            MeshRole::HostRoom { .. } | MeshRole::JoinRoom { .. } => {
                let port = udp.local_addr().map_err(|e| NetError::Io(e.to_string()))?.port();
                let stun = stun::query(&udp, stun::STUN_SERVERS, STUN_TIMEOUT);
                let mut my_cands: Vec<SocketAddr> = Vec::new();
                let v6_ips: Vec<IpAddr> =
                    if transport::is_v6(&udp) { stun::local_ipv6().into_iter().map(IpAddr::from).collect() } else { Vec::new() };
                for ip in stun::local_ipv4().into_iter().map(IpAddr::from).chain(v6_ips) {
                    let a = SocketAddr::new(ip, port);
                    if !my_cands.contains(&a) {
                        my_cands.push(a);
                    }
                }
                for a in stun.public {
                    if !my_cands.contains(&a) {
                        my_cands.push(a);
                    }
                }
                let (ws, code, url) = connect_room(&cfg)?;
                let (out_tx, out_rx) = mpsc::channel();
                let (in_tx, in_rx) = mpsc::channel();
                let (text_tx, text_rx) = mpsc::channel();
                let (close_tx, close_rx) = mpsc::channel();
                thread::Builder::new()
                    .name("ebobo-ws".into())
                    .spawn(move || ws_thread(ws, url, out_rx, text_tx, in_tx, close_tx))
                    .map_err(|e| NetError::Io(e.to_string()))?;
                let relay = RelayHandle { out: out_tx.clone(), inbound: in_rx };
                let transport = Transport::new(udp, Some(relay));
                let mode = Mode::Room { text_rx, close_rx, ws_out: out_tx };
                let mut mesh = base(transport, mode, PeerId(0));
                mesh.code = Some(code);
                mesh.token = random_token();
                mesh.my_cands = my_cands;
                mesh.symmetric = stun.symmetric;
                mesh.ready_pending = false;
                Ok(mesh)
            }
        }
    }

    fn base(transport: Transport, mode: Mode, me: PeerId, force_relay: bool, now: Instant) -> Mesh {
        Mesh {
            transport,
            mode,
            me,
            code: None,
            token: 0,
            my_cands: Vec::new(),
            symmetric: false,
            force_relay,
            peers: HashMap::new(),
            routes: HashMap::new(),
            events: Vec::new(),
            ready_pending: true,
            failed: false,
            last_keepalive: now,
            welcomed: false,
            in_game: false,
        }
    }

    /// Drives signaling, punching, keepalives and relay; never blocks. Call every few ms.
    pub(crate) fn poll(&mut self) -> Vec<MeshEvent> {
        let now = Instant::now();
        if self.ready_pending {
            self.ready_pending = false;
            self.events.push(MeshEvent::Ready { me: self.me, code: None });
        }
        self.poll_server();
        self.poll_packets();
        match &mut self.mode {
            Mode::Room { .. } => self.drive_pairs(now),
            Mode::HostDirect | Mode::Fixed => {}
            Mode::JoinDirect { target, deadline, last_punch } => {
                if !self.routes.contains_key(&PeerId(0)) && !self.failed {
                    if now >= *deadline {
                        self.failed = true;
                        self.events.push(MeshEvent::Failed(NetError::Timeout));
                    } else if last_punch.is_none_or(|t| now - t >= PUNCH_EVERY) {
                        *last_punch = Some(now);
                        let target = *target;
                        let punch = Packet::Punch { token: 0, from: 1, to: 0 }.to_vec();
                        self.transport.send_udp(target, &punch);
                    }
                }
            }
        }
        if now - self.last_keepalive >= KEEPALIVE_EVERY {
            self.last_keepalive = now;
            let ka = Packet::Keepalive.to_vec();
            for (&peer, route) in &self.routes {
                if matches!(route, Route::Udp(_)) {
                    self.transport.send(peer, &ka);
                }
            }
        }
        std::mem::take(&mut self.events)
    }

    /// Sends a packet to a peer with a decided route; in game also over the relay while the
    /// peer's direct route is silent (`GAME_MIRROR_AFTER`).
    pub(crate) fn send(&mut self, to: PeerId, packet: &[u8]) {
        self.transport.send(to, packet);
        if self.mirroring(to) {
            self.transport.send_relay(to, packet);
        }
    }

    /// In game, the peer's direct route has been silent ≥ `GAME_MIRROR_AFTER`, or it was just
    /// re-adopted from the relay and the peer has not been heard on it yet.
    fn mirroring(&self, peer: PeerId) -> bool {
        self.in_game
            && matches!(self.routes.get(&peer), Some(Route::Udp(_)))
            && self
                .peers
                .get(&peer)
                .is_some_and(|st| st.returning || st.last_rx.is_some_and(|t| t.elapsed() >= GAME_MIRROR_AFTER))
    }

    pub(crate) fn me(&self) -> PeerId {
        self.me
    }

    /// Every peer with a decided route.
    pub(crate) fn routed(&self) -> Vec<(PeerId, RouteKind)> {
        let mut list: Vec<(PeerId, RouteKind)> =
            self.routes.keys().filter_map(|&id| Some((id, self.transport.route_kind(id)?))).collect();
        list.sort_by_key(|(id, _)| *id);
        list
    }

    /// Hands the live mesh (routes, relay WebSocket, route health) to the game.
    pub(crate) fn into_link(mut self) -> Link {
        self.in_game = true;
        Link { mesh: self, departed: Vec::new(), knocks: Vec::new(), regroup: false }
    }

    /// Leaves: BYE to peers, closes the WebSocket.
    pub(crate) fn close(mut self) {
        self.transport.close();
        if let Mode::Room { ws_out, .. } = &self.mode {
            let _ = ws_out.send(RelayOut::Close);
        }
    }

    // ---- room signaling ----

    fn poll_server(&mut self) {
        let Mode::Room { text_rx, close_rx, .. } = &self.mode else { return };
        let mut texts = Vec::new();
        while let Ok(t) = text_rx.try_recv() {
            texts.push(t);
        }
        let closed = close_rx.try_recv().ok();
        for t in texts {
            if let Ok(v) = serde_json::from_str::<Value>(&t) {
                self.on_server(&v);
            }
        }
        if let Some((code, reason)) = closed {
            if !self.failed {
                self.failed = true;
                let err = if code == CLOSE_HOST_LEFT {
                    NetError::HostLeft
                } else {
                    NetError::Server(format!("websocket closed: {code} {reason}"))
                };
                self.events.push(MeshEvent::Failed(err));
            }
        }
    }

    fn on_server(&mut self, v: &Value) {
        let id_of = |key: &str| v.get(key).and_then(Value::as_u64).map(|n| PeerId(n as u8));
        match v.get("t").and_then(Value::as_str) {
            Some("welcome") => {
                let Some(me) = id_of("id") else { return };
                let list = v.get("peers").and_then(Value::as_array).cloned().unwrap_or_default();
                let listed: Vec<(PeerId, String, bool)> = list
                    .iter()
                    .filter_map(|p| {
                        let id = PeerId(p.get("id")?.as_u64()? as u8);
                        let away = p.get("away").and_then(Value::as_bool).unwrap_or(false);
                        Some((id, p.get("name").and_then(Value::as_str).unwrap_or("").to_string(), away))
                    })
                    .collect();
                if self.welcomed {
                    // Rejoin after a WebSocket drop (same id): catch up on joins/leaves missed
                    // during the gap; known peers keep their routes.
                    let gone: Vec<PeerId> =
                        self.peers.keys().copied().filter(|id| !listed.iter().any(|(l, _, _)| l == id)).collect();
                    for id in gone {
                        self.peer_left(id);
                    }
                    for (id, name, away) in listed {
                        if !self.peers.contains_key(&id) {
                            self.peer_joined(id, name);
                        }
                        self.events.push(if away { MeshEvent::PeerAway { id } } else { MeshEvent::PeerBack { id } });
                    }
                    return;
                }
                self.welcomed = true;
                self.me = me;
                self.events.push(MeshEvent::Ready { me, code: self.code.clone() });
                for (id, name, away) in listed {
                    self.peer_joined(id, name);
                    if away {
                        self.events.push(MeshEvent::PeerAway { id });
                    }
                }
            }
            Some("joined") => {
                let Some(id) = id_of("id") else { return };
                let name = v.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                self.peer_joined(id, name);
            }
            Some("left") => {
                let Some(id) = id_of("id") else { return };
                self.peer_left(id);
            }
            Some("away") => {
                let Some(id) = id_of("id") else { return };
                self.events.push(MeshEvent::PeerAway { id });
            }
            Some("back") => {
                let Some(id) = id_of("id") else { return };
                self.events.push(MeshEvent::PeerBack { id });
            }
            Some("cands") => {
                let Some(from) = id_of("from") else { return };
                let Some(token) = v.get("token").and_then(Value::as_str).and_then(|s| u64::from_str_radix(s, 16).ok()) else {
                    return;
                };
                let cands: Vec<SocketAddr> = v
                    .get("cands")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|c| c.as_str()?.parse().ok().map(transport::unmap)).collect())
                    .unwrap_or_default();
                let relay_only = v.get("relay_only").and_then(Value::as_bool).unwrap_or(false);
                let now = Instant::now();
                let st = self.peers.entry(from).or_insert_with(PeerState::new);
                st.cands = cands;
                st.token = Some(token);
                st.relay_only = relay_only;
                st.punch_started.get_or_insert(now);
                if !st.sent_cands {
                    self.send_cands(from);
                }
            }
            Some("route") => {
                let Some(from) = id_of("from") else { return };
                let Some(st) = self.peers.get_mut(&from) else { return };
                if st.decided {
                    // Later changes: a dead direct route falls back to the relay, and a pair
                    // that fell back in game returns to direct on the lower id's word.
                    let back_to = st.fell_back_at.and(st.probe_ack_from.or(st.probe_seen).or(st.direct_addr));
                    match (v.get("mode").and_then(Value::as_str), self.routes.get(&from)) {
                        (Some("relay"), Some(Route::Udp(_))) => self.switch_to_relay(from),
                        (Some("direct"), Some(Route::Relay)) if from < self.me => {
                            if let Some(addr) = back_to {
                                self.switch_to_direct(from, addr);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                let route = match v.get("mode").and_then(Value::as_str) {
                    Some("direct") => {
                        let given = v.get("addr").and_then(Value::as_str).and_then(|s| s.parse().ok()).map(transport::unmap);
                        match st.seen_from.or(st.confirmed).or(given) {
                            Some(a) => Route::Udp(a),
                            None => return,
                        }
                    }
                    Some("relay") => Route::Relay,
                    _ => return,
                };
                self.adopt_route(from, route);
            }
            _ => {}
        }
    }

    fn peer_left(&mut self, id: PeerId) {
        self.peers.remove(&id);
        self.forget_route(id);
        self.events.push(MeshEvent::PeerLeft { id });
    }

    /// Stops sending to `id` (it said BYE or stopped answering); a device that joins under
    /// this id later gets a new route.
    pub(crate) fn forget_route(&mut self, id: PeerId) {
        if self.routes.remove(&id).is_some() {
            self.transport.remove_route(id);
        }
    }

    /// Moves a pair to the relay. In game a pair leaving a direct route counts as a fallback:
    /// it probes the direct path after `return_wait` and may return (`switch_to_direct`).
    fn switch_to_relay(&mut self, peer: PeerId) {
        let fell_from = match self.routes.get(&peer) {
            Some(&Route::Udp(addr)) if self.in_game && !self.force_relay => Some(addr),
            _ => None,
        };
        if let (Some(addr), Some(st)) = (fell_from, self.peers.get_mut(&peer)) {
            let now = Instant::now();
            if st.direct_since.is_some_and(|t| now - t >= RETURN_STABLE_AFTER) {
                st.fallbacks = 0;
            }
            st.fallbacks += 1;
            st.fell_back_at = Some(now);
            st.direct_addr = Some(addr);
            st.direct_since = None;
            st.returning = false;
            st.probe_seen = None;
            st.probe_ack_from = None;
            st.probe_acks.clear();
        }
        self.transport.set_route(peer, Route::Relay);
        self.routes.insert(peer, Route::Relay);
        self.events.push(MeshEvent::RouteChanged { id: peer, kind: RouteKind::Relay });
    }

    /// A fallen-back pair goes direct again at `addr`. The health clock starts fresh, and packets
    /// keep going over the relay too until the peer is heard on `addr` (`returning`).
    fn switch_to_direct(&mut self, peer: PeerId, addr: SocketAddr) {
        let Some(st) = self.peers.get_mut(&peer) else { return };
        let now = Instant::now();
        st.last_rx = Some(now);
        st.direct_since = Some(now);
        st.fell_back_at = None;
        st.returning = true;
        st.probe_seen = None;
        st.probe_ack_from = None;
        st.probe_acks.clear();
        st.last_punch = None;
        self.transport.set_route(peer, Route::Udp(addr));
        self.routes.insert(peer, Route::Udp(addr));
        self.events.push(MeshEvent::RouteChanged { id: peer, kind: RouteKind::Direct });
    }

    fn peer_joined(&mut self, id: PeerId, name: String) {
        if id == self.me {
            return;
        }
        self.peers.entry(id).or_insert_with(PeerState::new);
        self.events.push(MeshEvent::PeerJoined { id, name });
        self.send_cands(id);
    }

    fn send_cands(&mut self, to: PeerId) {
        let cands: Vec<String> = if self.force_relay {
            Vec::new()
        } else {
            self.my_cands.iter().map(|a| a.to_string()).collect()
        };
        let msg = json!({
            "t": "cands",
            "to": to.0,
            "cands": cands,
            "token": format!("{:016x}", self.token),
            "relay_only": self.force_relay,
        });
        self.transport.send_text(msg.to_string());
        if let Some(st) = self.peers.get_mut(&to) {
            st.sent_cands = true;
        }
    }

    fn adopt_route(&mut self, peer: PeerId, route: Route) {
        if let Some(st) = self.peers.get_mut(&peer) {
            let now = Instant::now();
            st.decided = true;
            st.last_rx = Some(now);
            st.direct_since = matches!(route, Route::Udp(_)).then_some(now);
        }
        self.transport.set_route(peer, route);
        self.routes.insert(peer, route);
        let kind = match route {
            Route::Udp(_) => RouteKind::Direct,
            Route::Relay => RouteKind::Relay,
        };
        self.events.push(MeshEvent::RouteUp { id: peer, kind });
    }

    /// Punches undecided peers and, as the lower id of a pair, decides the route; in game also
    /// probes fallen-back pairs and, as the lower id, returns them to direct.
    fn drive_pairs(&mut self, now: Instant) {
        let me = self.me;
        let relay_after = if self.symmetric { RELAY_AFTER_SYMMETRIC } else { RELAY_AFTER };
        let (repunch_after, dead_after) =
            if self.in_game { (GAME_MIRROR_AFTER, GAME_RELAY_AFTER) } else { (ROUTE_REPUNCH_AFTER, ROUTE_DEAD_AFTER) };
        let mut decisions: Vec<(PeerId, Route)> = Vec::new();
        let mut dead: Vec<PeerId> = Vec::new();
        let mut returns: Vec<(PeerId, SocketAddr)> = Vec::new();
        let ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in ids {
            let Some(st) = self.peers.get_mut(&peer) else { continue };
            if st.decided {
                if self.in_game && matches!(self.routes.get(&peer), Some(Route::Relay)) {
                    // Return probing, only for pairs that fell back from direct in game.
                    let Some(fell) = st.fell_back_at else { continue };
                    if now - fell < return_wait(st.fallbacks) {
                        continue;
                    }
                    st.probe_acks.retain(|&t| now - t <= RETURN_WINDOW);
                    if me < peer && st.probe_acks.len() >= RETURN_ACKS {
                        if let Some(addr) = st.probe_ack_from {
                            returns.push((peer, addr));
                            continue;
                        }
                    }
                    if st.last_punch.is_none_or(|t| now - t >= RETURN_PROBE_EVERY) {
                        st.last_punch = Some(now);
                        let punch = Packet::Punch { token: self.token, from: me.0, to: peer.0 }.to_vec();
                        let mut targets: Vec<SocketAddr> = Vec::new();
                        for a in st.direct_addr.into_iter().chain(st.probe_seen).chain(st.probe_ack_from).chain(st.cands.iter().copied()) {
                            if !targets.contains(&a) {
                                targets.push(a);
                            }
                        }
                        for a in targets {
                            self.transport.send_udp(a, &punch);
                        }
                    }
                    continue;
                }
                // Route health: a silent direct route is punched again, then given up.
                let Some(&Route::Udp(addr)) = self.routes.get(&peer) else { continue };
                let silent = st.last_rx.map_or(Duration::ZERO, |t| now - t);
                if silent >= dead_after && me < peer {
                    dead.push(peer);
                } else if silent >= repunch_after && st.last_punch.is_none_or(|t| now - t >= PUNCH_EVERY) {
                    st.last_punch = Some(now);
                    let punch = Packet::Punch { token: self.token, from: me.0, to: peer.0 }.to_vec();
                    self.transport.send_udp(addr, &punch);
                    for &c in st.cands.iter().filter(|&&c| c != addr) {
                        self.transport.send_udp(c, &punch);
                    }
                }
                continue;
            }
            if st.token.is_none() {
                continue;
            }
            let relay_only = self.force_relay || st.relay_only;
            if me < peer {
                if relay_only {
                    decisions.push((peer, Route::Relay));
                    continue;
                }
                if let Some(addr) = st.confirmed {
                    decisions.push((peer, Route::Udp(addr)));
                    continue;
                }
                if st.punch_started.is_some_and(|t| now - t >= relay_after) {
                    decisions.push((peer, Route::Relay));
                    continue;
                }
            }
            if relay_only {
                continue;
            }
            if st.last_punch.is_none_or(|t| now - t >= PUNCH_EVERY) {
                st.last_punch = Some(now);
                let punch = Packet::Punch { token: self.token, from: me.0, to: peer.0 }.to_vec();
                for &c in &st.cands {
                    self.transport.send_udp(c, &punch);
                }
            }
        }
        for (peer, route) in decisions {
            let msg = match route {
                Route::Udp(addr) => json!({"t": "route", "to": peer.0, "mode": "direct", "addr": addr.to_string()}),
                Route::Relay => json!({"t": "route", "to": peer.0, "mode": "relay"}),
            };
            self.transport.send_text(msg.to_string());
            self.adopt_route(peer, route);
        }
        for peer in dead {
            self.transport.send_text(json!({"t": "route", "to": peer.0, "mode": "relay"}).to_string());
            self.switch_to_relay(peer);
        }
        for (peer, addr) in returns {
            let msg = json!({"t": "route", "to": peer.0, "mode": "direct", "addr": addr.to_string()});
            self.transport.send_text(msg.to_string());
            self.switch_to_direct(peer, addr);
        }
    }

    // ---- packets ----

    fn poll_packets(&mut self) {
        while let Some((src, data)) = self.transport.recv_raw() {
            if let Some(p) = self.routed_source(src).filter(|_| matches!(src, Source::Udp(_))) {
                let probe = matches!(Packet::parse(&data), Some(Packet::Punch { .. } | Packet::PunchAck { .. }));
                if let Some(st) = self.peers.get_mut(&p) {
                    st.last_rx = Some(Instant::now());
                    // Only a peer that has switched sends us non-punch packets on the direct route.
                    if !probe {
                        st.returning = false;
                    }
                }
            }
            match Packet::parse(&data) {
                Some(Packet::Punch { token, from, to }) => self.on_punch(src, token, PeerId(from), PeerId(to)),
                Some(Packet::PunchAck { token, from, to }) => {
                    self.on_punch_ack(src, token, PeerId(from), PeerId(to))
                }
                Some(Packet::Keepalive) => {}
                Some(_) => {
                    if let Some(from) = self.routed_source(src) {
                        self.events.push(MeshEvent::Packet { from, data });
                    }
                }
                None => {}
            }
        }
    }

    fn routed_source(&self, src: Source) -> Option<PeerId> {
        match src {
            Source::Relay(p) => self.routes.contains_key(&p).then_some(p),
            Source::Udp(addr) => self.transport.peer_at(addr),
        }
    }

    /// A decided direct peer punching (or acking) with its token from a new address moved
    /// (NAT rebinding, Wi-Fi → LTE): its route follows once the old address is silent
    /// (`ROAM_AFTER`). A relay-routed pair does not roam; a fallen-back one may return to direct
    /// through probing (`drive_pairs`, `switch_to_direct`).
    fn roam(&mut self, peer: PeerId, addr: SocketAddr) {
        let Some(&Route::Udp(old)) = self.routes.get(&peer) else { return };
        let Some(st) = self.peers.get_mut(&peer) else { return };
        if old == addr || !st.decided || st.last_rx.is_some_and(|t| t.elapsed() < ROAM_AFTER) {
            return;
        }
        st.last_rx = Some(Instant::now());
        self.transport.set_route(peer, Route::Udp(addr));
        self.routes.insert(peer, Route::Udp(addr));
        self.events.push(MeshEvent::RouteChanged { id: peer, kind: RouteKind::Direct });
    }

    fn on_punch(&mut self, src: Source, token: u64, from: PeerId, to: PeerId) {
        let Source::Udp(addr) = src else { return };
        if to != self.me {
            return;
        }
        match self.mode {
            Mode::HostDirect => {
                if from != PeerId(1) || token != 0 {
                    return;
                }
                let ack = Packet::PunchAck { token: 0, from: 0, to: 1 }.to_vec();
                self.transport.send_udp(addr, &ack);
                if !self.routes.contains_key(&from) {
                    self.transport.set_route(from, Route::Udp(addr));
                    self.routes.insert(from, Route::Udp(addr));
                    self.events.push(MeshEvent::PeerJoined { id: from, name: String::new() });
                    self.events.push(MeshEvent::RouteUp { id: from, kind: RouteKind::Direct });
                }
            }
            Mode::JoinDirect { .. } | Mode::Fixed => {}
            Mode::Room { .. } => {
                let probing = self.in_game && matches!(self.routes.get(&from), Some(Route::Relay));
                let Some(st) = self.peers.get_mut(&from) else { return };
                if st.token != Some(token) {
                    return;
                }
                st.seen_from.get_or_insert(addr);
                if probing && st.fell_back_at.is_some() {
                    st.probe_seen = Some(addr);
                }
                self.roam(from, addr);
                let ack = Packet::PunchAck { token: self.token, from: self.me.0, to: from.0 }.to_vec();
                self.transport.send_udp(addr, &ack);
            }
        }
    }

    fn on_punch_ack(&mut self, src: Source, token: u64, from: PeerId, to: PeerId) {
        let Source::Udp(addr) = src else { return };
        if to != self.me {
            return;
        }
        match self.mode {
            Mode::HostDirect | Mode::Fixed => {}
            Mode::JoinDirect { .. } => {
                if from != PeerId(0) || token != 0 || self.routes.contains_key(&from) {
                    return;
                }
                self.transport.set_route(from, Route::Udp(addr));
                self.routes.insert(from, Route::Udp(addr));
                self.events.push(MeshEvent::PeerJoined { id: from, name: String::new() });
                self.events.push(MeshEvent::RouteUp { id: from, kind: RouteKind::Direct });
            }
            Mode::Room { .. } => {
                let probing = self.in_game && matches!(self.routes.get(&from), Some(Route::Relay));
                let Some(st) = self.peers.get_mut(&from) else { return };
                if st.token != Some(token) {
                    return;
                }
                st.confirmed.get_or_insert(addr);
                let now = Instant::now();
                if probing && st.fell_back_at.is_some_and(|t| now - t >= return_wait(st.fallbacks)) {
                    if st.probe_ack_from != Some(addr) {
                        st.probe_ack_from = Some(addr);
                        st.probe_acks.clear();
                    }
                    if st.probe_acks.last().is_none_or(|&t| now - t >= RETURN_ACK_SPACING) {
                        st.probe_acks.push(now);
                    }
                }
                self.roam(from, addr);
            }
        }
    }
}

/// The mesh as the game sees it (`GameStart::link`): `NetSession` drives it on every poll, so
/// signaling (`route`), route health, re-punching, relay mirroring and roaming keep running in
/// game; only GGRS and BYE packets reach GGRS. `Send`, so the lobby thread can hand it over.
/// The room stays open while the game runs: a device that joins it gets its routes as usual
/// and shows up in `take_knocks`.
pub struct Link {
    mesh: Mesh,
    departed: Vec<PeerId>,
    knocks: Vec<PeerId>,
    regroup: bool,
}

impl Link {
    /// Server-less link on fixed UDP routes (no relay, no punching): direct-IP tests and tools.
    pub fn udp(udp: std::net::UdpSocket, me: PeerId, peers: &[(PeerId, SocketAddr)]) -> Link {
        let mut mesh = Mesh::base(Transport::udp(udp, peers), Mode::Fixed, me, false, Instant::now());
        mesh.ready_pending = false;
        for &(p, a) in peers {
            mesh.routes.insert(p, Route::Udp(transport::unmap(a)));
        }
        mesh.into_link()
    }

    pub fn me(&self) -> PeerId {
        self.mesh.me()
    }

    pub fn route_kind(&self, peer: PeerId) -> Option<RouteKind> {
        self.mesh.transport.route_kind(peer)
    }

    /// Peers that sent BYE since the last call.
    pub fn take_departed(&mut self) -> Vec<PeerId> {
        std::mem::take(&mut self.departed)
    }

    /// Devices that joined the room since the last call: the route to each is decided, so the
    /// room screen can take them in.
    pub fn take_knocks(&mut self) -> Vec<PeerId> {
        std::mem::take(&mut self.knocks)
    }

    /// The host called everyone back to the room (`{"t":"regroup"}`) since the last call.
    pub fn take_regroup(&mut self) -> bool {
        std::mem::take(&mut self.regroup)
    }

    /// Stops sending to a player who stopped answering. A device that joins the room under
    /// that id later gets a new route and counts as a newcomer (`take_knocks`).
    pub fn forget(&mut self, peer: PeerId) {
        self.mesh.forget_route(peer);
    }

    /// Gives the mesh back to the lobby (the game returns to the room on the same routes).
    pub(crate) fn into_mesh(mut self) -> Mesh {
        self.mesh.in_game = false;
        self.mesh
    }

    /// BYE to every routed peer, closes the relay WebSocket.
    pub fn close(self) {
        self.mesh.close();
    }
}

impl ggrs::NonBlockingSocket<PeerId> for Link {
    /// GGRS packet = `Packet::Ggrs(bincode 1.3 of msg)`.
    fn send_to(&mut self, msg: &ggrs::Message, addr: &PeerId) {
        let Ok(body) = bincode::serialize(msg) else { return };
        let pkt = Packet::Ggrs(&body).to_vec();
        if pkt.len() <= crate::packet::MAX_PACKET {
            self.mesh.send(*addr, &pkt);
        }
    }

    /// Polls the mesh (signaling, punches, keepalives, health), decodes GGRS packets, records
    /// BYE senders for `take_departed`, newcomers for `take_knocks` and the host's call back to
    /// the room, ignores the rest (late CTRL etc.).
    fn receive_all_messages(&mut self) -> Vec<(PeerId, ggrs::Message)> {
        let mut out = Vec::new();
        for ev in self.mesh.poll() {
            let (from, data) = match ev {
                MeshEvent::Packet { from, data } => (from, data),
                // Every pair of players was routed before the start, so a route that comes up
                // now belongs to a device that joined the room during the game.
                MeshEvent::RouteUp { id, .. } => {
                    if !self.knocks.contains(&id) {
                        self.knocks.push(id);
                    }
                    continue;
                }
                _ => continue,
            };
            match Packet::parse(&data) {
                Some(Packet::Ggrs(body)) => {
                    if let Ok(m) = bincode::deserialize::<ggrs::Message>(body) {
                        out.push((from, m));
                    }
                }
                Some(Packet::Bye) => {
                    if !self.departed.contains(&from) {
                        self.departed.push(from);
                    }
                    self.mesh.forget_route(from);
                }
                // Not acked here: the host repeats it until the lobby, which owns the CTRL
                // sequence numbers, has the mesh again and acks it.
                Some(Packet::Ctrl { json, .. }) if from == PeerId(0) => {
                    let called = serde_json::from_slice::<Value>(json).is_ok_and(|v| v["t"] == "regroup");
                    self.regroup |= called;
                }
                _ => {}
            }
        }
        out
    }
}

/// `host[:port]`, `[v6]:port` or a bare IPv6 literal → socket address, default port 7777,
/// IPv4 preferred for hostnames.
fn resolve_direct(addr: &str) -> Result<SocketAddr, NetError> {
    let addr = addr.trim();
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Ok(transport::unmap(a));
    }
    if let Ok(ip) = addr.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        return Ok(transport::unmap(SocketAddr::new(ip, DEFAULT_DIRECT_PORT)));
    }
    let with_port = if addr.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        addr.to_string()
    } else {
        format!("{addr}:{DEFAULT_DIRECT_PORT}")
    };
    let all: Vec<SocketAddr> = with_port
        .to_socket_addrs()
        .map_err(|e| NetError::Io(format!("{with_port}: {e}")))?
        .collect();
    all.iter()
        .find(|a| a.is_ipv4())
        .or(all.first())
        .copied()
        .ok_or_else(|| NetError::Io(format!("{with_port}: no address")))
}

/// Connects the room WebSocket; returns it with the room code. Host with no code retries
/// random codes on `room_taken`.
fn connect_room(cfg: &MeshConfig) -> Result<(Ws, String, String), NetError> {
    // Pin ring even if another dependency ever enables a second rustls provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = cfg.server.trim().trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    let name = urlencode(&cfg.nickname);
    let attempt = |code: &str, role: &str| -> Result<(Ws, String), NetError> {
        let url = format!("{base}/v1/room/{code}?role={role}&name={name}");
        match tungstenite::connect(url.as_str()) {
            Ok((ws, _)) => Ok((ws, url)),
            Err(tungstenite::Error::Http(resp)) => {
                let status = resp.status().as_u16();
                let body = resp.body().as_deref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                let code = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
                Err(match code.as_deref() {
                    Some("room_taken") => NetError::RoomTaken,
                    Some("no_room") => NetError::NoRoom,
                    Some("room_full") => NetError::RoomFull,
                    Some("room_locked") => NetError::RoomLocked,
                    _ => NetError::Server(format!("HTTP {status}: {body}")),
                })
            }
            Err(e) => Err(NetError::Server(e.to_string())),
        }
    };
    match &cfg.role {
        MeshRole::JoinRoom { code } => {
            let code = normalize_code(code).ok_or(NetError::NoRoom)?;
            attempt(&code, "guest").map(|(ws, url)| (ws, code, url))
        }
        MeshRole::HostRoom { code: Some(code) } => {
            let code = normalize_code(code).ok_or_else(|| NetError::Server(format!("bad room code {code}")))?;
            attempt(&code, "host").map(|(ws, url)| (ws, code, url))
        }
        MeshRole::HostRoom { code: None } => {
            for _ in 0..HOST_CODE_ATTEMPTS {
                let code = random_code();
                match attempt(&code, "host") {
                    Ok((ws, url)) => return Ok((ws, code, url)),
                    Err(NetError::RoomTaken) => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(NetError::RoomTaken)
        }
        MeshRole::HostDirect { .. } | MeshRole::JoinDirect { .. } => {
            Err(NetError::Server("direct modes have no signaling server".into()))
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn set_read_timeout(ws: &Ws, timeout: Duration) {
    let tcp: &TcpStream = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_ref(),
        _ => return,
    };
    let _ = tcp.set_read_timeout(Some(timeout));
}

/// How one WebSocket connection ended.
enum WsEnd {
    /// The mesh asked to close (or dropped its sender).
    Closed,
    /// The server closed it on purpose (4000 host_left, 4001 room_expired, 4002 replaced).
    Fatal(u16, String),
    /// Network drop: reconnect with `rejoin`.
    Dropped(String),
}

/// Owns the WebSocket: text → `text_tx`, binary `[src][packet]` → `in_tx`, `RelayOut` →
/// frames, `ping` every 20 s. A dropped connection is rejoined (`?rejoin=<id>&secret=`,
/// backoff 0.5/1/2/4 s, up to 60 s) for the lobby and the game alike; relay packets during the
/// gap are lost, signaling text is queued. Reports a final close unless the mesh asked for it.
fn ws_thread(
    mut ws: Ws,
    url: String,
    out_rx: Receiver<RelayOut>,
    text_tx: Sender<String>,
    in_tx: Sender<(PeerId, Vec<u8>)>,
    close_tx: Sender<(u16, String)>,
) {
    let mut rejoin: Option<(u8, String)> = None;
    let mut queued: Vec<String> = Vec::new();
    loop {
        set_read_timeout(&ws, WS_READ_TIMEOUT);
        let mut flush_ok = true;
        for t in queued.drain(..) {
            flush_ok &= ws.send(Message::text(t)).is_ok();
        }
        let end = if flush_ok {
            ws_pump(&mut ws, &out_rx, &text_tx, &in_tx, &mut rejoin)
        } else {
            WsEnd::Dropped("send failed".into())
        };
        let detail = match end {
            WsEnd::Closed => return,
            WsEnd::Fatal(code, reason) => {
                let _ = close_tx.send((code, reason));
                return;
            }
            WsEnd::Dropped(detail) => detail,
        };
        let Some((id, secret)) = rejoin.clone() else {
            let _ = close_tx.send((1006, detail));
            return;
        };
        let rejoin_url = format!("{url}&rejoin={id}&secret={secret}");
        match ws_reconnect(&rejoin_url, &out_rx, &mut queued) {
            Ok(Some(new)) => ws = new,
            Ok(None) => return,
            Err(e) => {
                let _ = close_tx.send((1006, format!("{detail}; rejoin failed: {e}")));
                return;
            }
        }
    }
}

fn ws_pump(
    ws: &mut Ws,
    out_rx: &Receiver<RelayOut>,
    text_tx: &Sender<String>,
    in_tx: &Sender<(PeerId, Vec<u8>)>,
    rejoin: &mut Option<(u8, String)>,
) -> WsEnd {
    let mut last_ping = Instant::now();
    let mut close_code: Option<(u16, String)> = None;
    let ended = |close_code: Option<(u16, String)>, detail: String| match close_code {
        Some((code, reason)) if (4000..5000).contains(&code) => WsEnd::Fatal(code, reason),
        _ => WsEnd::Dropped(detail),
    };
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let t = t.as_str();
                if t != "pong" {
                    if t.starts_with("{\"t\":\"welcome\"") {
                        if let Ok(v) = serde_json::from_str::<Value>(t) {
                            let id = v.get("id").and_then(Value::as_u64);
                            let secret = v.get("secret").and_then(Value::as_str);
                            if let (Some(id), Some(secret)) = (id, secret) {
                                *rejoin = Some((id as u8, urlencode(secret)));
                            }
                        }
                    }
                    let _ = text_tx.send(t.to_string());
                }
            }
            Ok(Message::Binary(b)) => {
                if let Some((&src, packet)) = b.split_first() {
                    let _ = in_tx.send((PeerId(src), packet.to_vec()));
                }
            }
            Ok(Message::Close(frame)) => {
                if let Some(f) = frame {
                    close_code = Some((u16::from(f.code), f.reason.as_str().to_string()));
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return ended(close_code, e.to_string()),
        }
        loop {
            let res = match out_rx.try_recv() {
                Ok(RelayOut::Text(s)) => ws.send(Message::text(s)),
                Ok(RelayOut::Binary { to, packet }) => {
                    let mut frame = Vec::with_capacity(packet.len() + 1);
                    frame.push(to.0);
                    frame.extend_from_slice(&packet);
                    ws.send(Message::binary(frame))
                }
                Ok(RelayOut::Close) | Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    let deadline = Instant::now() + Duration::from_millis(500);
                    while Instant::now() < deadline {
                        match ws.read() {
                            Err(tungstenite::Error::Io(e))
                                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                            Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    return WsEnd::Closed;
                }
                #[cfg(test)]
                Ok(RelayOut::Drop) => {
                    if let MaybeTlsStream::Rustls(tls) = ws.get_ref() {
                        let _ = tls.get_ref().shutdown(std::net::Shutdown::Both);
                    } else if let MaybeTlsStream::Plain(tcp) = ws.get_ref() {
                        let _ = tcp.shutdown(std::net::Shutdown::Both);
                    }
                    return WsEnd::Dropped("test drop".into());
                }
                Err(TryRecvError::Empty) => break,
            };
            if let Err(e) = res {
                if !matches!(&e, tungstenite::Error::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)) {
                    return ended(close_code, e.to_string());
                }
            }
        }
        if last_ping.elapsed() >= WS_PING_EVERY {
            last_ping = Instant::now();
            let _ = ws.send(Message::text("ping"));
        }
    }
}

/// Reconnects after a drop. `Ok(None)` = the mesh asked to close meanwhile; `Err` = gave up
/// (60 s passed or the server refused the rejoin: id expired or room gone).
fn ws_reconnect(url: &str, out_rx: &Receiver<RelayOut>, queued: &mut Vec<String>) -> Result<Option<Ws>, String> {
    let started = Instant::now();
    let mut step = 0usize;
    loop {
        let delay = Duration::from_millis(WS_BACKOFF_MS[step.min(WS_BACKOFF_MS.len() - 1)]);
        step += 1;
        let wake = Instant::now() + delay;
        while Instant::now() < wake {
            // Keep draining: relay packets are dropped, signaling text waits for the new socket.
            loop {
                match out_rx.try_recv() {
                    Ok(RelayOut::Text(t)) => {
                        if queued.len() < WS_QUEUE_MAX {
                            queued.push(t);
                        }
                    }
                    Ok(RelayOut::Binary { .. }) => {}
                    #[cfg(test)]
                    Ok(RelayOut::Drop) => {}
                    Ok(RelayOut::Close) | Err(TryRecvError::Disconnected) => return Ok(None),
                    Err(TryRecvError::Empty) => break,
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        if started.elapsed() >= WS_REJOIN_WINDOW {
            return Err("gave up after 60 s".into());
        }
        match ws_connect_once(url) {
            Ok(ws) => return Ok(Some(ws)),
            Err(tungstenite::Error::Http(resp)) if resp.status().is_client_error() => {
                return Err(format!("HTTP {}", resp.status().as_u16()));
            }
            Err(_) => {}
        }
    }
}

/// `tungstenite::connect` with a TCP connect timeout (a dead Wi-Fi must not hang a retry).
fn ws_connect_once(url: &str) -> Result<Ws, tungstenite::Error> {
    let io = |m: String| tungstenite::Error::Io(std::io::Error::other(m));
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        return Err(io(format!("bad url {url}")));
    };
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let host_port = if authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        authority.to_string()
    } else {
        format!("{authority}:{}", if tls { 443 } else { 80 })
    };
    let addrs: Vec<SocketAddr> = host_port.to_socket_addrs().map_err(tungstenite::Error::Io)?.collect();
    let mut last = io(format!("{host_port}: no address"));
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, WS_CONNECT_TIMEOUT) {
            Ok(tcp) => {
                let _ = tcp.set_read_timeout(Some(WS_CONNECT_TIMEOUT));
                let _ = tcp.set_nodelay(true);
                return match tungstenite::client_tls(url, tcp) {
                    Ok((ws, _)) => Ok(ws),
                    Err(tungstenite::HandshakeError::Failure(e)) => Err(e),
                    Err(tungstenite::HandshakeError::Interrupted(_)) => Err(io("handshake interrupted".into())),
                };
            }
            Err(e) => last = tungstenite::Error::Io(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::net::UdpSocket;

    const SERVER: &str = "https://ebobo-signal.mc-se.workers.dev";

    struct Node {
        mesh: Mesh,
        me: Option<PeerId>,
        code: Option<String>,
        routes: HashMap<PeerId, RouteKind>,
        got: Vec<(PeerId, Vec<u8>)>,
    }

    impl Node {
        fn new(mesh: Mesh) -> Node {
            Node { mesh, me: None, code: None, routes: HashMap::new(), got: Vec::new() }
        }

        fn pump(&mut self) {
            for ev in self.mesh.poll() {
                match ev {
                    MeshEvent::Ready { me, code } => {
                        self.me = Some(me);
                        self.code = code;
                    }
                    MeshEvent::RouteUp { id, kind } | MeshEvent::RouteChanged { id, kind } => {
                        self.routes.insert(id, kind);
                    }
                    MeshEvent::Packet { from, data } => self.got.push((from, data)),
                    MeshEvent::Failed(e) => panic!("mesh failed: {e:?}"),
                    MeshEvent::PeerJoined { .. }
                    | MeshEvent::PeerLeft { .. }
                    | MeshEvent::PeerAway { .. }
                    | MeshEvent::PeerBack { .. } => {}
                }
            }
        }
    }

    fn pump_until(nodes: &mut [Node], secs: u64, what: &str, done: impl Fn(&[Node]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done(nodes) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            for n in nodes.iter_mut() {
                n.pump();
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Every node sends a CTRL to every routed peer and waits until all arrive.
    fn exchange(nodes: &mut [Node]) {
        for n in nodes.iter_mut() {
            let me = n.me.expect("ready").0;
            let peers: Vec<PeerId> = n.routes.keys().copied().collect();
            for p in peers {
                let json = format!("{{\"from\":{me}}}");
                let pkt = Packet::Ctrl { seq: 1, json: json.as_bytes() }.to_vec();
                n.mesh.send(p, &pkt);
            }
        }
        pump_until(nodes, 10, "packets", |ns| {
            ns.iter().all(|n| {
                let senders: HashSet<PeerId> = n
                    .got
                    .iter()
                    .filter(|(from, d)| {
                        matches!(Packet::parse(d), Some(Packet::Ctrl { json, .. }) if json == format!("{{\"from\":{}}}", from.0).as_bytes())
                    })
                    .map(|(f, _)| *f)
                    .collect();
                n.routes.keys().all(|p| senders.contains(p))
            })
        });
    }

    fn cfg(role: MeshRole, force_relay: bool) -> MeshConfig {
        MeshConfig { server: SERVER.into(), nickname: "Test Ä".into(), role, force_relay }
    }

    #[test]
    fn mesh_direct_mode_localhost() {
        let port = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let host = Mesh::open(cfg(MeshRole::HostDirect { port }, false)).unwrap();
        let guest = Mesh::open(cfg(MeshRole::JoinDirect { addr: format!("127.0.0.1:{port}") }, false)).unwrap();
        let mut nodes = [Node::new(host), Node::new(guest)];
        pump_until(&mut nodes, 5, "direct routes", |ns| {
            ns[0].routes.get(&PeerId(1)) == Some(&RouteKind::Direct)
                && ns[1].routes.get(&PeerId(0)) == Some(&RouteKind::Direct)
        });
        assert_eq!(nodes[0].me, Some(PeerId(0)));
        assert_eq!(nodes[1].me, Some(PeerId(1)));
        exchange(&mut nodes);
        let [a, b] = nodes;
        a.mesh.close();
        b.mesh.close();
    }

    #[test]
    fn mesh_direct_mode_localhost_v6() {
        let Ok(probe) = UdpSocket::bind("[::1]:0") else {
            eprintln!("IPv6 loopback unavailable; skipped");
            return;
        };
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let host = Mesh::open(cfg(MeshRole::HostDirect { port }, false)).unwrap();
        let guest = Mesh::open(cfg(MeshRole::JoinDirect { addr: format!("[::1]:{port}") }, false)).unwrap();
        let mut nodes = [Node::new(host), Node::new(guest)];
        pump_until(&mut nodes, 5, "direct routes", |ns| {
            ns[0].routes.get(&PeerId(1)) == Some(&RouteKind::Direct)
                && ns[1].routes.get(&PeerId(0)) == Some(&RouteKind::Direct)
        });
        exchange(&mut nodes);
        let [a, b] = nodes;
        a.mesh.close();
        b.mesh.close();
    }

    #[test]
    fn mesh_resolve_direct_forms() {
        let r = |s: &str| resolve_direct(s).unwrap();
        assert_eq!(r("192.0.2.1"), "192.0.2.1:7777".parse().unwrap());
        assert_eq!(r("192.0.2.1:9000"), "192.0.2.1:9000".parse().unwrap());
        assert_eq!(r("[2001:db8::1]:9000"), "[2001:db8::1]:9000".parse().unwrap());
        assert_eq!(r("2001:db8::1"), "[2001:db8::1]:7777".parse().unwrap());
        assert_eq!(r("[2001:db8::1]"), "[2001:db8::1]:7777".parse().unwrap());
        assert_eq!(r("::1"), "[::1]:7777".parse().unwrap());
        assert_eq!(r("[::ffff:192.0.2.1]:5"), "192.0.2.1:5".parse().unwrap());
        assert!(r("localhost:5").port() == 5);
    }

    /// Opens a room mesh with an extra loopback candidate: this Mac's application firewall
    /// drops UDP sent to its own LAN address from unsigned binaries, so same-machine punching
    /// needs 127.0.0.1 (real peers use the LAN/STUN candidates).
    fn open_live(role: MeshRole, force_relay: bool) -> Mesh {
        let mut mesh = Mesh::open(cfg(role, force_relay)).expect("open");
        let port = mesh.my_cands.first().map(|a| a.port()).expect("candidates");
        mesh.my_cands.push(SocketAddr::from(([127, 0, 0, 1], port)));
        mesh
    }

    fn live_room(guests: usize, force_relay: bool) -> Vec<Node> {
        let host = open_live(MeshRole::HostRoom { code: None }, force_relay);
        let mut nodes = vec![Node::new(host)];
        pump_until(&mut nodes, 10, "host welcome", |ns| ns[0].code.is_some());
        let code = nodes[0].code.clone().unwrap();
        for _ in 0..guests {
            nodes.push(Node::new(open_live(MeshRole::JoinRoom { code: code.clone() }, force_relay)));
        }
        let total = nodes.len();
        pump_until(&mut nodes, 20, "all routes", |ns| ns.iter().all(|n| n.routes.len() == total - 1));
        for n in &nodes {
            assert_eq!(n.code.as_deref(), Some(code.as_str()));
            eprintln!("{:?} cands {:?} symmetric {} routes {:?}", n.me, n.mesh.my_cands, n.mesh.symmetric, n.routes);
            for (p, st) in &n.mesh.peers {
                eprintln!("  peer {p:?} cands {:?} seen_from {:?} confirmed {:?}", st.cands, st.seen_from, st.confirmed);
            }
        }
        let ids: HashSet<PeerId> = nodes.iter().map(|n| n.me.unwrap()).collect();
        assert_eq!(ids.len(), total);
        nodes
    }

    #[test]
    #[ignore]
    fn mesh_live_two_direct() {
        let mut nodes = live_room(1, false);
        assert!(nodes.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Direct)));
        exchange(&mut nodes);
        nodes.into_iter().for_each(|n| n.mesh.close());
    }

    #[test]
    #[ignore]
    fn mesh_live_two_relay() {
        let mut nodes = live_room(1, true);
        assert!(nodes.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Relay)));
        exchange(&mut nodes);
        nodes.into_iter().for_each(|n| n.mesh.close());
    }

    #[test]
    #[ignore]
    fn mesh_live_three() {
        let mut nodes = live_room(2, false);
        assert!(nodes.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Direct)));
        exchange(&mut nodes);
        nodes.into_iter().for_each(|n| n.mesh.close());
    }

    /// Sends CTRL packets every 200 ms until every node heard every routed peer (packets in a
    /// relay gap are lost, so one round is not enough right after a reconnect).
    fn exchange_until(nodes: &mut [Node], secs: u64) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        for n in nodes.iter_mut() {
            n.got.clear();
        }
        loop {
            for n in nodes.iter_mut() {
                let me = n.me.expect("ready").0;
                let peers: Vec<PeerId> = n.routes.keys().copied().collect();
                for p in peers {
                    let json = format!("{{\"from\":{me}}}");
                    n.mesh.send(p, &Packet::Ctrl { seq: 1, json: json.as_bytes() }.to_vec());
                }
            }
            let t = Instant::now() + Duration::from_millis(200);
            while Instant::now() < t {
                for n in nodes.iter_mut() {
                    n.pump();
                }
                thread::sleep(Duration::from_millis(5));
            }
            let done = nodes.iter().all(|n| n.routes.keys().all(|p| n.got.iter().any(|(f, _)| f == p)));
            if done {
                return;
            }
            assert!(Instant::now() < deadline, "relay never recovered");
        }
    }

    /// Both WebSockets drop (no close frame); each rejoins with its secret and the relay works
    /// again with the same ids and routes.
    #[test]
    #[ignore]
    fn mesh_live_ws_rejoin_keeps_relay() {
        let mut nodes = live_room(1, true);
        exchange(&mut nodes);
        let ids: Vec<PeerId> = nodes.iter().map(|n| n.me.unwrap()).collect();
        for n in &nodes {
            let Mode::Room { ws_out, .. } = &n.mesh.mode else { panic!("room mode") };
            ws_out.send(RelayOut::Drop).unwrap();
        }
        thread::sleep(Duration::from_millis(100));
        exchange_until(&mut nodes, 30);
        assert_eq!(nodes.iter().map(|n| n.me.unwrap()).collect::<Vec<_>>(), ids);
        assert!(nodes.iter().all(|n| n.routes.len() == 1 && n.routes.values().all(|k| *k == RouteKind::Relay)));
        nodes.into_iter().for_each(|n| n.mesh.close());
    }

    /// A direct route that goes silent (the decider's route points at a dead address) is
    /// re-punched after 3 s and moved to the relay after 5 s on both sides; traffic resumes.
    #[test]
    #[ignore]
    fn mesh_live_dead_direct_route_falls_back_to_relay() {
        let mut nodes = live_room(1, false);
        assert!(nodes.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Direct)));
        // Kill the direct path both ways and forget the candidates, so re-punching cannot heal
        // it by roaming back to a live address (that path has its own unit test).
        let ids: Vec<PeerId> = nodes.iter().map(|n| n.me.unwrap()).collect();
        let dead = Route::Udp(SocketAddr::from(([127, 0, 0, 1], 9)));
        for (i, n) in nodes.iter_mut().enumerate() {
            let peer = ids[1 - i];
            n.mesh.transport.set_route(peer, dead);
            n.mesh.routes.insert(peer, dead);
            if let Some(st) = n.mesh.peers.get_mut(&peer) {
                st.cands.clear();
            }
        }
        let started = Instant::now();
        pump_until(&mut nodes, 15, "relay fallback", |ns| {
            ns.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Relay))
        });
        let took = started.elapsed();
        assert!(took >= Duration::from_secs(4), "switched too early: {took:?}");
        exchange_until(&mut nodes, 10);
        nodes.into_iter().for_each(|n| n.mesh.close());
    }

    /// A pair that fell back to the relay in game returns to direct once the direct path works
    /// again (probing starts `RETURN_FIRST_WAIT` after the fallback); traffic keeps flowing.
    #[test]
    #[ignore]
    fn mesh_live_fallen_back_pair_returns_to_direct() {
        let mut nodes = live_room(1, false);
        assert!(nodes.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Direct)));
        let ids: Vec<PeerId> = nodes.iter().map(|n| n.me.unwrap()).collect();
        let dead = Route::Udp(SocketAddr::from(([127, 0, 0, 1], 9)));
        let mut saved = Vec::new();
        for (i, n) in nodes.iter_mut().enumerate() {
            let peer = ids[1 - i];
            n.mesh.in_game = true;
            n.mesh.transport.set_route(peer, dead);
            n.mesh.routes.insert(peer, dead);
            let st = n.mesh.peers.get_mut(&peer).unwrap();
            saved.push(std::mem::take(&mut st.cands));
        }
        pump_until(&mut nodes, 15, "relay fallback", |ns| {
            ns.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Relay))
        });
        // The path works again: the peers' candidates lead to their live sockets.
        for (i, n) in nodes.iter_mut().enumerate() {
            n.mesh.peers.get_mut(&ids[1 - i]).unwrap().cands = saved[i].clone();
        }
        pump_until(&mut nodes, 30, "return to direct", |ns| {
            ns.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Direct))
        });
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(n.mesh.transport.route_kind(ids[1 - i]), Some(RouteKind::Direct));
        }
        exchange_until(&mut nodes, 10);
        nodes.into_iter().for_each(|n| n.mesh.close());
    }

    // ---- in-game link ----

    fn ggrs_msg() -> ggrs::Message {
        // Message { header: { magic: 0x1234 }, body: SyncRequest { random_request: 42 } }
        let mut raw = 0x1234u16.to_le_bytes().to_vec();
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&42u32.to_le_bytes());
        bincode::deserialize(&raw).unwrap()
    }

    fn local_sock() -> UdpSocket {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        s
    }

    fn udp_pair() -> (Link, Link) {
        let (a, b) = (local_sock(), local_sock());
        let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        (Link::udp(a, PeerId(0), &[(PeerId(1), ba)]), Link::udp(b, PeerId(1), &[(PeerId(0), aa)]))
    }

    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            if let Some(v) = f() {
                return Some(v);
            }
            thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn link_ggrs_message_round_trip() {
        use ggrs::NonBlockingSocket;
        let (mut a, mut b) = udp_pair();
        a.send_to(&ggrs_msg(), &PeerId(1));
        let got = wait_for(|| b.receive_all_messages().pop()).unwrap();
        assert_eq!(got, (PeerId(0), ggrs_msg()));
    }

    #[test]
    fn link_ggrs_framing_and_bye() {
        use ggrs::NonBlockingSocket;
        let (mut a, mut b) = udp_pair();
        // A GGRS-framed packet with an undecodable body is ignored; BYE marks departure.
        a.mesh.send(PeerId(1), &Packet::Ggrs(&[0xFF; 3]).to_vec());
        a.close();
        let mut departed = Vec::new();
        wait_for(|| {
            assert!(b.receive_all_messages().is_empty());
            departed.extend(b.take_departed());
            (!departed.is_empty()).then_some(())
        })
        .unwrap();
        assert_eq!(departed, vec![PeerId(0)]);
        assert_eq!(b.route_kind(PeerId(0)), None);
    }

    /// An in-game room link without a server: a fake relay whose `RelayOut`s the test reads,
    /// injectable signaling text, and one peer decided direct at `peer_addr`.
    struct FakeRoom {
        link: Link,
        addr: SocketAddr,
        out: Receiver<RelayOut>,
        text: Sender<String>,
        _close: Sender<(u16, String)>,
        _inbound: Sender<(PeerId, Vec<u8>)>,
    }

    fn fake_room(me: PeerId, peer: PeerId, peer_addr: SocketAddr, peer_token: u64) -> FakeRoom {
        let udp = local_sock();
        let addr = udp.local_addr().unwrap();
        let (out_tx, out) = mpsc::channel();
        let (inbound, in_rx) = mpsc::channel();
        let (text, text_rx) = mpsc::channel();
        let (close, close_rx) = mpsc::channel();
        let relay = RelayHandle { out: out_tx.clone(), inbound: in_rx };
        let mode = Mode::Room { text_rx, close_rx, ws_out: out_tx };
        let mut mesh = Mesh::base(Transport::new(udp, Some(relay)), mode, me, false, Instant::now());
        mesh.ready_pending = false;
        mesh.welcomed = true;
        mesh.token = 0xA11CE;
        let mut st = PeerState::new();
        st.token = Some(peer_token);
        st.sent_cands = true;
        st.cands = vec![peer_addr];
        mesh.peers.insert(peer, st);
        mesh.adopt_route(peer, Route::Udp(peer_addr));
        FakeRoom { link: mesh.into_link(), addr, out, text, _close: close, _inbound: inbound }
    }

    impl FakeRoom {
        /// Pretends the peer has been silent for `ms`.
        fn silent_for(&mut self, peer: PeerId, ms: u64) {
            self.link.mesh.peers.get_mut(&peer).unwrap().last_rx = Some(Instant::now() - Duration::from_millis(ms));
        }

        fn poll(&mut self) {
            use ggrs::NonBlockingSocket;
            let _ = self.link.receive_all_messages();
        }

        /// Drains the fake relay: (binary destinations, text frames).
        fn drain(&self) -> (Vec<PeerId>, Vec<String>) {
            let (mut bins, mut texts) = (Vec::new(), Vec::new());
            while let Ok(o) = self.out.try_recv() {
                match o {
                    RelayOut::Binary { to, .. } => bins.push(to),
                    RelayOut::Text(t) => texts.push(t),
                    _ => {}
                }
            }
            (bins, texts)
        }

        /// Pretends the pair fell back to the relay `ms` ago.
        fn fell_back_for(&mut self, peer: PeerId, ms: u64) {
            self.link.mesh.peers.get_mut(&peer).unwrap().fell_back_at = Some(Instant::now() - Duration::from_millis(ms));
        }

        /// The peer at `b` answers `rounds` probe rounds with token-matched PUNCH_ACKs, one
        /// `RETURN_ACK_SPACING` apart (counted acks are aged instead of sleeping). Returns the events.
        fn answer_probes(&mut self, b: &UdpSocket, peer: PeerId, rounds: usize) -> Vec<MeshEvent> {
            let ack = Packet::PunchAck { token: 0xB0B, from: peer.0, to: self.link.me().0 }.to_vec();
            let mut events = Vec::new();
            for _ in 0..rounds {
                b.send_to(&ack, self.addr).unwrap();
                thread::sleep(Duration::from_millis(30));
                events.extend(self.link.mesh.poll());
                for t in &mut self.link.mesh.peers.get_mut(&peer).unwrap().probe_acks {
                    *t -= RETURN_ACK_SPACING;
                }
            }
            events
        }
    }

    /// Reads `s` until it goes quiet; whether any packet matched `want`.
    fn recv_kind(s: &UdpSocket, want: impl Fn(&Packet<'_>) -> bool) -> bool {
        let mut buf = [0u8; 1500];
        let mut found = false;
        while let Ok(n) = s.recv(&mut buf) {
            found |= Packet::parse(&buf[..n]).is_some_and(|p| want(&p));
        }
        found
    }

    fn route_texts(texts: &[String], mode: &str) -> Vec<Value> {
        texts.iter().filter_map(|t| serde_json::from_str::<Value>(t).ok()).filter(|v| v["t"] == "route" && v["mode"] == mode).collect()
    }

    #[test]
    fn link_silent_direct_route_mirrors_then_moves_to_relay() {
        let b = local_sock();
        let mut a = fake_room(PeerId(0), PeerId(1), b.local_addr().unwrap(), 0xB0B);
        let pkt = Packet::Ggrs(b"x").to_vec();
        a.poll();
        a.link.mesh.send(PeerId(1), &pkt);
        assert_eq!(a.drain().0, vec![], "healthy route: no mirroring");

        // Silent ≥ 1 s: re-punch the route and mirror every packet over the relay.
        a.silent_for(PeerId(1), 1100);
        a.poll();
        a.link.mesh.send(PeerId(1), &pkt);
        assert_eq!(a.drain().0, vec![PeerId(1)]);
        let mut buf = [0u8; 64];
        let punched = wait_for(|| {
            let n = b.recv(&mut buf).ok()?;
            matches!(Packet::parse(&buf[..n]), Some(Packet::Punch { token: 0xA11CE, from: 0, to: 1 })).then_some(())
        });
        assert!(punched.is_some(), "silent route re-punched");

        // Traffic back on the direct address stops mirroring.
        b.send_to(&Packet::Keepalive.to_vec(), a.addr).unwrap();
        wait_for(|| {
            a.poll();
            a.link.mesh.peers[&PeerId(1)].last_rx.filter(|t| t.elapsed() < GAME_MIRROR_AFTER)
        })
        .expect("keepalive heard");
        a.drain();
        a.link.mesh.send(PeerId(1), &pkt);
        assert_eq!(a.drain().0, vec![]);
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Direct));

        // Silent ≥ 3 s: the lower id moves the pair to the relay with `route`.
        a.silent_for(PeerId(1), 3100);
        a.poll();
        let (_, texts) = a.drain();
        let route: Vec<Value> = texts.iter().filter_map(|t| serde_json::from_str(t).ok()).collect();
        assert!(route.iter().any(|v| v["t"] == "route" && v["to"] == 1 && v["mode"] == "relay"), "{texts:?}");
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Relay));
        a.link.mesh.send(PeerId(1), &pkt);
        assert_eq!(a.drain().0, vec![PeerId(1)]);
    }

    #[test]
    fn link_higher_id_mirrors_but_waits_for_route() {
        let b = local_sock();
        let mut a = fake_room(PeerId(2), PeerId(1), b.local_addr().unwrap(), 0xB0B);
        a.silent_for(PeerId(1), 3100);
        a.poll();
        a.link.mesh.send(PeerId(1), &Packet::Keepalive.to_vec());
        let (bins, texts) = a.drain();
        assert_eq!(bins, vec![PeerId(1)]);
        assert!(texts.is_empty(), "only the lower id decides: {texts:?}");
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Direct));
        a.text.send(json!({"t": "route", "from": 1, "mode": "relay"}).to_string()).unwrap();
        a.poll();
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Relay));
    }

    #[test]
    fn link_roams_to_new_address_on_punch_with_token() {
        use ggrs::NonBlockingSocket;
        let b_old = local_sock();
        let mut a = fake_room(PeerId(0), PeerId(1), b_old.local_addr().unwrap(), 0xB0B);
        let b_new = local_sock();
        let b_new_addr = b_new.local_addr().unwrap();
        a.silent_for(PeerId(1), 1100);

        // A wrong token never moves the route.
        let stranger = local_sock();
        stranger.send_to(&Packet::Punch { token: 0xBAD, from: 1, to: 0 }.to_vec(), a.addr).unwrap();
        thread::sleep(Duration::from_millis(30));
        a.poll();
        assert_eq!(a.link.mesh.routes[&PeerId(1)], Route::Udp(b_old.local_addr().unwrap()));

        // B's PUNCH with its token from the new socket moves the route and is answered.
        b_new.send_to(&Packet::Punch { token: 0xB0B, from: 1, to: 0 }.to_vec(), a.addr).unwrap();
        wait_for(|| {
            a.poll();
            (a.link.mesh.routes[&PeerId(1)] == Route::Udp(b_new_addr)).then_some(())
        })
        .expect("roamed");
        assert_eq!(a.link.mesh.transport.peer_at(b_new_addr), Some(PeerId(1)));
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Direct));
        let mut buf = [0u8; 1500];
        let n = b_new.recv(&mut buf).unwrap();
        assert!(matches!(Packet::parse(&buf[..n]), Some(Packet::PunchAck { token: 0xA11CE, from: 0, to: 1 })));

        // GGRS flows both ways on the new address.
        a.link.send_to(&ggrs_msg(), &PeerId(1));
        let got = wait_for(|| {
            let n = b_new.recv(&mut buf).ok()?;
            matches!(Packet::parse(&buf[..n]), Some(Packet::Ggrs(_))).then_some(())
        });
        assert!(got.is_some(), "ggrs packet reached the new address");
        let body = bincode::serialize(&ggrs_msg()).unwrap();
        b_new.send_to(&Packet::Ggrs(&body).to_vec(), a.addr).unwrap();
        let got = wait_for(|| a.link.receive_all_messages().pop()).unwrap();
        assert_eq!(got, (PeerId(1), ggrs_msg()));
    }

    #[test]
    fn link_relay_pair_does_not_roam() {
        let b_old = local_sock();
        let mut a = fake_room(PeerId(0), PeerId(1), b_old.local_addr().unwrap(), 0xB0B);
        a.link.mesh.switch_to_relay(PeerId(1));
        a.silent_for(PeerId(1), 5000);
        let b_new = local_sock();
        b_new.send_to(&Packet::Punch { token: 0xB0B, from: 1, to: 0 }.to_vec(), a.addr).unwrap();
        thread::sleep(Duration::from_millis(30));
        a.poll();
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Relay));
    }

    #[test]
    fn link_fallen_back_pair_returns_to_direct() {
        let b = local_sock();
        let b_addr = b.local_addr().unwrap();
        let mut a = fake_room(PeerId(0), PeerId(1), b_addr, 0xB0B);
        a.silent_for(PeerId(1), 3100);
        a.poll();
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Relay));
        a.drain();

        // After the wait the old direct address is probed with the session token.
        a.fell_back_for(PeerId(1), 5100);
        a.poll();
        assert!(recv_kind(&b, |p| matches!(p, Packet::Punch { token: 0xA11CE, from: 0, to: 1 })), "probed");

        // Steady acks from one address: the lower id announces direct and switches.
        let events = a.answer_probes(&b, PeerId(1), RETURN_ACKS);
        assert!(
            events.iter().any(|e| matches!(e, MeshEvent::RouteChanged { id: PeerId(1), kind: RouteKind::Direct })),
            "{events:?}"
        );
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Direct));
        let (_, texts) = a.drain();
        let direct = route_texts(&texts, "direct");
        assert!(direct.iter().any(|v| v["to"] == 1 && v["addr"] == b_addr.to_string()), "{texts:?}");

        // A packet sent mid-switch (peer not heard on direct yet) goes by UDP and the relay.
        let pkt = Packet::Ggrs(b"x").to_vec();
        a.link.mesh.send(PeerId(1), &pkt);
        assert_eq!(a.drain().0, vec![PeerId(1)], "mirrored until the peer switched");
        assert!(recv_kind(&b, |p| matches!(p, Packet::Ggrs(_))), "sent by UDP");

        // Once the peer is heard on the direct route, packets leave by UDP only.
        b.send_to(&Packet::Keepalive.to_vec(), a.addr).unwrap();
        wait_for(|| {
            a.poll();
            (!a.link.mesh.peers[&PeerId(1)].returning).then_some(())
        })
        .expect("peer heard on the direct route");
        a.drain();
        a.link.mesh.send(PeerId(1), &pkt);
        assert_eq!(a.drain().0, vec![]);
        assert!(recv_kind(&b, |p| matches!(p, Packet::Ggrs(_))), "sent by UDP");
    }

    /// The higher id follows the lower id's `route direct` to where the peer's probes come
    /// from, and a packet around the switch is accepted on either route.
    #[test]
    fn link_higher_id_returns_on_route_direct_and_packets_arrive_on_either_route() {
        let b = local_sock();
        let mut a = fake_room(PeerId(2), PeerId(1), b.local_addr().unwrap(), 0xB0B);
        a.text.send(json!({"t": "route", "from": 1, "mode": "relay"}).to_string()).unwrap();
        a.poll();
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Relay));

        // The peer moved: its token-matched probe from a new socket is answered and remembered.
        let b_new = local_sock();
        let b_new_addr = b_new.local_addr().unwrap();
        b_new.send_to(&Packet::Punch { token: 0xB0B, from: 1, to: 2 }.to_vec(), a.addr).unwrap();
        thread::sleep(Duration::from_millis(30));
        a.poll();
        assert!(recv_kind(&b_new, |p| matches!(p, Packet::PunchAck { token: 0xA11CE, from: 2, to: 1 })));
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Relay), "a probe alone never switches");

        a.text.send(json!({"t": "route", "from": 1, "mode": "direct", "addr": "127.0.0.1:9"}).to_string()).unwrap();
        thread::sleep(Duration::from_millis(30));
        let events = a.link.mesh.poll();
        assert!(events.iter().any(|e| matches!(e, MeshEvent::RouteChanged { id: PeerId(1), kind: RouteKind::Direct })));
        assert_eq!(a.link.mesh.routes[&PeerId(1)], Route::Udp(b_new_addr));

        // The decider's packets mirrored over the relay and its UDP packets both arrive.
        a._inbound.send((PeerId(1), Packet::Ggrs(b"r").to_vec())).unwrap();
        b_new.send_to(&Packet::Ggrs(b"u").to_vec(), a.addr).unwrap();
        thread::sleep(Duration::from_millis(30));
        let mut got = Vec::new();
        for _ in 0..3 {
            for ev in a.link.mesh.poll() {
                if let MeshEvent::Packet { from, data } = ev {
                    got.push((from, data));
                }
            }
        }
        assert!(got.contains(&(PeerId(1), Packet::Ggrs(b"r").to_vec())), "relay copy accepted");
        assert!(got.contains(&(PeerId(1), Packet::Ggrs(b"u").to_vec())), "UDP copy accepted");
    }

    /// A pair relayed by decision (relay_only, force_relay, punching never worked) never
    /// probes and never leaves the relay, whatever arrives.
    #[test]
    fn link_relay_by_decision_pair_never_probes_or_returns() {
        let b = local_sock();
        let mut a = fake_room(PeerId(0), PeerId(1), b.local_addr().unwrap(), 0xB0B);
        a.link.mesh.peers.get_mut(&PeerId(1)).unwrap().relay_only = true;
        a.link.mesh.adopt_route(PeerId(1), Route::Relay);
        for _ in 0..=RETURN_ACKS {
            b.send_to(&Packet::Punch { token: 0xB0B, from: 1, to: 0 }.to_vec(), a.addr).unwrap();
            b.send_to(&Packet::PunchAck { token: 0xB0B, from: 1, to: 0 }.to_vec(), a.addr).unwrap();
            thread::sleep(Duration::from_millis(30));
            a.poll();
        }
        assert_eq!(a.link.route_kind(PeerId(1)), Some(RouteKind::Relay));
        let st = &a.link.mesh.peers[&PeerId(1)];
        assert!(st.fell_back_at.is_none() && st.probe_acks.is_empty());
        assert!(route_texts(&a.drain().1, "direct").is_empty());
        assert!(!recv_kind(&b, |p| matches!(p, Packet::Punch { .. })), "never probed");
    }

    #[test]
    fn link_return_waits_and_backs_off() {
        let s = Duration::from_secs;
        assert_eq!([1, 2, 3, 4, 5, 30].map(return_wait), [s(5), s(10), s(20), s(40), s(60), s(60)]);
        let b = local_sock();
        let mut a = fake_room(PeerId(0), PeerId(1), b.local_addr().unwrap(), 0xB0B);
        let p = PeerId(1);
        a.silent_for(p, 3100);
        a.poll();
        assert_eq!(a.link.route_kind(p), Some(RouteKind::Relay));

        // Inside the first wait: no probes, acks are not counted, no return.
        a.fell_back_for(p, 4000);
        a.answer_probes(&b, p, RETURN_ACKS + 1);
        assert_eq!(a.link.route_kind(p), Some(RouteKind::Relay));
        assert!(a.link.mesh.peers[&p].probe_acks.is_empty());
        assert!(!recv_kind(&b, |q| matches!(q, Packet::Punch { .. })), "no probe inside the wait");
        a.fell_back_for(p, 5100);
        a.answer_probes(&b, p, RETURN_ACKS);
        assert_eq!(a.link.route_kind(p), Some(RouteKind::Direct));

        // A second fallback soon after waits twice as long.
        a.silent_for(p, 3100);
        a.poll();
        assert_eq!(a.link.route_kind(p), Some(RouteKind::Relay));
        assert_eq!(a.link.mesh.peers[&p].fallbacks, 2);
        a.fell_back_for(p, 5100);
        a.answer_probes(&b, p, RETURN_ACKS + 1);
        assert_eq!(a.link.route_kind(p), Some(RouteKind::Relay), "second wait is 10 s");
        a.fell_back_for(p, 10_100);
        a.answer_probes(&b, p, RETURN_ACKS);
        assert_eq!(a.link.route_kind(p), Some(RouteKind::Direct));

        // A direct route that held `RETURN_STABLE_AFTER` resets the backoff.
        a.link.mesh.peers.get_mut(&p).unwrap().direct_since = Some(Instant::now() - RETURN_STABLE_AFTER - s(1));
        a.silent_for(p, 3100);
        a.poll();
        assert_eq!(a.link.mesh.peers[&p].fallbacks, 1);
    }
}
