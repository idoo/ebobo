//! Network self-test for the "Network check" screen. Owner: diagnostics slice.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{random_code, stun};

/// Whole `diagnose` run; the checks run in parallel inside it.
const BUDGET: Duration = Duration::from_millis(5500);
/// One TCP connect / read inside a check.
const IO_TIMEOUT: Duration = Duration::from_secs(4);
const STUN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatKind {
    /// Same public port from every STUN server: hole punching usually works.
    Cone,
    /// Different public ports per destination: direct P2P unlikely, relay will be used.
    Symmetric,
    /// STUN did not answer (UDP blocked or no internet).
    UdpBlocked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetDiag {
    /// Round trip to the signaling server's health endpoint, or the failure text.
    pub server: Result<Duration, String>,
    /// WebSocket upgrade to the signaling server works (room creation path).
    pub websocket: Result<(), String>,
    pub public_addr: Option<SocketAddr>,
    pub local_ip: Option<Ipv4Addr>,
    pub nat: NatKind,
}

/// Signaling server address split from `https://host[:port]` (also `http://`, `ws(s)://`,
/// or a bare host, which means HTTPS).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    tls: bool,
    host: String,
    port: u16,
}

impl Target {
    fn parse(server: &str) -> Option<Target> {
        let s = server.trim();
        let (tls, rest) = if let Some(r) = s.strip_prefix("https://").or_else(|| s.strip_prefix("wss://")) {
            (true, r)
        } else if let Some(r) = s.strip_prefix("http://").or_else(|| s.strip_prefix("ws://")) {
            (false, r)
        } else {
            (true, s)
        };
        let authority = rest.split(['/', '?']).next().unwrap_or("");
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().ok()?),
            _ => (authority, if tls { 443 } else { 80 }),
        };
        (!host.is_empty()).then(|| Target { tls, host: host.to_string(), port })
    }

    fn ws_url(&self, path: &str) -> String {
        let scheme = if self.tls { "wss" } else { "ws" };
        format!("{scheme}://{}:{}{path}", self.host, self.port)
    }
}

enum Check {
    Server(Result<Duration, String>),
    Websocket(Result<(), String>),
    Stun(Option<SocketAddr>, NatKind),
}

/// Runs every check (blocking, at most ~6 s). Call from a background thread.
pub fn diagnose(server: &str) -> NetDiag {
    let deadline = Instant::now() + BUDGET;
    let (tx, rx) = mpsc::channel();
    match Target::parse(server) {
        Some(target) => {
            let http = target.clone();
            spawn(&tx, "ebobo-diag-http", move || Check::Server(http_check(&http)));
            spawn(&tx, "ebobo-diag-ws", move || Check::Websocket(ws_check(&target)));
        }
        None => {
            let _ = tx.send(Check::Server(Err("no server address".into())));
            let _ = tx.send(Check::Websocket(Err("no server address".into())));
        }
    }
    spawn(&tx, "ebobo-diag-stun", stun_check);
    drop(tx);

    let local_ip = stun::local_ipv4().into_iter().next();
    let mut diag = NetDiag {
        server: Err("timeout".into()),
        websocket: Err("timeout".into()),
        public_addr: None,
        local_ip,
        nat: NatKind::UdpBlocked,
    };
    // A check that panicked drops its sender: its field keeps the failure value.
    while let Ok(check) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        match check {
            Check::Server(r) => diag.server = r,
            Check::Websocket(r) => diag.websocket = r,
            Check::Stun(addr, nat) => {
                diag.public_addr = addr;
                diag.nat = nat;
            }
        }
    }
    diag
}

/// Runs `f` on a named thread and sends its result; a thread that cannot start is skipped.
fn spawn(tx: &mpsc::Sender<Check>, name: &str, f: impl FnOnce() -> Check + Send + 'static) {
    let tx = tx.clone();
    let _ = thread::Builder::new().name(name.into()).spawn(move || {
        let _ = tx.send(f());
    });
}

