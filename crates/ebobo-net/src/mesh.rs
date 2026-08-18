//! Rendezvous and routes: signaling WebSocket, candidate exchange, hole punching, route
//! decisions, relay fallback, direct-IP mode (`docs/PROTOCOL.md` sections 1-2). Owner: mesh
//! slice.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::packet::Packet;
use crate::stun;
use crate::transport::{PeerId, RelayHandle, RelayOut, Route, RouteKind, Source, Transport};
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
    /// A decided pair moved to another route (a dead direct route fell back to the relay).
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
/// A direct route silent this long gets punched again...
const ROUTE_REPUNCH_AFTER: Duration = Duration::from_secs(3);
/// ...and silent this long moves to the relay (decided by the lower id, sent as `route`).
const ROUTE_DEAD_AFTER: Duration = Duration::from_secs(5);
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
        }
    }
}

enum Mode {
    Room {
        text_rx: Receiver<String>,
        close_rx: Receiver<(u16, String)>,
        ws_out: Sender<RelayOut>,
    },
    HostDirect,
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
}

impl Mesh {
    /// Binds the UDP socket, runs STUN, connects the WebSocket (room modes). Blocking, at
    /// most ~6 s. Server refusals map to `NetError::{RoomTaken, NoRoom, RoomFull, RoomLocked}`.
    pub(crate) fn open(cfg: MeshConfig) -> Result<Mesh, NetError> {
        let bind_port = match cfg.role {
            MeshRole::HostDirect { port } => port,
            _ => 0,
        };
        let udp = UdpSocket::bind(("0.0.0.0", bind_port)).map_err(|e| NetError::Io(e.to_string()))?;
        let now = Instant::now();
        let base = |transport: Transport, mode: Mode, me: PeerId| Mesh {
            transport,
            mode,
            me,
            code: None,
            token: 0,
            my_cands: Vec::new(),
            symmetric: false,
            force_relay: cfg.force_relay,
            peers: HashMap::new(),
            routes: HashMap::new(),
            events: Vec::new(),
            ready_pending: true,
            failed: false,
            last_keepalive: now,
            welcomed: false,
        };
        match &cfg.role {
            MeshRole::HostDirect { .. } => {
                let transport = Transport::new(udp, PeerId(0), None);
                Ok(base(transport, Mode::HostDirect, PeerId(0)))
            }
            MeshRole::JoinDirect { addr } => {
                let target = resolve_direct(addr)?;
                let transport = Transport::new(udp, PeerId(1), None);
                let mode = Mode::JoinDirect { target, deadline: now + DIRECT_TIMEOUT, last_punch: None };
                Ok(base(transport, mode, PeerId(1)))
            }
            MeshRole::HostRoom { .. } | MeshRole::JoinRoom { .. } => {
                let port = udp.local_addr().map_err(|e| NetError::Io(e.to_string()))?.port();
                let stun = stun::query(&udp, stun::STUN_SERVERS, STUN_TIMEOUT);
                let mut my_cands: Vec<SocketAddr> = Vec::new();
                for ip in stun::local_ipv4() {
                    let a = SocketAddr::new(ip.into(), port);
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
                let transport = Transport::new(udp, PeerId(0), Some(relay));
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
            Mode::HostDirect => {}
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

    /// Sends a packet to a peer with a decided route.
    pub(crate) fn send(&mut self, to: PeerId, packet: &[u8]) {
        self.transport.send(to, packet);
    }

    pub(crate) fn me(&self) -> PeerId {
        self.me
    }

    /// Host: `{"t":"lock"}` to the server so nobody joins a running game.
    pub(crate) fn lock_room(&mut self) {
        self.transport.send_text(json!({"t": "lock"}).to_string());
    }

    /// Hands routes (and the live relay WebSocket) to the game.
    pub(crate) fn into_transport(self) -> Transport {
        self.transport
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
                self.transport.set_me(me);
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
                    .map(|a| a.iter().filter_map(|c| c.as_str()?.parse().ok()).collect())
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
                    // The only later change: a dead direct route falls back to the relay.
                    if v.get("mode").and_then(Value::as_str) == Some("relay")
                        && matches!(self.routes.get(&from), Some(Route::Udp(_)))
                    {
                        self.switch_to_relay(from);
                    }
                    return;
                }
                let route = match v.get("mode").and_then(Value::as_str) {
                    Some("direct") => {
                        let given = v.get("addr").and_then(Value::as_str).and_then(|s| s.parse().ok());
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
        if self.routes.remove(&id).is_some() {
            self.transport.remove_route(id);
        }
        self.events.push(MeshEvent::PeerLeft { id });
    }

    fn switch_to_relay(&mut self, peer: PeerId) {
        self.transport.set_route(peer, Route::Relay);
        self.routes.insert(peer, Route::Relay);
        self.events.push(MeshEvent::RouteChanged { id: peer, kind: RouteKind::Relay });
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
            st.decided = true;
            st.last_rx = Some(Instant::now());
        }
        self.transport.set_route(peer, route);
        self.routes.insert(peer, route);
        let kind = match route {
            Route::Udp(_) => RouteKind::Direct,
            Route::Relay => RouteKind::Relay,
        };
        self.events.push(MeshEvent::RouteUp { id: peer, kind });
    }

    /// Punches undecided peers and, as the lower id of a pair, decides the route.
    fn drive_pairs(&mut self, now: Instant) {
        let me = self.me;
        let relay_after = if self.symmetric { RELAY_AFTER_SYMMETRIC } else { RELAY_AFTER };
        let mut decisions: Vec<(PeerId, Route)> = Vec::new();
        let mut dead: Vec<PeerId> = Vec::new();
        let ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in ids {
            let Some(st) = self.peers.get_mut(&peer) else { continue };
            if st.decided {
                // Route health: a silent direct route is punched again, then given up.
                let Some(&Route::Udp(addr)) = self.routes.get(&peer) else { continue };
                let silent = st.last_rx.map_or(Duration::ZERO, |t| now - t);
                if silent >= ROUTE_DEAD_AFTER && me < peer {
                    dead.push(peer);
                } else if silent >= ROUTE_REPUNCH_AFTER && st.last_punch.is_none_or(|t| now - t >= PUNCH_EVERY) {
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
    }

    // ---- packets ----

    fn poll_packets(&mut self) {
        while let Some((src, data)) = self.transport.recv_raw() {
            if let Some(p) = self.routed_source(src).filter(|_| matches!(src, Source::Udp(_))) {
                if let Some(st) = self.peers.get_mut(&p) {
                    st.last_rx = Some(Instant::now());
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
            Source::Udp(addr) => self
                .routes
                .iter()
                .find(|(_, r)| **r == Route::Udp(addr))
                .map(|(&p, _)| p),
        }
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
            Mode::JoinDirect { .. } => {}
            Mode::Room { .. } => {
                let Some(st) = self.peers.get_mut(&from) else { return };
                if st.token != Some(token) {
                    return;
                }
                st.seen_from.get_or_insert(addr);
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
            Mode::HostDirect => {}
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
                let Some(st) = self.peers.get_mut(&from) else { return };
                if st.token != Some(token) {
                    return;
                }
                st.confirmed.get_or_insert(addr);
            }
        }
    }
}

/// `host[:port]` → socket address, default port 7777, IPv4 preferred.
fn resolve_direct(addr: &str) -> Result<SocketAddr, NetError> {
    let addr = addr.trim();
    let with_port = if addr.parse::<SocketAddr>().is_ok() || addr.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
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
        let peer = nodes[1].me.unwrap();
        let dead = Route::Udp(SocketAddr::from(([127, 0, 0, 1], 9)));
        nodes[0].mesh.transport.set_route(peer, dead);
        nodes[0].mesh.routes.insert(peer, dead);
        let started = Instant::now();
        pump_until(&mut nodes, 15, "relay fallback", |ns| {
            ns.iter().all(|n| n.routes.values().all(|k| *k == RouteKind::Relay))
        });
        let took = started.elapsed();
        assert!(took >= Duration::from_secs(4), "switched too early: {took:?}");
        exchange_until(&mut nodes, 10);
        nodes.into_iter().for_each(|n| n.mesh.close());
    }
}
