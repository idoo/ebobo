//! Sans-IO TURN client (RFC 8656 over UDP, STUN message format RFC 8489): the UDP relay
//! fallback through Cloudflare Realtime TURN with short-lived credentials. Owner: turn slice.
//!
//! The caller owns the socket: it sends everything [`TurnClient::poll`], [`TurnClient::wrap`]
//! and [`TurnClient::close`] return to [`TurnClient::server`], feeds every datagram received
//! from the server to [`TurnClient::handle`] and sleeps until [`TurnClient::next_timeout`].

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

const MAGIC_COOKIE: u32 = 0x2112_A442;
const FINGERPRINT_XOR: u32 = 0x5354_554E;
const HEADER_LEN: usize = 20;

const METHOD_ALLOCATE: u16 = 0x003;
const METHOD_REFRESH: u16 = 0x004;
const METHOD_SEND: u16 = 0x006;
const METHOD_DATA: u16 = 0x007;
const METHOD_CREATE_PERMISSION: u16 = 0x008;
const METHOD_CHANNEL_BIND: u16 = 0x009;

const CLASS_REQUEST: u16 = 0x0000;
const CLASS_INDICATION: u16 = 0x0010;
const CLASS_SUCCESS: u16 = 0x0100;
const CLASS_ERROR: u16 = 0x0110;

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_FINGERPRINT: u16 = 0x8028;

const PROTO_UDP: u8 = 17;
const DEFAULT_PORT: u16 = 3478;
const INITIAL_RTO: Duration = Duration::from_millis(250);
const MAX_TRANSMISSIONS: u32 = 6;
/// Wait after the last transmission before giving up: Rm (16) × initial RTO (RFC 8489 §6.2.1).
const FINAL_WAIT: Duration = Duration::from_millis(250 * 16);
/// Allocation lifetime assumed when the server's answer carries no LIFETIME (RFC 8656 default).
const DEFAULT_LIFETIME: u32 = 600;
const PERMISSION_REFRESH: Duration = Duration::from_secs(240);
const CHANNEL_REFRESH: Duration = Duration::from_secs(540);
const FIRST_CHANNEL: u16 = 0x4000;
const LAST_CHANNEL: u16 = 0x4FFF;
/// Consecutive 438 Stale Nonce answers tolerated before giving up.
const MAX_STALE_NONCE: u32 = 4;

/// Where the allocation stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnState {
    /// Allocate sent (or about to be), no answer yet.
    Allocating,
    /// The server relays for us from `relayed`; it sees us at `mapped`.
    Ready { relayed: SocketAddr, mapped: SocketAddr },
    /// Error code/reason, "timeout", or "closed". Terminal.
    Failed(String),
}

/// When a periodic request is due next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    /// On the next `poll`.
    Now,
    At(Instant),
    /// A request is outstanding; its answer schedules the next one.
    InFlight,
}

impl Due {
    fn at(self, now: Instant) -> Option<Instant> {
        match self {
            Due::Now => Some(now),
            Due::At(t) => Some(t),
            Due::InFlight => None,
        }
    }

    fn ready(self, now: Instant) -> bool {
        self.at(now).is_some_and(|t| t <= now)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Allocate,
    /// Refresh asking for this LIFETIME.
    Refresh(u32),
    Permission(IpAddr),
    /// ChannelBind of a channel number to a peer.
    Bind(u16, SocketAddr),
}

impl Kind {
    fn method(self) -> u16 {
        match self {
            Kind::Allocate => METHOD_ALLOCATE,
            Kind::Refresh(_) => METHOD_REFRESH,
            Kind::Permission(_) => METHOD_CREATE_PERMISSION,
            Kind::Bind(..) => METHOD_CHANNEL_BIND,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Allocate => "allocate",
            Kind::Refresh(_) => "refresh",
            Kind::Permission(_) => "create-permission",
            Kind::Bind(..) => "channel-bind",
        }
    }
}

/// An outstanding request and its retransmission schedule.
struct Pending {
    txid: [u8; 12],
    kind: Kind,
    msg: Vec<u8>,
    sends: u32,
    rto: Duration,
    next: Instant,
}

struct Channel {
    number: u16,
    bound: bool,
    due: Due,
}

/// One TURN allocation on one server, driven by the caller's UDP socket.
pub struct TurnClient {
    server: SocketAddr,
    username: String,
    password: String,
    state: TurnState,
    /// Latest instant passed to `new`/`poll`/`handle`; work due "now" is reported at it.
    now: Instant,
    realm: String,
    nonce: String,
    /// Long-term credential key MD5(username:realm:password), once the server named its realm.
    key: Option<[u8; 16]>,
    pending: Vec<Pending>,
    /// Requests to send on the next `poll`: the first Allocate and 401/438 retries.
    queued: Vec<Kind>,
    stale_nonces: u32,
    lifetime: u32,
    /// Allocation Refresh timer (InFlight until Ready).
    allocation: Due,
    permissions: HashMap<IpAddr, Due>,
    channels: HashMap<SocketAddr, Channel>,
    /// Peer of channel `FIRST_CHANNEL + index`.
    channel_peers: Vec<SocketAddr>,
    /// Transaction id counter for Send indications (no answer is matched to them).
    indications: u64,
}

impl TurnClient {
    /// `server` = resolved TURN server address (IPv4), `username`/`password` = the short-lived credential.
    pub fn new(server: SocketAddr, username: &str, password: &str, now: Instant) -> TurnClient {
        TurnClient {
            server,
            username: username.to_owned(),
            password: password.to_owned(),
            state: TurnState::Allocating,
            now,
            realm: String::new(),
            nonce: String::new(),
            key: None,
            pending: Vec::new(),
            queued: vec![Kind::Allocate],
            stale_nonces: 0,
            lifetime: DEFAULT_LIFETIME,
            allocation: Due::InFlight,
            permissions: HashMap::new(),
            channels: HashMap::new(),
            channel_peers: Vec::new(),
            indications: 0,
        }
    }

    /// The TURN server every datagram of this client goes to and comes from.
    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// Current allocation state.
    pub fn state(&self) -> &TurnState {
        &self.state
    }

