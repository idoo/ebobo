//! Per-peer routes (direct UDP or WebSocket relay) and the ggrs socket. Owner: transport slice.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, Sender};

use crate::packet::{Packet, MAX_PACKET};

/// Peer id inside a room: 0 = host, 1..3 = guests. Also the GGRS address type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    /// Direct UDP (hole punched, LAN, or direct-IP mode).
    Direct,
    /// Through the signaling server's WebSocket.
    Relay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Udp(SocketAddr),
    Relay,
}

/// Where a raw packet came from, before it is matched to a peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Udp(SocketAddr),
    Relay(PeerId),
}

/// Commands to the WebSocket thread owned by the mesh.
pub(crate) enum RelayOut {
    /// Binary relay frame: `[dest id][packet]` on the wire.
    Binary { to: PeerId, packet: Vec<u8> },
    /// Signaling JSON text frame.
    Text(String),
    Close,
    /// Tests: cut the TCP connection without a close frame (a Wi-Fi blip) to exercise rejoin.
    #[cfg(test)]
    Drop,
}

/// Channels to the mesh's WebSocket thread. `inbound` carries relayed packets only (binary
/// frames, already split into source id + packet); signaling text stays inside the mesh.
pub(crate) struct RelayHandle {
    pub(crate) out: Sender<RelayOut>,
    pub(crate) inbound: Receiver<(PeerId, Vec<u8>)>,
}

/// One UDP socket plus an optional relay, with a route per peer. `Send`, so the lobby thread
/// can hand it to the game loop.
pub struct Transport {
    udp: UdpSocket,
    me: PeerId,
    routes: [Option<Route>; 4],
    by_addr: HashMap<SocketAddr, PeerId>,
    relay: Option<RelayHandle>,
    buf: Vec<u8>,
    departed: Vec<PeerId>,
}

const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Transport>();
};

impl Transport {
    /// Non-blocking socket, no routes yet.
    pub(crate) fn new(udp: UdpSocket, me: PeerId, relay: Option<RelayHandle>) -> Transport {
        let _ = udp.set_nonblocking(true);
        Transport {
            udp,
            me,
            routes: [None; 4],
            by_addr: HashMap::new(),
            relay,
            buf: vec![0; 1500],
            departed: Vec::new(),
        }
    }

    /// UDP-only transport with fixed routes (direct-IP mode after the handshake, tests).
    pub fn udp(udp: UdpSocket, me: PeerId, peers: &[(PeerId, SocketAddr)]) -> Transport {
        let mut t = Transport::new(udp, me, None);
        for &(p, a) in peers {
            t.set_route(p, Route::Udp(a));
        }
        t
    }

    pub fn me(&self) -> PeerId {
        self.me
    }

    pub(crate) fn set_me(&mut self, me: PeerId) {
        self.me = me;
    }

    pub(crate) fn set_route(&mut self, peer: PeerId, route: Route) {
        let Some(slot) = self.routes.get_mut(peer.0 as usize) else { return };
        if let Some(Route::Udp(old)) = slot.take() {
            self.by_addr.remove(&old);
        }
        if let Route::Udp(a) = route {
            self.by_addr.insert(a, peer);
        }
        *slot = Some(route);
    }

    pub(crate) fn remove_route(&mut self, peer: PeerId) {
        if let Some(slot) = self.routes.get_mut(peer.0 as usize) {
            if let Some(Route::Udp(a)) = slot.take() {
                self.by_addr.remove(&a);
            }
        }
    }

    fn route(&self, peer: PeerId) -> Option<Route> {
        self.routes.get(peer.0 as usize).copied().flatten()
    }

    pub fn route_kind(&self, peer: PeerId) -> Option<RouteKind> {
        self.route(peer).map(|r| match r {
            Route::Udp(_) => RouteKind::Direct,
            Route::Relay => RouteKind::Relay,
        })
    }

    /// Peers that currently have a route.
    pub fn peers(&self) -> Vec<PeerId> {
        (0..4u8).map(PeerId).filter(|p| self.route(*p).is_some()).collect()
    }

    /// Sends one packet to a routed peer; silently dropped when there is no route.
    pub fn send(&mut self, to: PeerId, packet: &[u8]) {
        match self.route(to) {
            Some(Route::Udp(a)) => self.send_udp(a, packet),
            Some(Route::Relay) => self.send_relay(to, packet),
            None => {}
        }
    }

    /// Sends a raw packet to a UDP address (punching, before a route exists).
    pub(crate) fn send_udp(&mut self, addr: SocketAddr, packet: &[u8]) {
        // WouldBlock / unreachable / refused all mean "packet lost" on an unreliable route.
        let _ = self.udp.send_to(packet, addr);
    }

    /// Sends a raw packet through the relay to a peer that has no route yet.
    pub(crate) fn send_relay(&mut self, to: PeerId, packet: &[u8]) {
        if let Some(r) = &self.relay {
            let _ = r.out.send(RelayOut::Binary { to, packet: packet.to_vec() });
        }
    }

    /// Sends a signaling text frame through the WebSocket (no-op without relay).
    pub(crate) fn send_text(&mut self, json: String) {
        if let Some(r) = &self.relay {
            let _ = r.out.send(RelayOut::Text(json));
        }
    }

