//! STUN (RFC 5389 Binding) and local address discovery. Owner: transport slice.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

pub(crate) const STUN_SERVERS: &[&str] = &["stun.cloudflare.com:3478", "stun.l.google.com:19302"];

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED: u16 = 0x0001;
const ATTR_XOR_MAPPED: u16 = 0x0020;

/// Result of asking every STUN server from the game socket.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StunResult {
    /// Reflexive (public) addresses, deduplicated, in server order.
    pub(crate) public: Vec<SocketAddr>,
    /// Two servers saw different ports: endpoint-dependent (symmetric) NAT.
    pub(crate) symmetric: bool,
}

fn encode_request(txid: &[u8; 12]) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    // length 0
    b[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    b[8..20].copy_from_slice(txid);
    b
}

/// Mapped IPv4 address of a Binding Success response with this transaction id.
fn parse_response(buf: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if buf.len() < 20
        || u16::from_be_bytes([buf[0], buf[1]]) != BINDING_SUCCESS
        || buf[4..8] != MAGIC_COOKIE.to_be_bytes()
        || &buf[8..20] != txid
    {
        return None;
    }
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let attrs = buf.get(20..20 + len)?;
    let mut i = 0;
    let mut mapped = None;
    while i + 4 <= attrs.len() {
        let ty = u16::from_be_bytes([attrs[i], attrs[i + 1]]);
        let alen = u16::from_be_bytes([attrs[i + 2], attrs[i + 3]]) as usize;
        let v = attrs.get(i + 4..i + 4 + alen)?;
        if (ty == ATTR_XOR_MAPPED || ty == ATTR_MAPPED) && alen >= 8 && v[1] == 0x01 {
            let mut port = u16::from_be_bytes([v[2], v[3]]);
            let mut ip = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
            if ty == ATTR_XOR_MAPPED {
                port ^= (MAGIC_COOKIE >> 16) as u16;
                ip ^= MAGIC_COOKIE;
                return Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port)));
            }
            mapped.get_or_insert(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(ip), port)));
        }
        i += 4 + alen.div_ceil(4) * 4;
    }
    mapped
}

/// Sends Binding Requests from `socket` to every server (retrying a few times within
/// `timeout`) and collects XOR-MAPPED-ADDRESS answers. Blocking; restores the socket's
/// previous non-blocking/timeout state before returning.
pub(crate) fn query(socket: &UdpSocket, servers: &[&str], timeout: Duration) -> StunResult {
    let mut txid = [0u8; 12];
    let _ = getrandom::fill(&mut txid);
    let req = encode_request(&txid);
    let targets: Vec<SocketAddr> = servers
        .iter()
        .filter_map(|s| s.to_socket_addrs().ok()?.find(SocketAddr::is_ipv4))
        .collect();
    let mut answers: Vec<Option<SocketAddr>> = vec![None; targets.len()];
    if targets.is_empty() {
        return StunResult::default();
    }

    let prev_timeout = socket.read_timeout().ok().flatten();
    // std has no non-blocking getter. A socket without a read timeout is taken to be a
    // non-blocking game socket and is made non-blocking again afterwards.
    let _ = socket.set_nonblocking(false);
    let start = Instant::now();
    let mut next_send = start;
    let mut buf = [0u8; 1500];
    while answers.iter().any(Option::is_none) {
        let now = Instant::now();
        if now >= start + timeout {
            break;
        }
        if now >= next_send {
            for (t, a) in targets.iter().zip(&answers) {
                if a.is_none() {
                    let _ = socket.send_to(&req, t);
                }
            }
            next_send = now + Duration::from_millis(250);
        }
        let wait = (next_send.min(start + timeout) - now).max(Duration::from_millis(1));
        let _ = socket.set_read_timeout(Some(wait));
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            if let Some(addr) = parse_response(&buf[..n], &txid) {
                if let Some(i) = targets.iter().position(|t| *t == from) {
                    answers[i] = Some(addr);
                }
            }
        }
    }
    let _ = socket.set_read_timeout(prev_timeout);
    if prev_timeout.is_none() {
        let _ = socket.set_nonblocking(true);
    }

    let got: Vec<SocketAddr> = answers.into_iter().flatten().collect();
    let symmetric = got.windows(2).any(|w| w[0].port() != w[1].port());
    let mut public = Vec::new();
    for a in got {
        if !public.contains(&a) {
            public.push(a);
        }
    }
    StunResult { public, symmetric }
}