    fn ready(&self) -> bool {
        matches!(self.state, TurnState::Ready { .. })
    }

    fn failed(&self) -> bool {
        matches!(self.state, TurnState::Failed(_))
    }

    fn fail(&mut self, reason: String) {
        self.state = TurnState::Failed(reason);
        self.pending.clear();
        self.queued.clear();
    }

    /// Datagrams to send to `server()` now: first Allocate, retransmits, Refresh, permission and channel refreshes.
    pub fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        self.now = now;
        let mut out = Vec::new();
        if self.failed() {
            return out;
        }
        for p in &mut self.pending {
            if p.next > now {
                continue;
            }
            if p.sends >= MAX_TRANSMISSIONS {
                let reason = match p.kind {
                    Kind::Allocate => "timeout".to_owned(),
                    kind => format!("{} timeout", kind.name()),
                };
                self.fail(reason);
                return Vec::new();
            }
            out.push(p.msg.clone());
            p.sends += 1;
            p.next = now + if p.sends == MAX_TRANSMISSIONS { FINAL_WAIT } else { p.rto };
            p.rto *= 2;
        }
        for kind in std::mem::take(&mut self.queued) {
            out.push(self.send_request(now, kind));
        }
        if !self.ready() {
            return out;
        }
        if self.allocation.ready(now) {
            self.allocation = Due::InFlight;
            out.push(self.send_request(now, Kind::Refresh(self.lifetime)));
        }
        let due_ips: Vec<IpAddr> =
            self.permissions.iter().filter(|(_, d)| d.ready(now)).map(|(ip, _)| *ip).collect();
        for ip in due_ips {
            self.permissions.insert(ip, Due::InFlight);
            out.push(self.send_request(now, Kind::Permission(ip)));
        }
        let due_binds: Vec<(u16, SocketAddr)> = self
            .channels
            .iter_mut()
            .filter(|(_, c)| c.due.ready(now))
            .map(|(peer, c)| {
                c.due = Due::InFlight;
                (c.number, *peer)
            })
            .collect();
        for (number, peer) in due_binds {
            out.push(self.send_request(now, Kind::Bind(number, peer)));
        }
        out
    }

    /// Earliest instant `poll` has something to do (for sleeps); None when Failed/closed.
    pub fn next_timeout(&self) -> Option<Instant> {
        if self.failed() {
            return None;
        }
        let now = self.now;
        let mut next = self.pending.iter().map(|p| p.next).min();
        let mut take = |t: Option<Instant>| {
            if let Some(t) = t {
                next = Some(next.map_or(t, |n| n.min(t)));
            }
        };
        if !self.queued.is_empty() {
            take(Some(now));
        }
        if self.ready() {
            take(self.allocation.at(now));
            self.permissions.values().for_each(|d| take(d.at(now)));
            self.channels.values().for_each(|c| take(c.due.at(now)));
        }
        next
    }

    /// One datagram received from `server()`. Consumes STUN responses; returns peer data carried by a Data indication or ChannelData as (peer address, payload).
    pub fn handle(&mut self, now: Instant, datagram: &[u8]) -> Option<(SocketAddr, Vec<u8>)> {
        self.now = now;
        if self.failed() {
            return None;
        }
        if let Some((number, payload)) = decode_channel_data(datagram) {
            let index = usize::from(number.checked_sub(FIRST_CHANNEL)?);
            let peer = *self.channel_peers.get(index)?;
            return Some((peer, payload.to_vec()));
        }
        let msg = Message::parse(datagram)?;
        match msg.class {
            CLASS_INDICATION if msg.method == METHOD_DATA => {
                let peer = msg.xor_addr(ATTR_XOR_PEER_ADDRESS)?;
                if !self.permissions.contains_key(&peer.ip()) {
                    return None;
                }
                Some((peer, msg.attr(ATTR_DATA)?.to_vec()))
            }
            CLASS_SUCCESS | CLASS_ERROR => {
                self.on_response(now, &msg);
                None
            }
            _ => None,
        }
    }

    /// Allow `peer` (CreatePermission for its IP, then ChannelBind a channel to it). Idempotent. Before Ready it is queued.
    /// At most 4096 peers per allocation (the channel number range); later ones are ignored.
    pub fn add_peer(&mut self, peer: SocketAddr) {
        if self.channels.contains_key(&peer) || self.channel_peers.len() > usize::from(LAST_CHANNEL - FIRST_CHANNEL) {
            return;
        }
        self.permissions.entry(peer.ip()).or_insert(Due::Now);
        let number = FIRST_CHANNEL + self.channel_peers.len() as u16;
        self.channel_peers.push(peer);
        self.channels.insert(peer, Channel { number, bound: false, due: Due::Now });
    }

    /// `data` framed for `peer` via the server: ChannelData once the channel is bound, else a Send indication. None before Ready or if `add_peer` was never called for it.
    pub fn wrap(&mut self, peer: SocketAddr, data: &[u8]) -> Option<Vec<u8>> {
        if !self.ready() {
            return None;
        }
        let channel = self.channels.get(&peer)?;
        if channel.bound {
            return Some(encode_channel_data(channel.number, data));
        }
        self.indications += 1;
        let mut txid = [0u8; 12];
        txid[4..].copy_from_slice(&self.indications.to_be_bytes());
        let mut b = Builder::new(METHOD_SEND, CLASS_INDICATION, &txid);
        b.xor_addr(ATTR_XOR_PEER_ADDRESS, peer);
        b.attr(ATTR_DATA, data);
        Some(b.buf)
    }

    /// Refresh with LIFETIME 0 (deallocate), best effort; the client is Failed("closed") afterwards.
    pub fn close(&mut self) -> Option<Vec<u8>> {
        let msg = (self.ready() && self.key.is_some()).then(|| self.encode_request(Kind::Refresh(0), &random_txid()));
        self.fail("closed".to_owned());
        msg
    }