    /// Next raw packet from anywhere (UDP or relay), non-blocking. Mesh uses this while
    /// punching.
    pub(crate) fn recv_raw(&mut self) -> Option<(Source, Vec<u8>)> {
        // Bounded: transient errors (ICMP-induced ECONNRESET etc.) are skipped.
        for _ in 0..64 {
            match self.udp.recv_from(&mut self.buf) {
                Ok((n, from)) => return Some((Source::Udp(from), self.buf[..n].to_vec())),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => continue,
            }
        }
        let r = self.relay.as_ref()?;
        r.inbound.try_recv().ok().map(|(p, b)| (Source::Relay(p), b))
    }

    /// Next packet from a routed peer, non-blocking. Packets from unknown sources are dropped.
    pub fn recv(&mut self) -> Option<(PeerId, Vec<u8>)> {
        loop {
            let (src, bytes) = self.recv_raw()?;
            let peer = match src {
                Source::Udp(a) => self.by_addr.get(&a).copied(),
                Source::Relay(p) => self.route(p).is_some().then_some(p),
            };
            if let Some(p) = peer {
                if Packet::parse(&bytes).is_some() {
                    return Some((p, bytes));
                }
            }
        }
    }

    /// Peers that sent BYE since the last call (collected by the ggrs socket path).
    pub fn take_departed(&mut self) -> Vec<PeerId> {
        std::mem::take(&mut self.departed)
    }

    /// Tells every routed peer we are leaving (BYE, a few copies) and closes the relay.
    pub fn close(&mut self) {
        let bye = Packet::Bye.to_vec();
        for p in self.peers() {
            for _ in 0..3 {
                self.send(p, &bye);
            }
        }
        if let Some(r) = &self.relay {
            let _ = r.out.send(RelayOut::Close);
        }
    }
}

impl ggrs::NonBlockingSocket<PeerId> for Transport {
    /// GGRS packet = `Packet::Ggrs(bincode 1.3 of msg)`.
    fn send_to(&mut self, msg: &ggrs::Message, addr: &PeerId) {
        let Ok(body) = bincode::serialize(msg) else { return };
        let pkt = Packet::Ggrs(&body).to_vec();
        if pkt.len() <= MAX_PACKET {
            self.send(*addr, &pkt);
        }
    }

    /// Drains `recv`, decodes GGRS packets, records BYE senders for `take_departed`, ignores the
    /// rest (late PUNCH/KEEPALIVE/CTRL).
    fn receive_all_messages(&mut self) -> Vec<(PeerId, ggrs::Message)> {
        let mut out = Vec::new();
        while let Some((p, bytes)) = self.recv() {
            match Packet::parse(&bytes) {
                Some(Packet::Ggrs(body)) => {
                    if let Ok(m) = bincode::deserialize::<ggrs::Message>(body) {
                        out.push((p, m));
                    }
                }
                Some(Packet::Bye) => {
                    if !self.departed.contains(&p) {
                        self.departed.push(p);
                    }
                    self.remove_route(p);
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ggrs::NonBlockingSocket;
    use std::time::{Duration, Instant};

    fn sock() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").unwrap()
    }

    fn wait<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            if let Some(v) = f() {
                return Some(v);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    fn pair() -> (Transport, Transport) {
        let (a, b) = (sock(), sock());
        let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        (
            Transport::udp(a, PeerId(0), &[(PeerId(1), ba)]),
            Transport::udp(b, PeerId(1), &[(PeerId(0), aa)]),
        )
    }

    #[test]
    fn transport_ctrl_both_ways() {
        let (mut a, mut b) = pair();
        let p = Packet::Ctrl { seq: 7, json: b"{}" }.to_vec();
        a.send(PeerId(1), &p);
        assert_eq!(wait(|| b.recv()), Some((PeerId(0), p.clone())));
        b.send(PeerId(0), &p);
        assert_eq!(wait(|| a.recv()), Some((PeerId(1), p.clone())));
        assert_eq!(a.route_kind(PeerId(1)), Some(RouteKind::Direct));
        assert_eq!(a.peers(), vec![PeerId(1)]);
    }

    #[test]
    fn transport_unrouted_dropped_by_recv_seen_by_raw() {
        let (mut a, _b) = pair();
        let stranger = sock();
        let to = a.udp.local_addr().unwrap();
        let p = Packet::Keepalive.to_vec();
        stranger.send_to(&p, to).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(a.recv(), None);
        stranger.send_to(&p, to).unwrap();
        let (src, bytes) = wait(|| a.recv_raw()).unwrap();
        assert_eq!(src, Source::Udp(stranger.local_addr().unwrap()));
        assert_eq!(bytes, p);
    }

    #[test]
    fn transport_ggrs_framing_and_bye() {
        let (mut a, mut b) = pair();
        // A GGRS-framed packet with an undecodable body is ignored; BYE marks departure.
        a.send(PeerId(1), &Packet::Ggrs(&[0xFF; 3]).to_vec());
        a.close();
        let mut departed = Vec::new();
        wait(|| {
            assert!(b.receive_all_messages().is_empty());
            departed.extend(b.take_departed());
            (!departed.is_empty()).then_some(())
        })
        .unwrap();
        assert_eq!(departed, vec![PeerId(0)]);
        assert_eq!(b.route_kind(PeerId(0)), None);
    }

    #[test]
    fn transport_ggrs_message_round_trip() {
        let (mut a, mut b) = pair();
        // Message { header: { magic: 0x1234 }, body: SyncRequest { random_request: 42 } }
        let mut raw = 0x1234u16.to_le_bytes().to_vec();
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&42u32.to_le_bytes());
        let msg: ggrs::Message = bincode::deserialize(&raw).unwrap();
        a.send_to(&msg, &PeerId(1));
        let got = wait(|| b.receive_all_messages().pop()).unwrap();
        assert_eq!(got, (PeerId(0), msg));
    }
}
