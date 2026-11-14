//! Per-peer routes (direct UDP or WebSocket relay) and the ggrs socket. Owner: transport slice.

use std::collections::HashMap;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::mpsc::{Receiver, Sender};

use crate::packet::Packet;

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

/// Binds the session's UDP socket: dual-stack `[::]:port` (IPV6_V6ONLY off), else
/// `0.0.0.0:port` when IPv6 is unavailable.
pub(crate) fn bind_udp(port: u16) -> io::Result<UdpSocket> {
    let dual = || -> io::Result<UdpSocket> {
        let s = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
        s.set_only_v6(false)?;
        s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
        Ok(s.into())
    };
    dual().or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)))
}

/// IPv4-mapped `::ffff:a.b.c.d` → plain V4; everything else unchanged.
pub(crate) fn unmap(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(ip) => SocketAddr::new(ip.into(), v6.port()),
            None => a,
        },
        SocketAddr::V4(_) => a,
    }
}

/// Whether `socket` is an IPv6 (dual-stack) socket.
pub(crate) fn is_v6(socket: &UdpSocket) -> bool {
    socket.local_addr().is_ok_and(|a| a.is_ipv6())
}

/// Destination as the socket's family needs it: V4 is mapped for an IPv6 socket; a real
/// IPv6 address is unreachable (`None`) from an IPv4-only socket.
fn dest(v6_socket: bool, a: SocketAddr) -> Option<SocketAddr> {
    match (v6_socket, unmap(a)) {
        (true, SocketAddr::V4(v4)) => Some(SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port())),
        (false, SocketAddr::V6(_)) => None,
        (_, a) => Some(a),
    }
}

/// `send_to` for either socket family; an unreachable family is silently dropped.
pub(crate) fn send_udp_to(socket: &UdpSocket, v6_socket: bool, packet: &[u8], to: SocketAddr) -> io::Result<usize> {
    match dest(v6_socket, to) {
        Some(d) => socket.send_to(packet, d),
        None => Ok(0),
    }
}

/// `recv_from` with the source normalized by `unmap`.
pub(crate) fn recv_udp_from(socket: &UdpSocket, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    socket.recv_from(buf).map(|(n, from)| (n, unmap(from)))
}

/// One UDP socket plus an optional relay, with a route per peer. Owned by the mesh.
pub(crate) struct Transport {
    udp: UdpSocket,
    v6: bool,
    routes: [Option<Route>; 4],
    by_addr: HashMap<SocketAddr, PeerId>,
    relay: Option<RelayHandle>,
    buf: Vec<u8>,
}

impl Transport {
    /// Non-blocking socket, no routes yet.
    pub(crate) fn new(udp: UdpSocket, relay: Option<RelayHandle>) -> Transport {
        let _ = udp.set_nonblocking(true);
        Transport {
            v6: is_v6(&udp),
            udp,
            routes: [None; 4],
            by_addr: HashMap::new(),
            relay,
            buf: vec![0; 1500],
        }
    }

    /// UDP-only transport with fixed routes (direct-IP mode after the handshake, tests).
    pub(crate) fn udp(udp: UdpSocket, peers: &[(PeerId, SocketAddr)]) -> Transport {
        let mut t = Transport::new(udp, None);
        for &(p, a) in peers {
            t.set_route(p, Route::Udp(a));
        }
        t
    }

    pub(crate) fn set_route(&mut self, peer: PeerId, route: Route) {
        let route = match route {
            Route::Udp(a) => Route::Udp(unmap(a)),
            r => r,
        };
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

    pub(crate) fn route_kind(&self, peer: PeerId) -> Option<RouteKind> {
        self.route(peer).map(|r| match r {
            Route::Udp(_) => RouteKind::Direct,
            Route::Relay => RouteKind::Relay,
        })
    }

    /// Peers that currently have a route.
    pub(crate) fn peers(&self) -> Vec<PeerId> {
        (0..4u8).map(PeerId).filter(|p| self.route(*p).is_some()).collect()
    }

    /// Sends one packet to a routed peer; silently dropped when there is no route.
    pub(crate) fn send(&mut self, to: PeerId, packet: &[u8]) {
        match self.route(to) {
            Some(Route::Udp(a)) => self.send_udp(a, packet),
            Some(Route::Relay) => self.send_relay(to, packet),
            None => {}
        }
    }

    /// Sends a raw packet to a UDP address (punching, before a route exists).
    pub(crate) fn send_udp(&mut self, addr: SocketAddr, packet: &[u8]) {
        // WouldBlock / unreachable / refused all mean "packet lost" on an unreliable route.
        let _ = send_udp_to(&self.udp, self.v6, packet, addr);
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
            match recv_udp_from(&self.udp, &mut self.buf) {
                Ok((n, from)) => return Some((Source::Udp(from), self.buf[..n].to_vec())),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => continue,
            }
        }
        let r = self.relay.as_ref()?;
        r.inbound.try_recv().ok().map(|(p, b)| (Source::Relay(p), b))
    }