    fn send_request(&mut self, now: Instant, kind: Kind) -> Vec<u8> {
        let txid = random_txid();
        let msg = self.encode_request(kind, &txid);
        self.pending.push(Pending { txid, kind, msg: msg.clone(), sends: 1, rto: INITIAL_RTO * 2, next: now + INITIAL_RTO });
        msg
    }

    fn encode_request(&self, kind: Kind, txid: &[u8; 12]) -> Vec<u8> {
        let mut b = Builder::new(kind.method(), CLASS_REQUEST, txid);
        match kind {
            Kind::Allocate => b.attr(ATTR_REQUESTED_TRANSPORT, &[PROTO_UDP, 0, 0, 0]),
            Kind::Refresh(lifetime) => b.attr(ATTR_LIFETIME, &lifetime.to_be_bytes()),
            Kind::Permission(ip) => b.xor_addr(ATTR_XOR_PEER_ADDRESS, SocketAddr::new(ip, 0)),
            Kind::Bind(number, peer) => {
                let [hi, lo] = number.to_be_bytes();
                b.attr(ATTR_CHANNEL_NUMBER, &[hi, lo, 0, 0]);
                b.xor_addr(ATTR_XOR_PEER_ADDRESS, peer);
            }
        }
        if let Some(key) = &self.key {
            b.attr(ATTR_USERNAME, self.username.as_bytes());
            b.attr(ATTR_REALM, self.realm.as_bytes());
            b.attr(ATTR_NONCE, self.nonce.as_bytes());
            b.integrity(key);
        }
        b.fingerprint();
        b.buf
    }

    fn set_realm(&mut self, realm: &str) {
        self.realm = realm.to_owned();
        self.key = Some(long_term_key(&self.username, realm, &self.password));
    }

    fn on_response(&mut self, now: Instant, msg: &Message) {
        let Some(index) = self.pending.iter().position(|p| p.txid == msg.txid) else {
            return;
        };
        if let Some(key) = &self.key {
            if msg.integrity_ok(key) == Some(false) {
                return;
            }
        }
        let kind = self.pending.swap_remove(index).kind;
        if msg.class == CLASS_SUCCESS {
            self.stale_nonces = 0;
            self.on_success(now, kind, msg);
            return;
        }
        let (code, reason) = msg.error().unwrap_or((0, ""));
        match code {
            401 if kind == Kind::Allocate && self.key.is_none() => {
                match (msg.text(ATTR_REALM), msg.text(ATTR_NONCE)) {
                    (Some(realm), Some(nonce)) => {
                        self.nonce = nonce.to_owned();
                        self.set_realm(realm);
                        self.queued.push(kind);
                    }
                    _ => self.fail("allocate error 401 without realm/nonce".to_owned()),
                }
            }
            438 if self.stale_nonces < MAX_STALE_NONCE && self.key.is_some() => match msg.text(ATTR_NONCE) {
                Some(nonce) => {
                    self.stale_nonces += 1;
                    self.nonce = nonce.to_owned();
                    if let Some(realm) = msg.text(ATTR_REALM) {
                        self.set_realm(realm);
                    }
                    self.queued.push(kind);
                }
                None => self.fail(format!("{} error 438 without nonce", kind.name())),
            },
            _ => self.fail(format!("{} error {code} {reason}", kind.name()).trim_end().to_owned()),
        }
    }

    fn on_success(&mut self, now: Instant, kind: Kind, msg: &Message) {
        let lifetime = || msg.attr(ATTR_LIFETIME).and_then(|v| v.try_into().ok()).map(u32::from_be_bytes);
        match kind {
            Kind::Allocate => {
                let (Some(relayed), Some(mapped)) =
                    (msg.xor_addr(ATTR_XOR_RELAYED_ADDRESS), msg.xor_addr(ATTR_XOR_MAPPED_ADDRESS))
                else {
                    self.fail("allocate answer without addresses".to_owned());
                    return;
                };
                self.lifetime = lifetime().unwrap_or(DEFAULT_LIFETIME);
                self.allocation = Due::At(now + half(self.lifetime));
                self.state = TurnState::Ready { relayed, mapped };
            }
            Kind::Refresh(asked) => {
                self.lifetime = lifetime().unwrap_or(asked);
                self.allocation = Due::At(now + half(self.lifetime));
            }
            Kind::Permission(ip) => {
                if let Some(due) = self.permissions.get_mut(&ip) {
                    *due = Due::At(now + PERMISSION_REFRESH);
                }
            }
            Kind::Bind(_, peer) => {
                if let Some(channel) = self.channels.get_mut(&peer) {
                    channel.bound = true;
                    channel.due = Due::At(now + CHANNEL_REFRESH);
                }
            }
        }
    }
}

fn half(lifetime: u32) -> Duration {
    Duration::from_secs(u64::from(lifetime.max(2)) / 2)
}

fn random_txid() -> [u8; 12] {
    let mut txid = [0u8; 12];
    getrandom::fill(&mut txid).expect("OS random source");
    txid
}

/// Long-term credential key: MD5(username ":" realm ":" password) (RFC 8489 §9.2.2, no SASLprep).
fn long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let mut md5 = Md5::new();
    md5.update(username.as_bytes());
    md5.update(b":");
    md5.update(realm.as_bytes());
    md5.update(b":");
    md5.update(password.as_bytes());
    md5.finalize().into()
}

/// HMAC-SHA1 over `prefix` (a message up to where MESSAGE-INTEGRITY goes) with the header
/// length counting the MESSAGE-INTEGRITY attribute.
fn integrity_hmac(prefix: &[u8], key: &[u8]) -> [u8; 20] {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    let len = (prefix.len() - HEADER_LEN + 24) as u16;
    mac.update(&prefix[..2]);
    mac.update(&len.to_be_bytes());
    mac.update(&prefix[4..]);
    mac.finalize().into_bytes().into()
}

/// FINGERPRINT value over `prefix` (a message up to where FINGERPRINT goes).
fn fingerprint_crc(prefix: &[u8]) -> u32 {
    let mut crc = crc32fast::Hasher::new();
    let len = (prefix.len() - HEADER_LEN + 8) as u16;
    crc.update(&prefix[..2]);
    crc.update(&len.to_be_bytes());
    crc.update(&prefix[4..]);
    crc.finalize() ^ FINGERPRINT_XOR
}

