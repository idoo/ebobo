//! Session tests: lossy-network determinism, timeout disconnect, BYE.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ebobo_core::Nes;

use super::*;

const PROBE_FRAME: i32 = 900;

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// True with probability `percent` / 100.
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// `None` (after a notice) when the test ROMs were not fetched.
fn nes() -> Option<Nes> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ebobo-core/tests/roms/nestest/nestest.nes");
    let Ok(rom) = std::fs::read(path) else {
        println!("test ROMs missing: run scripts/fetch-test-roms.sh");
        return None;
    };
    Some(Nes::new(&rom, None).expect("load nestest"))
}

fn bind() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").expect("bind")
}

/// Lossy link emulator: one thread, a delay queue, 0-80 ms delay, 10 % loss, 2 % duplicates.
struct Proxy {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Builds `n` transports whose routes all go through a lossy proxy. For each ordered pair
/// (a, b) the proxy owns socket `s[a][b]`, which is `a`'s route to `b`; whatever arrives there
/// from `a` is re-sent to `b` from `s[b][a]`, the address `b` knows as `a`.
fn lossy_mesh(n: usize, seed: u64) -> (Vec<Transport>, Proxy) {
    let peers: Vec<UdpSocket> = (0..n).map(|_| bind()).collect();
    let peer_addrs: Vec<SocketAddr> = peers.iter().map(|s| s.local_addr().unwrap()).collect();
    let mut sock = vec![vec![None; n]; n];
    let mut links = Vec::new(); // (in socket index, out socket index, destination)
    let mut sockets: Vec<UdpSocket> = Vec::new();
    for a in 0..n {
        for b in 0..n {
            if a != b {
                let s = bind();
                s.set_nonblocking(true).unwrap();
                sock[a][b] = Some(sockets.len());
                sockets.push(s);
            }
        }
    }
    for a in 0..n {
        for b in 0..n {
            if a != b {
                links.push((sock[a][b].unwrap(), sock[b][a].unwrap(), peer_addrs[b]));
            }
        }
    }
    let transports = peers
        .into_iter()
        .enumerate()
        .map(|(a, udp)| {
            let routes: Vec<(PeerId, SocketAddr)> = (0..n)
                .filter(|&b| b != a)
                .map(|b| (PeerId(b as u8), sockets[sock[a][b].unwrap()].local_addr().unwrap()))
                .collect();
            Transport::udp(udp, PeerId(a as u8), &routes)
        })
        .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut rng = XorShift(seed | 1);
        let mut queue: Vec<(Instant, usize, SocketAddr, Vec<u8>)> = Vec::new();
        let mut buf = [0u8; 2048];
        while !stop2.load(Ordering::Relaxed) {
            for &(inp, out, dest) in &links {
                while let Ok((len, _)) = sockets[inp].recv_from(&mut buf) {
                    if rng.chance(10) {
                        continue;
                    }
                    let copies = if rng.chance(2) { 2 } else { 1 };
                    for _ in 0..copies {
                        let due = Instant::now() + Duration::from_millis(rng.next() % 81);
                        queue.push((due, out, dest, buf[..len].to_vec()));
                    }
                }
            }
            let now = Instant::now();
            queue.retain(|(due, out, dest, bytes)| {
                if *due <= now {
                    let _ = sockets[*out].send_to(bytes, dest);
                    false
                } else {
                    true
                }
            });
            std::thread::sleep(Duration::from_micros(300));
        }
    });
    (transports, Proxy { stop, thread: Some(thread) })
}