fn tcp_connect(target: &Target) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> =
        (target.host.as_str(), target.port).to_socket_addrs().map_err(|e| format!("DNS: {e}"))?.collect();
    let mut last = format!("{}: no address", target.host);
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, IO_TIMEOUT) {
            Ok(tcp) => {
                let _ = tcp.set_read_timeout(Some(IO_TIMEOUT));
                let _ = tcp.set_write_timeout(Some(IO_TIMEOUT));
                let _ = tcp.set_nodelay(true);
                return Ok(tcp);
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

/// `GET /` over HTTPS (or HTTP); Ok = 2xx status, timed from connect to the status line.
fn http_check(target: &Target) -> Result<Duration, String> {
    let started = Instant::now();
    let tcp = tcp_connect(target)?;
    let status = if target.tls {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(target.host.clone()).map_err(|e| e.to_string())?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
        http_get_status(&mut rustls::StreamOwned::new(conn, tcp), &target.host)?
    } else {
        http_get_status(&mut { tcp }, &target.host)?
    };
    if (200..300).contains(&status) {
        Ok(started.elapsed())
    } else {
        Err(format!("HTTP {status}"))
    }
}

/// Sends `GET /` and returns the response status code.
fn http_get_status(stream: &mut impl ReadWrite, host: &str) -> Result<u16, String> {
    let req = format!(
        "GET / HTTP/1.1\r\nHost: {host}\r\nUser-Agent: ebobo/{}\r\nConnection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    stream.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut head = Vec::new();
    let mut buf = [0u8; 512];
    while !head.contains(&b'\n') && head.len() < 4096 {
        let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line = String::from_utf8_lossy(&head);
    let line = line.lines().next().unwrap_or("");
    // "HTTP/1.1 200 OK"
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .filter(|_| line.starts_with("HTTP/"))
        .ok_or_else(|| format!("bad response: {}", line.chars().take(40).collect::<String>()))
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// Opens a host socket in a fresh random room, waits for `welcome`, closes (the room goes
/// away with its only peer).
fn ws_check(target: &Target) -> Result<(), String> {
    // Pin ring even if another dependency ever enables a second rustls provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = target.ws_url(&format!("/v1/room/{}?role=host&name=diag", random_code()));
    let tcp = tcp_connect(target)?;
    let mut ws = match tungstenite::client_tls(url.as_str(), tcp) {
        Ok((ws, _)) => ws,
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp))) => {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        Err(tungstenite::HandshakeError::Failure(e)) => return Err(e.to_string()),
        Err(tungstenite::HandshakeError::Interrupted(_)) => return Err("handshake interrupted".into()),
    };
    let result = loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                let kind = serde_json::from_str::<Value>(t.as_str())
                    .ok()
                    .and_then(|v| v.get("t").and_then(Value::as_str).map(str::to_string));
                if kind.as_deref() == Some("welcome") {
                    break Ok(());
                }
            }
            Ok(tungstenite::Message::Close(_)) => break Err("closed by server".into()),
            Ok(_) => {}
            Err(e) => break Err(e.to_string()),
        }
    };
    let _ = ws.close(None);
    let _ = ws.flush();
    result
}

fn stun_check() -> Check {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return Check::Stun(None, NatKind::UdpBlocked);
    };
    let _ = socket.set_read_timeout(Some(STUN_TIMEOUT));
    let res = stun::query(&socket, stun::STUN_SERVERS, STUN_TIMEOUT);
    let nat = if res.public.is_empty() {
        NatKind::UdpBlocked
    } else if res.symmetric {
        NatKind::Symmetric
    } else {
        NatKind::Cone
    };
    Check::Stun(res.public.first().copied(), nat)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE_SERVER: &str = "https://ebobo-signal.mc-se.workers.dev";

    #[test]
    fn diag_target_parse() {
        let t = |s| Target::parse(s);
        assert_eq!(t("https://a.dev"), Some(Target { tls: true, host: "a.dev".into(), port: 443 }));
        assert_eq!(t("http://a.dev:8787/"), Some(Target { tls: false, host: "a.dev".into(), port: 8787 }));
        assert_eq!(t("a.dev"), Some(Target { tls: true, host: "a.dev".into(), port: 443 }));
        assert_eq!(t("wss://a.dev/x?y"), Some(Target { tls: true, host: "a.dev".into(), port: 443 }));
        assert_eq!(t("  "), None);
        assert_eq!(t("https://"), None);
        assert_eq!(t("https://a.dev").unwrap().ws_url("/v1/room/ABCDE"), "wss://a.dev:443/v1/room/ABCDE");
    }

    /// A dead server fails quickly and within the budget, without panicking.
    #[test]
    fn diag_dead_server_reports_errors() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let started = Instant::now();
        let d = diagnose(&format!("http://127.0.0.1:{port}"));
        assert!(started.elapsed() < Duration::from_millis(6500), "took {:?}", started.elapsed());
        assert!(d.server.is_err(), "{d:?}");
        assert!(d.websocket.is_err(), "{d:?}");
        let d = diagnose("");
        assert!(d.server.is_err() && d.websocket.is_err());
    }

    #[test]
    #[ignore]
    fn diag_live_server() {
        let started = Instant::now();
        let d = diagnose(LIVE_SERVER);
        eprintln!("{d:?} in {:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(d.server.is_ok(), "{d:?}");
        assert_eq!(d.websocket, Ok(()));
        assert!(d.local_ip.is_some());
        assert_ne!(d.nat, NatKind::UdpBlocked, "{d:?}");
        assert!(d.public_addr.is_some());
    }
}