    /// The peer whose direct route is `addr`.
    pub(crate) fn peer_at(&self, addr: SocketAddr) -> Option<PeerId> {
        self.by_addr.get(&addr).copied()
    }

    /// Tells every routed peer we are leaving (BYE, a few copies) and closes the relay.
    pub(crate) fn close(&mut self) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Next packet from a routed peer (the mesh's `routed_source` for UDP).
    fn recv(t: &mut Transport) -> Option<(PeerId, Vec<u8>)> {
        loop {
            let (src, bytes) = t.recv_raw()?;
            if let Source::Udp(a) = src {
                if let Some(p) = t.peer_at(a) {
                    return Some((p, bytes));
                }
            }
        }
    }

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
            Transport::udp(a, &[(PeerId(1), ba)]),
            Transport::udp(b, &[(PeerId(0), aa)]),
        )
    }

    #[test]
    fn transport_ctrl_both_ways() {
        let (mut a, mut b) = pair();
        let p = Packet::Ctrl { seq: 7, json: b"{}" }.to_vec();
        a.send(PeerId(1), &p);
        assert_eq!(wait(|| recv(&mut b)), Some((PeerId(0), p.clone())));
        b.send(PeerId(0), &p);
        assert_eq!(wait(|| recv(&mut a)), Some((PeerId(1), p.clone())));
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
        assert_eq!(recv(&mut a), None);
        stranger.send_to(&p, to).unwrap();
        let (src, bytes) = wait(|| a.recv_raw()).unwrap();
        assert_eq!(src, Source::Udp(stranger.local_addr().unwrap()));
        assert_eq!(bytes, p);
    }

    #[test]
    fn transport_unmap_and_dest() {
        let mapped: SocketAddr = "[::ffff:192.0.2.7]:4000".parse().unwrap();
        let v4: SocketAddr = "192.0.2.7:4000".parse().unwrap();
        let v6: SocketAddr = "[2001:db8::1]:4000".parse().unwrap();
        assert_eq!(unmap(mapped), v4);
        assert_eq!(unmap(v4), v4);
        assert_eq!(unmap(v6), v6);
        assert_eq!(dest(true, v4), Some(mapped));
        assert_eq!(dest(true, v6), Some(v6));
        assert_eq!(dest(false, v4), Some(v4));
        assert_eq!(dest(false, mapped), Some(v4));
        assert_eq!(dest(false, v6), None);
    }

    #[test]
    fn transport_dual_stack_talks_to_v4() {
        let dual = bind_udp(0).unwrap();
        if !is_v6(&dual) {
            eprintln!("IPv6 unavailable; dual-stack test skipped");
            return;
        }
        let port = dual.local_addr().unwrap().port();
        let v4 = sock();
        let v4_addr = v4.local_addr().unwrap();
        let dual_seen_as = SocketAddr::from(([127, 0, 0, 1], port));
        let mut a = Transport::udp(dual, &[(PeerId(1), v4_addr)]);
        let mut b = Transport::udp(v4, &[(PeerId(0), dual_seen_as)]);
        let p = Packet::Ctrl { seq: 1, json: b"{}" }.to_vec();
        a.send(PeerId(1), &p);
        assert_eq!(wait(|| recv(&mut b)), Some((PeerId(0), p.clone())));
        b.send_udp(dual_seen_as, &p);
        let (src, bytes) = wait(|| a.recv_raw()).unwrap();
        assert_eq!(src, Source::Udp(v4_addr));
        assert!(matches!(src, Source::Udp(SocketAddr::V4(_))));
        assert_eq!(bytes, p);
        // An IPv4-only socket drops IPv6 destinations silently.
        b.send_udp("[2001:db8::1]:9".parse().unwrap(), &p);
    }
}