/// Runs `n` peers through the lossy proxy until every peer advanced past `probe` and confirmed
/// it; asserts equal state hashes at `probe`, no desync events and bounded queues/lag.
fn run_lossy(n: usize, seed: u64, probe: i32, timeout: Duration) {
    let Some(first) = nes() else { return };
    let (transports, _proxy) = lossy_mesh(n, seed);
    let slots: Vec<PeerId> = (0..n as u8).map(PeerId).collect();
    let mut sessions: Vec<NetSession> = transports
        .into_iter()
        .map(|t| {
            let mut s = NetSession::new(t, &slots, 2, 60).expect("session");
            s.probe = (probe, None);
            s
        })
        .collect();
    let mut emus: Vec<Nes> = std::iter::once(first).chain((1..n).map(|_| nes().unwrap())).collect();
    let mut rngs: Vec<XorShift> = (0..n).map(|i| XorShift(seed ^ (0x9E37_79B9 * (i as u64 + 1)))).collect();
    let mut advanced = vec![0u32; n];
    let mut rollbacks = vec![0u64; n];
    let mut desyncs = Vec::new();
    let mut max_pending = 0usize;
    let mut max_behind = 0i32;
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        let done = advanced.iter().all(|&a| a as i32 >= probe)
            && sessions.iter().all(|s| s.ggrs.confirmed_frame() > probe);
        if done {
            break;
        }
        assert!(Instant::now() < deadline, "timed out: advanced {advanced:?}");
        for i in 0..n {
            let s = &mut sessions[i];
            s.poll();
            let input = if advanced[i] as i32 >= probe {
                0
            } else if i == 0 && advanced[i] == 300 {
                INPUT_RESET
            } else {
                (rngs[i].next() & 0xFF) as u16
            };
            let out = s.advance(&mut emus[i], input).expect("advance");
            if out.advanced {
                advanced[i] += 1;
            }
            rollbacks[i] += out.rollback_frames as u64;
            max_pending = max_pending.max(s.pending.len());
            if advanced[i] % 64 == 0 {
                for p in s.stats().peers {
                    max_behind = max_behind.max(p.local_frames_behind.abs()).max(p.remote_frames_behind.abs());
                }
            }
            for e in s.events() {
                if let NetEvent::Desync { frame } = e {
                    desyncs.push((i, frame));
                }
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    let hashes: Vec<Option<u64>> = sessions.iter().map(|s| s.probe.1).collect();
    println!(
        "{n} players: advanced {advanced:?} in {:?}, rollback frames {rollbacks:?}, hash@{probe} {hashes:x?}, \
         max pending events {max_pending}, max frames behind {max_behind}",
        started.elapsed()
    );
    assert!(desyncs.is_empty(), "desync events: {desyncs:?}");
    assert!(hashes[0].is_some(), "frame {probe} never saved");
    assert!(hashes.iter().all(|h| *h == hashes[0]), "hashes differ: {hashes:x?}");
    assert!(advanced.iter().all(|&a| a as i32 >= probe));
    assert!(max_pending <= 64, "pending events grew to {max_pending}");
    // GGRS frames_behind is a time-sync estimate, not the prediction window: under 80 ms jitter
    // it reaches ~24 over 5000 frames. Bound it at one second (60 frames): anything above means drift.
    assert!(max_behind <= 60, "a peer fell {max_behind} frames behind");
}

#[test]
fn session_lossy_determinism_2p() {
    run_lossy(2, 0xEB0B0_2, PROBE_FRAME, Duration::from_secs(120));
}

#[test]
fn session_lossy_determinism_4p() {
    run_lossy(4, 0xEB0B0_4, PROBE_FRAME, Duration::from_secs(120));
}

/// Soak: 4 peers, 10 % loss, 0-80 ms jitter, 2 % duplicates for 5000 frames.
/// `cargo test --release -p ebobo-net session_lossy_soak_4p -- --ignored --nocapture`
#[test]
#[ignore]
fn session_lossy_soak_4p() {
    run_lossy(4, 0x50A4_50A4, 5000, Duration::from_secs(600));
}

/// Two directly connected sessions with a 1 s disconnect timeout (notify after 300 ms).
fn direct_pair() -> (NetSession, NetSession) {
    let (a, b) = (bind(), bind());
    let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
    let slots = [PeerId(0), PeerId(1)];
    let t = Duration::from_secs(1);
    let notify = Duration::from_millis(300);
    let sa = NetSession::with_timeouts(Transport::udp(a, PeerId(0), &[(PeerId(1), ba)]), &slots, 2, 60, t, notify).unwrap();
    let sb = NetSession::with_timeouts(Transport::udp(b, PeerId(1), &[(PeerId(0), aa)]), &slots, 2, 60, t, notify).unwrap();
    (sa, sb)
}

/// Drives both until running and `frames` frames advanced on each.
fn warm_up(sa: &mut NetSession, sb: &mut NetSession, na: &mut Nes, nb: &mut Nes, frames: u32) {
    let (mut fa, mut fb) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(20);
    while fa < frames || fb < frames {
        assert!(Instant::now() < deadline, "warm-up timed out");
        sa.poll();
        sb.poll();
        fa += sa.advance(na, 1).unwrap().advanced as u32;
        fb += sb.advance(nb, 2).unwrap().advanced as u32;
        let _ = (sa.events(), sb.events());
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn session_disconnect_on_timeout() {
    let (mut sa, mut sb) = direct_pair();
    let Some(mut na) = nes() else { return };
    let mut nb = nes().unwrap();
    warm_up(&mut sa, &mut sb, &mut na, &mut nb, 60);
    // `sb` goes silent.
    let (mut interrupted, mut disconnected) = (false, false);
    let mut after = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while after < 60 {
        assert!(Instant::now() < deadline, "interrupted {interrupted}, disconnected {disconnected}, after {after}");
        sa.poll();
        let out = sa.advance(&mut na, 3).unwrap();
        for e in sa.events() {
            match e {
                NetEvent::Interrupted { peer, .. } if peer == PeerId(1) => interrupted = true,
                NetEvent::Disconnected { peer } if peer == PeerId(1) => {
                    assert!(interrupted, "disconnected before interrupted");
                    disconnected = true;
                }
                _ => {}
            }
        }
        if disconnected && out.advanced {
            after += 1;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(interrupted && disconnected);
}

#[test]
fn session_bye_disconnects_quickly() {
    let (mut sa, mut sb) = direct_pair();
    let Some(mut na) = nes() else { return };
    let mut nb = nes().unwrap();
    warm_up(&mut sa, &mut sb, &mut na, &mut nb, 30);
    sb.leave();
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(1), "no Disconnected within a second");
        sa.poll();
        let _ = sa.advance(&mut na, 0).unwrap();
        if sa.events().contains(&NetEvent::Disconnected { peer: PeerId(1) }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("BYE noticed after {:?}", start.elapsed());
}