fn cookie_and_txid(txid: &[u8; 12]) -> [u8; 16] {
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(txid);
    mask
}

/// XOR-*-ADDRESS value for `addr` into `out`; returns its length.
fn encode_xor_addr(addr: SocketAddr, txid: &[u8; 12], out: &mut [u8; 20]) -> usize {
    let mask = cookie_and_txid(txid);
    out[2..4].copy_from_slice(&(addr.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
    let mut octets = [0u8; 16];
    let len = match addr.ip() {
        IpAddr::V4(ip) => {
            out[1] = 1;
            octets[..4].copy_from_slice(&ip.octets());
            4
        }
        IpAddr::V6(ip) => {
            out[1] = 2;
            octets = ip.octets();
            16
        }
    };
    for i in 0..len {
        out[4 + i] = octets[i] ^ mask[i];
    }
    4 + len
}

fn decode_xor_addr(value: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    let mask = cookie_and_txid(txid);
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]) ^ (MAGIC_COOKIE >> 16) as u16;
    let ip = match value[1] {
        1 => {
            let b: [u8; 4] = value.get(4..8)?.try_into().ok()?;
            IpAddr::V4(Ipv4Addr::from(std::array::from_fn::<u8, 4, _>(|i| b[i] ^ mask[i])))
        }
        2 => {
            let b: [u8; 16] = value.get(4..20)?.try_into().ok()?;
            IpAddr::V6(Ipv6Addr::from(std::array::from_fn::<u8, 16, _>(|i| b[i] ^ mask[i])))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// ChannelData frame: channel number, payload length, payload (unpadded; fine over UDP).
fn encode_channel_data(number: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&number.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// Channel number and payload of a ChannelData frame; trailing padding is ignored.
fn decode_channel_data(buf: &[u8]) -> Option<(u16, &[u8])> {
    if buf.len() < 4 || buf[0] & 0xC0 != 0x40 {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
    Some((u16::from_be_bytes([buf[0], buf[1]]), buf.get(4..4 + len)?))
}

fn msg_type(method: u16, class: u16) -> u16 {
    (method & 0x000F) | ((method & 0x0070) << 1) | ((method & 0x0F80) << 2) | class
}

/// STUN message encoder: header plus attributes, length kept current.
struct Builder {
    buf: Vec<u8>,
}

impl Builder {
    fn new(method: u16, class: u16, txid: &[u8; 12]) -> Builder {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&msg_type(method, class).to_be_bytes());
        buf.extend_from_slice(&[0, 0]);
        buf.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        buf.extend_from_slice(txid);
        Builder { buf }
    }

    fn attr(&mut self, ty: u16, value: &[u8]) {
        self.buf.extend_from_slice(&ty.to_be_bytes());
        self.buf.extend_from_slice(&(value.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(value);
        self.buf.extend_from_slice(&[0u8; 3][..(4 - value.len() % 4) % 4]);
        let len = (self.buf.len() - HEADER_LEN) as u16;
        self.buf[2..4].copy_from_slice(&len.to_be_bytes());
    }

    fn xor_addr(&mut self, ty: u16, addr: SocketAddr) {
        let txid: [u8; 12] = self.buf[8..HEADER_LEN].try_into().expect("header is 20 bytes");
        let mut value = [0u8; 20];
        let len = encode_xor_addr(addr, &txid, &mut value);
        self.attr(ty, &value[..len]);
    }

    fn integrity(&mut self, key: &[u8]) {
        let mac = integrity_hmac(&self.buf, key);
        self.attr(ATTR_MESSAGE_INTEGRITY, &mac);
    }

    fn fingerprint(&mut self) {
        let crc = fingerprint_crc(&self.buf);
        self.attr(ATTR_FINGERPRINT, &crc.to_be_bytes());
    }
}

/// A parsed STUN message borrowing the datagram.
struct Message<'a> {
    method: u16,
    class: u16,
    txid: [u8; 12],
    raw: &'a [u8],
}

impl<'a> Message<'a> {
    fn parse(buf: &'a [u8]) -> Option<Message<'a>> {
        if buf.len() < HEADER_LEN || buf[0] & 0xC0 != 0 || buf[4..8] != MAGIC_COOKIE.to_be_bytes() {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        if len % 4 != 0 {
            return None;
        }
        let raw = buf.get(..HEADER_LEN + len)?;
        let ty = u16::from_be_bytes([buf[0], buf[1]]);
        let method = (ty & 0x000F) | ((ty >> 1) & 0x0070) | ((ty >> 2) & 0x0F80);
        let txid = buf[8..HEADER_LEN].try_into().ok()?;
        Some(Message { method, class: ty & 0x0110, txid, raw })
    }

    /// (offset in `raw`, type, value) of each attribute.
    fn attrs(&self) -> impl Iterator<Item = (usize, u16, &'a [u8])> + use<'a> {
        let raw = self.raw;
        let mut pos = HEADER_LEN;
        std::iter::from_fn(move || {
            let head = raw.get(pos..pos + 4)?;
            let ty = u16::from_be_bytes([head[0], head[1]]);
            let len = usize::from(u16::from_be_bytes([head[2], head[3]]));
            let value = raw.get(pos + 4..pos + 4 + len)?;
            let at = pos;
            pos += 4 + len.div_ceil(4) * 4;
            Some((at, ty, value))
        })
    }

    fn attr(&self, ty: u16) -> Option<&'a [u8]> {
        self.attrs().find(|a| a.1 == ty).map(|a| a.2)
    }

    fn text(&self, ty: u16) -> Option<&'a str> {
        std::str::from_utf8(self.attr(ty)?).ok()
    }

    fn xor_addr(&self, ty: u16) -> Option<SocketAddr> {
        decode_xor_addr(self.attr(ty)?, &self.txid)
    }

    /// ERROR-CODE as (code, reason phrase).
    fn error(&self) -> Option<(u16, &'a str)> {
        let v = self.attr(ATTR_ERROR_CODE)?;
        if v.len() < 4 {
            return None;
        }
        let code = u16::from(v[2] & 0x07) * 100 + u16::from(v[3]);
        Some((code, std::str::from_utf8(&v[4..]).unwrap_or("")))
    }

    /// Whether MESSAGE-INTEGRITY matches `key`; None when the message has none.
    fn integrity_ok(&self, key: &[u8]) -> Option<bool> {
        let (at, _, value) = self.attrs().find(|a| a.1 == ATTR_MESSAGE_INTEGRITY)?;
        Some(integrity_hmac(&self.raw[..at], key)[..] == *value)
    }
}

/// Resolves `turn:host:port?transport=udp` style URLs (from the server's credential message) to the first IPv4 address.
pub fn resolve_turn_url(url: &str) -> Option<SocketAddr> {
    let rest = url.strip_prefix("turn:")?;
    let (host_port, query) = rest.split_once('?').unwrap_or((rest, ""));
    for pair in query.split('&') {
        if let Some(transport) = pair.strip_prefix("transport=") {
            if !transport.eq_ignore_ascii_case("udp") {
                return None;
            }
        }
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (host_port, DEFAULT_PORT),
    };
    (host, port).to_socket_addrs().ok()?.find(SocketAddr::is_ipv4)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "192.0.2.10:3478";
    const RELAYED: &str = "203.0.113.5:50000";
    const MAPPED: &str = "198.51.100.7:40000";
    const PEER: &str = "203.0.113.9:60000";

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
        digits.chunks(2).map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap()).collect()
    }

    /// A server answer to `req` built by `attrs`, with MESSAGE-INTEGRITY when `key` is given.
    fn reply(req: &[u8], class: u16, key: Option<&[u8]>, attrs: impl FnOnce(&mut Builder)) -> Vec<u8> {
        let m = Message::parse(req).unwrap();
        let mut b = Builder::new(m.method, class, &m.txid);
        attrs(&mut b);
        if let Some(key) = key {
            b.integrity(key);
        }
        b.fingerprint();
        b.buf
    }

    fn error_code(code: u16, reason: &str) -> Vec<u8> {
        let mut v = vec![0, 0, (code / 100) as u8, (code % 100) as u8];
        v.extend_from_slice(reason.as_bytes());
        v
    }

    fn key() -> [u8; 16] {
        long_term_key("user", "example.org", "pass")
    }

    fn single(mut out: Vec<Vec<u8>>) -> Vec<u8> {
        assert_eq!(out.len(), 1, "expected one datagram");
        out.pop().unwrap()
    }

    /// Client after 401 → authenticated Allocate → success with LIFETIME 600.
    fn ready_client(t0: Instant) -> TurnClient {
        let mut c = TurnClient::new(addr(SERVER), "user", "pass", t0);
        let first = single(c.poll(t0));
        let challenge = reply(&first, CLASS_ERROR, None, |b| {
            b.attr(ATTR_ERROR_CODE, &error_code(401, "Unauthorized"));
            b.attr(ATTR_REALM, b"example.org");
            b.attr(ATTR_NONCE, b"n1");
        });
        assert_eq!(c.handle(t0, &challenge), None);
        let auth = single(c.poll(t0));
        let ok = reply(&auth, CLASS_SUCCESS, Some(&key()), |b| {
            b.xor_addr(ATTR_XOR_RELAYED_ADDRESS, addr(RELAYED));
            b.xor_addr(ATTR_XOR_MAPPED_ADDRESS, addr(MAPPED));
            b.attr(ATTR_LIFETIME, &600u32.to_be_bytes());
        });
        assert_eq!(c.handle(t0, &ok), None);
        assert_eq!(*c.state(), TurnState::Ready { relayed: addr(RELAYED), mapped: addr(MAPPED) });
        c
    }

    #[test]
    fn turn_rfc5769_sample_request_integrity_and_fingerprint() {
        let raw = hex(
            "00 01 00 58 21 12 a4 42 b7 e7 a7 01 bc 34 d6 86 fa 87 df ae
             80 22 00 10 53 54 55 4e 20 74 65 73 74 20 63 6c 69 65 6e 74
             00 24 00 04 6e 00 01 ff 80 29 00 08 93 2f f9 b1 51 26 3b 36
             00 06 00 09 65 76 74 6a 3a 68 36 76 59 20 20 20
             00 08 00 14 9a ea a7 0c bf d8 cb 56 78 1e f2 b5 b2 d3 f2 49 c1 b5 71 a2
             80 28 00 04 e5 7a 3b cf",
        );
        let msg = Message::parse(&raw).unwrap();
        assert_eq!(msg.text(ATTR_USERNAME), Some("evtj:h6vY"));
        assert_eq!(msg.integrity_ok(b"VOkJxbRl1RmTxUk/WvJxBt"), Some(true));
        assert_eq!(msg.integrity_ok(b"wrong"), Some(false));
        let fp_at = raw.len() - 8;
        assert_eq!(fingerprint_crc(&raw[..fp_at]).to_be_bytes(), raw[fp_at + 4..]);
    }

    #[test]
    fn turn_rfc5769_long_term_sample_request_is_reproduced() {
        let expected = hex(
            "00 01 00 60 21 12 a4 42 78 ad 34 33 c6 ad 72 c0 29 da 41 2e
             00 06 00 12 e3 83 9e e3 83 88 e3 83 aa e3 83 83 e3 82 af e3 82 b9 00 00
             00 15 00 1c 66 2f 2f 34 39 39 6b 39 35 34 64 36 4f 4c 33 34 6f 4c 39 46 53 54 76 79 36 34 73 41
             00 14 00 0b 65 78 61 6d 70 6c 65 2e 6f 72 67 00
             00 08 00 14 f6 70 24 65 6d d6 4a 3e 02 b8 e0 71 2e 85 c9 a2 8c a8 96 66",
        );
        let txid: [u8; 12] = expected[8..20].try_into().unwrap();
        let username = "\u{30DE}\u{30C8}\u{30EA}\u{30C3}\u{30AF}\u{30B9}";
        let key = long_term_key(username, "example.org", "TheMatrIX");
        let mut b = Builder::new(0x001, CLASS_REQUEST, &txid);
        b.attr(ATTR_USERNAME, username.as_bytes());
        b.attr(ATTR_NONCE, b"f//499k954d6OL34oL9FSTvy64sA");
        b.attr(ATTR_REALM, b"example.org");
        b.integrity(&key);
        assert_eq!(b.buf, expected);
    }

    #[test]
    fn turn_channel_data_round_trip_tolerates_padding() {
        let frame = encode_channel_data(0x4001, b"abcde");
        assert_eq!(frame, [0x40, 0x01, 0x00, 0x05, b'a', b'b', b'c', b'd', b'e']);
        let mut padded = frame.clone();
        padded.extend_from_slice(&[0, 0, 0]);
        assert_eq!(decode_channel_data(&padded), Some((0x4001, &b"abcde"[..])));
        assert_eq!(decode_channel_data(&frame[..7]), None, "truncated");
        assert_eq!(decode_channel_data(&[0x00, 0x01, 0, 0]), None, "STUN, not ChannelData");
    }

    #[test]
    fn turn_xor_address_round_trip_v4_and_v6() {
        let txid = [7u8; 12];
        for a in [addr("192.0.2.1:32853"), addr("[2001:db8:1234:5678:11:2233:4455:6677]:32853")] {
            let mut v = [0u8; 20];
            let n = encode_xor_addr(a, &txid, &mut v);
            assert_eq!(decode_xor_addr(&v[..n], &txid), Some(a));
        }
    }

    #[test]
    fn turn_401_then_authenticated_allocate() {
        let t0 = Instant::now();
        let mut c = TurnClient::new(addr(SERVER), "user", "pass", t0);
        assert_eq!(c.server(), addr(SERVER));
        assert_eq!(*c.state(), TurnState::Allocating);
        assert_eq!(c.next_timeout(), Some(t0));
        let first = single(c.poll(t0));
        let m = Message::parse(&first).unwrap();
        assert_eq!((m.method, m.class), (METHOD_ALLOCATE, CLASS_REQUEST));
        assert_eq!(m.attr(ATTR_REQUESTED_TRANSPORT), Some(&[17u8, 0, 0, 0][..]));
        assert_eq!(m.attr(ATTR_USERNAME), None);
        let challenge = reply(&first, CLASS_ERROR, None, |b| {
            b.attr(ATTR_ERROR_CODE, &error_code(401, "Unauthorized"));
            b.attr(ATTR_REALM, b"example.org");
            b.attr(ATTR_NONCE, b"n1");
        });
        assert_eq!(c.handle(t0, &challenge), None);
        assert_eq!(c.next_timeout(), Some(t0));
        let auth = single(c.poll(t0));
        let m = Message::parse(&auth).unwrap();
        assert_eq!(m.method, METHOD_ALLOCATE);
        assert_ne!(m.txid, Message::parse(&first).unwrap().txid);
        assert_eq!(m.text(ATTR_USERNAME), Some("user"));
        assert_eq!(m.text(ATTR_REALM), Some("example.org"));
        assert_eq!(m.text(ATTR_NONCE), Some("n1"));
        assert_eq!(m.integrity_ok(&key()), Some(true));
        let fp_at = auth.len() - 8;
        assert_eq!(fingerprint_crc(&auth[..fp_at]).to_be_bytes(), auth[fp_at + 4..]);

        // A forged success (wrong key) is ignored; the genuine one makes the client Ready.
        let addrs = |b: &mut Builder| {
            b.xor_addr(ATTR_XOR_RELAYED_ADDRESS, addr(RELAYED));
            b.xor_addr(ATTR_XOR_MAPPED_ADDRESS, addr(MAPPED));
            b.attr(ATTR_LIFETIME, &600u32.to_be_bytes());
        };
        c.handle(t0, &reply(&auth, CLASS_SUCCESS, Some(b"forged"), addrs));
        assert_eq!(*c.state(), TurnState::Allocating);
        c.handle(t0, &reply(&auth, CLASS_SUCCESS, Some(&key()), addrs));
        assert_eq!(*c.state(), TurnState::Ready { relayed: addr(RELAYED), mapped: addr(MAPPED) });
        // No retransmits left; next work is the allocation refresh at half the lifetime.
        assert!(c.poll(t0 + Duration::from_secs(1)).is_empty());
        assert_eq!(c.next_timeout(), Some(t0 + Duration::from_secs(300)));
        let refresh = single(c.poll(t0 + Duration::from_secs(300)));
        let m = Message::parse(&refresh).unwrap();
        assert_eq!(m.method, METHOD_REFRESH);
        assert_eq!(m.attr(ATTR_LIFETIME), Some(&600u32.to_be_bytes()[..]));
        assert_eq!(m.integrity_ok(&key()), Some(true));
    }

    #[test]
    fn turn_438_stale_nonce_retries_with_new_nonce() {
        let t0 = Instant::now();
        let mut c = ready_client(t0);
        let t1 = t0 + Duration::from_secs(300);
        let refresh = single(c.poll(t1));
        let stale = reply(&refresh, CLASS_ERROR, Some(&key()), |b| {
            b.attr(ATTR_ERROR_CODE, &error_code(438, "Stale Nonce"));
            b.attr(ATTR_REALM, b"example.org");
            b.attr(ATTR_NONCE, b"n2");
        });
        assert_eq!(c.handle(t1, &stale), None);
        assert!(matches!(c.state(), TurnState::Ready { .. }));
        let retry = single(c.poll(t1));
        let m = Message::parse(&retry).unwrap();
        assert_eq!(m.method, METHOD_REFRESH);
        assert_eq!(m.text(ATTR_NONCE), Some("n2"));
        assert_eq!(m.integrity_ok(&key()), Some(true));
        c.handle(t1, &reply(&retry, CLASS_SUCCESS, Some(&key()), |b| b.attr(ATTR_LIFETIME, &100u32.to_be_bytes())));
        assert_eq!(c.next_timeout(), Some(t1 + Duration::from_secs(50)));
    }

    #[test]
    fn turn_other_error_fails_with_code_and_reason() {
        let t0 = Instant::now();
        let mut c = TurnClient::new(addr(SERVER), "user", "pass", t0);
        let first = single(c.poll(t0));
        c.handle(t0, &reply(&first, CLASS_ERROR, None, |b| b.attr(ATTR_ERROR_CODE, &error_code(403, "Forbidden"))));
        assert_eq!(*c.state(), TurnState::Failed("allocate error 403 Forbidden".into()));
        assert_eq!(c.next_timeout(), None);
        assert!(c.poll(t0).is_empty());
    }

    #[test]
    fn turn_retransmit_schedule_then_timeout() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = TurnClient::new(addr(SERVER), "user", "pass", t0);
        let first = single(c.poll(t0));
        assert_eq!(c.next_timeout(), Some(ms(250)));
        assert!(c.poll(ms(249)).is_empty());
        for (at, next) in [(250, 750), (750, 1750), (1750, 3750), (3750, 7750), (7750, 11750)] {
            assert_eq!(single(c.poll(ms(at))), first, "retransmit at {at} ms");
            assert_eq!(c.next_timeout(), Some(ms(next)));
        }
        assert!(c.poll(ms(11749)).is_empty());
        assert_eq!(*c.state(), TurnState::Allocating);
        assert!(c.poll(ms(11750)).is_empty());
        assert_eq!(*c.state(), TurnState::Failed("timeout".into()));
        assert_eq!(c.next_timeout(), None);
    }

    #[test]
    fn turn_unknown_transaction_is_ignored() {
        let t0 = Instant::now();
        let mut c = TurnClient::new(addr(SERVER), "user", "pass", t0);
        let first = single(c.poll(t0));
        let mut other = first.clone();
        other[19] ^= 0xFF;
        let bogus = reply(&other, CLASS_ERROR, None, |b| b.attr(ATTR_ERROR_CODE, &error_code(403, "Forbidden")));
        assert_eq!(c.handle(t0, &bogus), None);
        assert_eq!(*c.state(), TurnState::Allocating);
        assert_eq!(c.next_timeout(), Some(t0 + INITIAL_RTO));
    }

    #[test]
    fn turn_peers_permissions_channels_and_close() {
        let t0 = Instant::now();
        let peer = addr(PEER);
        let mut c = TurnClient::new(addr(SERVER), "user", "pass", t0);
        c.add_peer(peer);
        assert_eq!(c.wrap(peer, b"x"), None, "not Ready yet");
        let mut c = {
            // Same flow as ready_client but with the peer queued before Ready.
            let first = single(c.poll(t0));
            c.handle(t0, &reply(&first, CLASS_ERROR, None, |b| {
                b.attr(ATTR_ERROR_CODE, &error_code(401, "Unauthorized"));
                b.attr(ATTR_REALM, b"example.org");
                b.attr(ATTR_NONCE, b"n1");
            }));
            let auth = single(c.poll(t0));
            c.handle(t0, &reply(&auth, CLASS_SUCCESS, Some(&key()), |b| {
                b.xor_addr(ATTR_XOR_RELAYED_ADDRESS, addr(RELAYED));
                b.xor_addr(ATTR_XOR_MAPPED_ADDRESS, addr(MAPPED));
            }));
            c
        };
        c.add_peer(peer); // idempotent
        assert_eq!(c.next_timeout(), Some(t0), "queued peer work is due");
        let out = c.poll(t0);
        assert_eq!(out.len(), 2);
        let (mut perm, mut bind) = (None, None);
        for d in &out {
            let m = Message::parse(d).unwrap();
            assert_eq!(m.integrity_ok(&key()), Some(true));
            assert_eq!(m.xor_addr(ATTR_XOR_PEER_ADDRESS).map(|a| a.ip()), Some(peer.ip()));
            match m.method {
                METHOD_CREATE_PERMISSION => perm = Some(d.clone()),
                METHOD_CHANNEL_BIND => {
                    assert_eq!(m.attr(ATTR_CHANNEL_NUMBER), Some(&[0x40u8, 0x00, 0, 0][..]));
                    assert_eq!(m.xor_addr(ATTR_XOR_PEER_ADDRESS), Some(peer));
                    bind = Some(d.clone());
                }
                other => panic!("unexpected method {other:#x}"),
            }
        }

        // Before the channel is bound: Send indication.
        let send = c.wrap(peer, b"hello").unwrap();
        let m = Message::parse(&send).unwrap();
        assert_eq!((m.method, m.class), (METHOD_SEND, CLASS_INDICATION));
        assert_eq!(m.xor_addr(ATTR_XOR_PEER_ADDRESS), Some(peer));
        assert_eq!(m.attr(ATTR_DATA), Some(&b"hello"[..]));
        assert_eq!(c.wrap(addr("203.0.113.77:1"), b"x"), None, "unknown peer");

        c.handle(t0, &reply(&perm.unwrap(), CLASS_SUCCESS, Some(&key()), |_| {}));
        c.handle(t0, &reply(&bind.unwrap(), CLASS_SUCCESS, Some(&key()), |_| {}));
        assert_eq!(c.wrap(peer, b"hello").unwrap(), encode_channel_data(0x4000, b"hello"));

        // Incoming: padded ChannelData, Data indications from permitted and foreign IPs.
        let mut frame = encode_channel_data(0x4000, b"pong");
        frame.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(c.handle(t0, &frame), Some((peer, b"pong".to_vec())));
        assert_eq!(c.handle(t0, &encode_channel_data(0x4001, b"?")), None, "unbound channel");
        let data_from = |from: SocketAddr| {
            let mut b = Builder::new(METHOD_DATA, CLASS_INDICATION, &[9; 12]);
            b.xor_addr(ATTR_XOR_PEER_ADDRESS, from);
            b.attr(ATTR_DATA, b"via data");
            b.buf
        };
        assert_eq!(c.handle(t0, &data_from(peer)), Some((peer, b"via data".to_vec())));
        assert_eq!(c.handle(t0, &data_from(addr("198.51.100.99:5"))), None, "no permission");

        // Timers: permission 4 min, allocation 5 min (default 600 s lifetime), channel 9 min.
        let s = |n: u64| t0 + Duration::from_secs(n);
        assert_eq!(c.next_timeout(), Some(s(240)));
        let m = single(c.poll(s(240)));
        assert_eq!(Message::parse(&m).unwrap().method, METHOD_CREATE_PERMISSION);
        c.handle(s(240), &reply(&m, CLASS_SUCCESS, Some(&key()), |_| {}));
        assert_eq!(c.next_timeout(), Some(s(300)));
        let m = single(c.poll(s(300)));
        assert_eq!(Message::parse(&m).unwrap().method, METHOD_REFRESH);
        c.handle(s(300), &reply(&m, CLASS_SUCCESS, Some(&key()), |b| b.attr(ATTR_LIFETIME, &600u32.to_be_bytes())));
        assert_eq!(c.next_timeout(), Some(s(480)));
        let m = single(c.poll(s(480)));
        assert_eq!(Message::parse(&m).unwrap().method, METHOD_CREATE_PERMISSION);
        c.handle(s(480), &reply(&m, CLASS_SUCCESS, Some(&key()), |_| {}));
        assert_eq!(c.next_timeout(), Some(s(540)));
        let m = single(c.poll(s(540)));
        assert_eq!(Message::parse(&m).unwrap().method, METHOD_CHANNEL_BIND);

        let bye = c.close().unwrap();
        let m = Message::parse(&bye).unwrap();
        assert_eq!(m.method, METHOD_REFRESH);
        assert_eq!(m.attr(ATTR_LIFETIME), Some(&[0u8, 0, 0, 0][..]));
        assert_eq!(m.integrity_ok(&key()), Some(true));
        assert_eq!(*c.state(), TurnState::Failed("closed".into()));
        assert_eq!(c.next_timeout(), None);
        assert_eq!(c.wrap(peer, b"x"), None);
        assert_eq!(c.close(), None);
    }

    #[test]
    fn turn_resolve_url_forms() {
        let a = addr("127.0.0.1:3478");
        assert_eq!(resolve_turn_url("turn:127.0.0.1:3478?transport=udp"), Some(a));
        assert_eq!(resolve_turn_url("turn:127.0.0.1"), Some(a));
        assert_eq!(resolve_turn_url("turn:127.0.0.1:53?transport=udp"), Some(addr("127.0.0.1:53")));
        assert_eq!(resolve_turn_url("turn:127.0.0.1:3478?transport=tcp"), None);
        assert_eq!(resolve_turn_url("turns:127.0.0.1:5349?transport=tcp"), None);
        assert_eq!(resolve_turn_url("stun:127.0.0.1:3478"), None);
    }

    /// Sends what `c` wants to send, then feeds it every datagram waiting on `sock`.
    fn pump(sock: &std::net::UdpSocket, c: &mut TurnClient) -> Vec<(SocketAddr, Vec<u8>)> {
        for d in c.poll(Instant::now()) {
            sock.send_to(&d, c.server()).unwrap();
        }
        let mut got = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, from)) if from == c.server() => got.extend(c.handle(Instant::now(), &buf[..n])),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return got,
                Err(e) => panic!("recv: {e}"),
            }
        }
    }

    #[test]
    #[ignore = "needs EBOBO_TURN_USER/EBOBO_TURN_PASS (Cloudflare TURN credential) and internet"]
    fn turn_live_cloudflare_relay_round_trip() {
        let (Ok(user), Ok(pass)) = (std::env::var("EBOBO_TURN_USER"), std::env::var("EBOBO_TURN_PASS")) else {
            println!("skipping: EBOBO_TURN_USER/EBOBO_TURN_PASS not set");
            return;
        };
        let server = resolve_turn_url("turn:turn.cloudflare.com:3478?transport=udp").expect("resolve TURN server");
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut ends: Vec<(std::net::UdpSocket, TurnClient)> = (0..2)
            .map(|_| {
                let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
                sock.set_nonblocking(true).unwrap();
                (sock, TurnClient::new(server, &user, &pass, Instant::now()))
            })
            .collect();
        let relayed = |c: &TurnClient| match c.state() {
            TurnState::Ready { relayed, .. } => Some(*relayed),
            TurnState::Failed(why) => panic!("allocation failed: {why}"),
            TurnState::Allocating => None,
        };
        let addrs = loop {
            assert!(Instant::now() < deadline, "allocations timed out");
            for (sock, c) in &mut ends {
                pump(sock, c);
            }
            if let (Some(a), Some(b)) = (relayed(&ends[0].1), relayed(&ends[1].1)) {
                break [a, b];
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        println!("relayed addresses: {addrs:?}");
        ends[0].1.add_peer(addrs[1]);
        ends[1].1.add_peer(addrs[0]);
        let payloads = [b"hello from a".to_vec(), b"hello from b".to_vec()];

        // Round 1 accepts any framing (Send indications first); round 2 waits until both
        // sides frame as ChannelData, so both paths are exercised.
        for channel_only in [false, true] {
            let mut got = [false, false];
            let mut last_send: Option<Instant> = None;
            while !(got[0] && got[1]) {
                assert!(Instant::now() < deadline, "exchange timed out (channel_only={channel_only})");
                for (i, (sock, c)) in ends.iter_mut().enumerate() {
                    for (from, data) in pump(sock, c) {
                        assert_eq!(from, addrs[1 - i]);
                        assert_eq!(data, payloads[1 - i]);
                        got[i] = true;
                    }
                }
                if last_send.is_none_or(|t| t.elapsed() >= Duration::from_millis(200)) {
                    for (i, (sock, c)) in ends.iter_mut().enumerate() {
                        let frame = c.wrap(addrs[1 - i], &payloads[i]).expect("peer added and Ready");
                        if !channel_only || frame[0] & 0xC0 == 0x40 {
                            sock.send_to(&frame, c.server()).unwrap();
                        }
                    }
                    last_send = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for (sock, c) in &mut ends {
            if let Some(bye) = c.close() {
                let _ = sock.send_to(&bye, c.server());
            }
        }
    }
}