/// Non-loopback IPv4 addresses of this machine's interfaces (LAN candidates).
pub(crate) fn local_ipv4() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    for probe in ["8.8.8.8:53", "192.168.0.1:9", "10.0.0.1:9"] {
        let Ok(s) = UdpSocket::bind("0.0.0.0:0") else { continue };
        if s.connect(probe).is_err() {
            continue;
        }
        if let Ok(SocketAddr::V4(a)) = s.local_addr() {
            let ip = *a.ip();
            if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() && !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stun_request_encoding() {
        let tx = [7u8; 12];
        let r = encode_request(&tx);
        assert_eq!(&r[0..4], &[0, 1, 0, 0]);
        assert_eq!(&r[4..8], &[0x21, 0x12, 0xA4, 0x42]);
        assert_eq!(&r[8..], &tx);
    }

    #[test]
    fn stun_parse_xor_mapped() {
        let tx = [9u8; 12];
        let ip = u32::from(Ipv4Addr::new(203, 0, 113, 5)) ^ MAGIC_COOKIE;
        let port = 40000u16 ^ 0x2112;
        let mut b = Vec::new();
        b.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
        b.extend_from_slice(&12u16.to_be_bytes());
        b.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        b.extend_from_slice(&tx);
        b.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
        b.extend_from_slice(&8u16.to_be_bytes());
        b.extend_from_slice(&[0, 1]);
        b.extend_from_slice(&port.to_be_bytes());
        b.extend_from_slice(&ip.to_be_bytes());
        assert_eq!(parse_response(&b, &tx), Some("203.0.113.5:40000".parse().unwrap()));
        assert_eq!(parse_response(&b, &[0u8; 12]), None);
    }

    #[test]
    fn stun_live_query() {
        let s = UdpSocket::bind("0.0.0.0:0").unwrap();
        s.set_nonblocking(true).unwrap();
        let r = query(&s, STUN_SERVERS, Duration::from_secs(2));
        println!("stun live: {r:?}; local: {:?}", local_ipv4());
        for srv in STUN_SERVERS {
            println!("stun {srv}: {:?}", query(&s, &[srv], Duration::from_secs(2)).public);
        }
    }

    /// Fake STUN server on localhost answering every request with `port` mapped.
    fn fake_server(port: u16) -> String {
        let srv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = srv.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = srv.recv_from(&mut buf) {
                if n < 20 {
                    continue;
                }
                let mut b = Vec::new();
                b.extend_from_slice(&BINDING_SUCCESS.to_be_bytes());
                b.extend_from_slice(&12u16.to_be_bytes());
                b.extend_from_slice(&buf[4..20]);
                b.extend_from_slice(&ATTR_XOR_MAPPED.to_be_bytes());
                b.extend_from_slice(&8u16.to_be_bytes());
                b.extend_from_slice(&[0, 1]);
                b.extend_from_slice(&(port ^ 0x2112).to_be_bytes());
                b.extend_from_slice(&(u32::from(Ipv4Addr::new(203, 0, 113, 5)) ^ MAGIC_COOKIE).to_be_bytes());
                let _ = srv.send_to(&b, from);
            }
        });
        addr
    }

    #[test]
    fn stun_query_detects_symmetric() {
        let (a, b, c) = (fake_server(1000), fake_server(2000), fake_server(1000));
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_nonblocking(true).unwrap();
        let r = query(&s, &[&a, &b], Duration::from_secs(2));
        assert!(r.symmetric);
        assert_eq!(r.public.len(), 2);
        let r = query(&s, &[&a, &c], Duration::from_secs(2));
        assert!(!r.symmetric);
        assert_eq!(r.public, vec!["203.0.113.5:1000".parse().unwrap()]);
        // Restored to non-blocking.
        let mut buf = [0u8; 4];
        assert_eq!(s.recv_from(&mut buf).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }
}
