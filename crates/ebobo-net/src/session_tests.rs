//! Session tests: lossy-network determinism, bad-network profiles (mobile, far, asymmetric,
//! blackout), timeout disconnect, BYE.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

    /// True with probability `permille` / 1000.
    fn permille(&mut self, permille: u64) -> bool {
        self.next() % 1000 < permille
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

/// Emulated network between every pair of peers. Every random choice comes from per-link
/// generators seeded from the run seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Profile {
    /// 0-80 ms uniform delay without FIFO order, 10 % loss, 2 % duplicates.
    Lossy,
    /// 60 ± 30 ms one way, bursty Gilbert-Elliott loss, 1 % reordering.
    Mobile,
    /// 150 ± 20 ms one way, 2 % loss.
    Far,
    /// 15 ms from the lower peer id to the higher, 180 ms back, 3 % loss.
    Asym,
    /// `Mobile`, and every link of the last peer is dead for `BLACKOUT` mid-run.
    Blackout,
}

/// How long `Profile::Blackout` cuts its victim off.
const BLACKOUT: Duration = Duration::from_secs(4);
/// A reordered packet is held this long past the FIFO order, so later ones overtake it.
const REORDER_HOLD: Duration = Duration::from_millis(17);

/// One direction of a link.
#[derive(Clone, Copy)]
struct LinkModel {
    base_ms: u64,
    /// Delay is uniform in `base_ms ± jitter_ms`.
    jitter_ms: u64,
    /// Loss in ‰ (in the good state when `bursty`).
    loss: u64,
    /// Gilbert-Elliott: good→bad 1 %, bad→good 30 % per packet, 50 % loss while bad.
    bursty: bool,
    /// Duplicates in ‰.
    dup: u64,
    /// Reordered packets in ‰ (held back by `REORDER_HOLD`).
    reorder: u64,
    /// Jitter keeps the sending order (real queues); false = every packet independent.
    fifo: bool,
}

impl Profile {
    const MEASURED: [Profile; 4] = [Profile::Mobile, Profile::Far, Profile::Asym, Profile::Blackout];

    fn name(self) -> &'static str {
        match self {
            Profile::Lossy => "lossy",
            Profile::Mobile => "mobile",
            Profile::Far => "far",
            Profile::Asym => "asym",
            Profile::Blackout => "blackout",
        }
    }

    fn link(self, from: usize, to: usize) -> LinkModel {
        let m = LinkModel { base_ms: 0, jitter_ms: 0, loss: 0, bursty: false, dup: 0, reorder: 0, fifo: true };
        match self {
            Profile::Lossy => LinkModel { base_ms: 40, jitter_ms: 40, loss: 100, dup: 20, fifo: false, ..m },
            Profile::Mobile | Profile::Blackout => {
                LinkModel { base_ms: 60, jitter_ms: 30, loss: 5, bursty: true, reorder: 10, ..m }
            }
            Profile::Far => LinkModel { base_ms: 150, jitter_ms: 20, loss: 20, ..m },
            Profile::Asym => LinkModel { base_ms: if from < to { 15 } else { 180 }, jitter_ms: 5, loss: 30, ..m },
        }
    }

    /// Peer whose links `Proxy::blackout` cuts.
    fn victim(self, n: usize) -> Option<usize> {
        (self == Profile::Blackout).then_some(n - 1)
    }
}

/// Link emulator: one thread, a delay queue, per-direction `LinkModel`s.
struct Proxy {
    stop: Arc<AtomicBool>,
    /// While set, every link of the profile's victim drops everything.
    blackout: Arc<AtomicBool>,
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

struct Link {
    inp: usize,
    out: usize,
    dest: SocketAddr,
    from: usize,
    to: usize,
    model: LinkModel,
    rng: XorShift,
    bad: bool,
    last_due: Instant,
}

/// Builds `n` transports whose routes all go through an emulated `profile` network. For each
/// ordered pair (a, b) the proxy owns socket `s[a][b]`, which is `a`'s route to `b`; whatever
/// arrives there from `a` is re-sent to `b` from `s[b][a]`, the address `b` knows as `a`.
fn lossy_mesh(n: usize, seed: u64, profile: Profile) -> (Vec<crate::Link>, Proxy) {
    let peers: Vec<UdpSocket> = (0..n).map(|_| bind()).collect();
    let peer_addrs: Vec<SocketAddr> = peers.iter().map(|s| s.local_addr().unwrap()).collect();
    let mut sock = vec![vec![None; n]; n];
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
    let mut links = Vec::new();
    for a in 0..n {
        for b in 0..n {
            if a != b {
                links.push(Link {
                    inp: sock[a][b].unwrap(),
                    out: sock[b][a].unwrap(),
                    dest: peer_addrs[b],
                    from: a,
                    to: b,
                    model: profile.link(a, b),
                    rng: XorShift((seed ^ (0x9E37_79B9_7F4A_7C15u64.wrapping_mul((a * 8 + b + 1) as u64))) | 1),
                    bad: false,
                    last_due: Instant::now(),
                });
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
            crate::Link::udp(udp, PeerId(a as u8), &routes)
        })
        .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let blackout = Arc::new(AtomicBool::new(false));
    let (stop2, blackout2) = (stop.clone(), blackout.clone());
    let victim = profile.victim(n);
    let thread = std::thread::spawn(move || {
        let mut queue: Vec<(Instant, usize, SocketAddr, Vec<u8>)> = Vec::new();
        let mut buf = [0u8; 2048];
        while !stop2.load(Ordering::Relaxed) {
            let dark = blackout2.load(Ordering::Relaxed);
            for l in &mut links {
                while let Ok((len, _)) = sockets[l.inp].recv_from(&mut buf) {
                    if dark && victim.is_some_and(|v| l.from == v || l.to == v) {
                        continue;
                    }
                    let m = l.model;
                    if m.bursty {
                        l.bad = if l.bad { !l.rng.permille(300) } else { l.rng.permille(10) };
                    }
                    if l.rng.permille(if l.bad { 500 } else { m.loss }) {
                        continue;
                    }
                    let copies = if l.rng.permille(m.dup) { 2 } else { 1 };
                    for _ in 0..copies {
                        let ms = m.base_ms - m.jitter_ms + l.rng.next() % (2 * m.jitter_ms + 1);
                        let mut due = Instant::now() + Duration::from_millis(ms);
                        if m.fifo {
                            due = due.max(l.last_due);
                            if l.rng.permille(m.reorder) {
                                due += REORDER_HOLD;
                            } else {
                                l.last_due = due;
                            }
                        }
                        queue.push((due, l.out, l.dest, buf[..len].to_vec()));
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
    (transports, Proxy { stop, blackout, thread: Some(thread) })
}

/// Runs `n` peers through the lossy proxy until every peer advanced past `probe` and confirmed
/// it; asserts equal state hashes at `probe`, no desync events and bounded queues/lag.
fn run_lossy(n: usize, seed: u64, probe: i32, timeout: Duration) {
    let Some(first) = nes() else { return };
    let (transports, _proxy) = lossy_mesh(n, seed, Profile::Lossy);
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

/// Totals of one paced run (all peers together).
#[derive(Clone, Copy, Debug, Default)]
struct RunStats {
    /// `advance` calls while running.
    ticks: u64,
    /// Ticks that produced no frame: GGRS hit the prediction window.
    stalls: u64,
    /// Longest run of consecutive stalled ticks on one peer.
    max_stall_streak: u32,
    advanced: u64,
    rollback_frames: u64,
    max_rollback: u32,
    /// Frames advanced after the blackout ended (`Profile::Blackout` only).
    after_blackout: u64,
    desyncs: u32,
    interrupted: u32,
    disconnected: u32,
}

/// Plays `n` peers on `profile` for `secs` after all are synchronized, paced like the frontend:
/// one `advance` per 60 fps tick, the tick stretched 2 % per frame ahead (max 10 %), frame debt
/// above 3 ticks dropped. Inputs are random every frame, so nearly every late remote frame is a
/// misprediction (worst case for rollback). Blackout starts at 30 % of `secs`.
fn run_paced(profile: Profile, n: usize, delay: usize, max_prediction: usize, secs: f64, seed: u64) -> Option<RunStats> {
    let first = nes()?;
    let (transports, proxy) = lossy_mesh(n, seed, profile);
    let slots: Vec<PeerId> = (0..n as u8).map(PeerId).collect();
    let mut sessions: Vec<NetSession> = transports
        .into_iter()
        .map(|t| {
            NetSession::with_tuning(t, &slots, delay, 60, max_prediction, DISCONNECT_TIMEOUT, DISCONNECT_NOTIFY)
                .expect("session")
        })
        .collect();
    let mut emus: Vec<Nes> = std::iter::once(first).chain((1..n).map(|_| nes().unwrap())).collect();
    let mut rngs: Vec<XorShift> = (0..n).map(|i| XorShift(seed ^ (0x9E37_79B9 * (i as u64 + 1)))).collect();
    let mut st = RunStats::default();
    let mut streak = vec![0u32; n];
    let mut next = vec![Instant::now(); n];
    let (dark_from, dark_to) = (secs * 0.3, secs * 0.3 + BLACKOUT.as_secs_f64());
    let sync_deadline = Instant::now() + Duration::from_secs(30);
    let mut start: Option<Instant> = None;
    loop {
        let now = Instant::now();
        let t = match start {
            Some(t0) => (now - t0).as_secs_f64(),
            None if sessions.iter().all(|s| s.is_running()) => {
                start = Some(now);
                next.iter_mut().for_each(|d| *d = now);
                0.0
            }
            None => {
                assert!(now < sync_deadline, "{} {n}p: synchronization timed out", profile.name());
                -1.0
            }
        };
        if t >= secs {
            break;
        }
        proxy.blackout.store(profile == Profile::Blackout && t >= dark_from && t < dark_to, Ordering::Relaxed);
        for i in 0..n {
            let s = &mut sessions[i];
            s.poll();
            if t >= 0.0 && now >= next[i] {
                let running = s.is_running();
                let out = s.advance(&mut emus[i], (rngs[i].next() & 0xFF) as u16).expect("advance");
                if running {
                    st.ticks += 1;
                    if out.advanced {
                        streak[i] = 0;
                        st.advanced += 1;
                        st.rollback_frames += out.rollback_frames as u64;
                        st.max_rollback = st.max_rollback.max(out.rollback_frames);
                        if profile == Profile::Blackout && t >= dark_to {
                            st.after_blackout += 1;
                        }
                    } else {
                        st.stalls += 1;
                        streak[i] += 1;
                        st.max_stall_streak = st.max_stall_streak.max(streak[i]);
                    }
                }
                // Mirrors `ebobo::app::logic::{frame_period, next_deadline}`.
                let stretch = (0.02 * s.frames_ahead().max(0) as f64).min(0.10);
                let period = Duration::from_secs_f64((1.0 + stretch) / 60.0);
                let due = next[i] + period;
                next[i] = if now.saturating_duration_since(due) > period * 3 { now } else { due };
            }
            for e in s.events() {
                match e {
                    NetEvent::Desync { .. } => st.desyncs += 1,
                    NetEvent::Interrupted { .. } => st.interrupted += 1,
                    NetEvent::Disconnected { .. } => st.disconnected += 1,
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    Some(st)
}

/// Mean time of one 12-frame rollback on the real core (load, 12 × save + hash + run, the last
/// frame rendered) and the worst of `reps`.
fn rollback_cost(nes: &mut Nes, reps: u32) -> (Duration, Duration) {
    for f in 0..120u32 {
        nes.run_frame([f as u8, 0, 0, 0]);
    }
    nes.clear_audio();
    let snap = nes.snapshot();
    let (mut total, mut worst) = (Duration::ZERO, Duration::ZERO);
    for _ in 0..reps {
        let started = Instant::now();
        nes.restore(&snap);
        for f in 0..12u8 {
            std::hint::black_box((nes.snapshot(), nes.state_hash()));
            nes.set_fast_forward(f < 11);
            nes.run_frame([f.wrapping_mul(37), 0, 0, 0]);
        }
        nes.clear_audio();
        let took = started.elapsed();
        total += took;
        worst = worst.max(took);
    }
    (total / reps, worst)
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Bad-network matrix: profiles × 2/4 peers × input delay 1-6 × max prediction 8/10/12, paced
/// at 60 fps. Prints stalls, rollback depth, desyncs and Interrupted/Disconnected counts.
/// `cargo test --release -p ebobo-net session_profile -- --ignored --nocapture`
/// (`EBOBO_PROFILE_SECS`, default 10, run length; `EBOBO_PROFILE_JOBS`, default 8, parallel runs).
#[test]
#[ignore]
fn session_profile_matrix() {
    let Some(mut core) = nes() else { return };
    let (mean, worst) = rollback_cost(&mut core, 200);
    println!("12-frame rollback on the core: mean {mean:?}, worst {worst:?} (frame budget 16.6 ms)");
    let secs: f64 = env_or("EBOBO_PROFILE_SECS", 10.0);
    let workers: usize = env_or("EBOBO_PROFILE_JOBS", 8);
    let mut jobs = Vec::new();
    for profile in Profile::MEASURED {
        for n in [2, 4] {
            for delay in 1..=6 {
                for pred in [8, 10, 12] {
                    jobs.push((profile, n, delay, pred));
                }
            }
        }
    }
    let total = jobs.len();
    let queue = Arc::new(Mutex::new(jobs.into_iter().enumerate().collect::<Vec<_>>()));
    let results = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let (queue, results) = (queue.clone(), results.clone());
            std::thread::spawn(move || loop {
                let Some((idx, (profile, n, delay, pred))) = queue.lock().unwrap().pop() else { return };
                let seed = 0x5EED_0000 ^ (idx as u64 * 0x1_0001);
                let st = run_paced(profile, n, delay, pred, secs, seed).expect("ROMs present");
                results.lock().unwrap().push((idx, (profile, n, delay, pred), st));
            })
        })
        .collect();
    for h in handles {
        h.join().expect("profile worker panicked");
    }
    let mut rows = std::mem::take(&mut *results.lock().unwrap());
    assert_eq!(rows.len(), total);
    rows.sort_by_key(|r| r.0);
    println!("{secs} s per run; stall % = stalled ticks / ticks while running");
    println!("profile   n delay pred | stall%  max streak | rb mean rb max | desync interr disc");
    let mut desyncs = 0;
    for (_, (profile, n, delay, pred), st) in rows {
        desyncs += st.desyncs;
        println!(
            "{:<9} {n} {delay:>5} {pred:>4} | {:>6.2} {:>11} | {:>7.2} {:>6} | {:>6} {:>6} {:>4}",
            profile.name(),
            100.0 * st.stalls as f64 / st.ticks.max(1) as f64,
            st.max_stall_streak,
            st.rollback_frames as f64 / st.advanced.max(1) as f64,
            st.max_rollback,
            st.desyncs,
            st.interrupted,
            st.disconnected,
        );
    }
    assert_eq!(desyncs, 0, "desyncs under bad-network profiles");
}

/// Bad-internet regression: 2 peers on a mobile network, one cut off for 4 s, finish without a
/// disconnect or desync and keep playing after the blackout.
#[test]
fn session_profile_blackout_2p() {
    let Some(st) = run_paced(Profile::Blackout, 2, 3, MAX_PREDICTION, 8.0, 0xB1AC_0002) else { return };
    println!("blackout 2p: {st:?}");
    assert_eq!(st.disconnected, 0, "a 4 s blackout disconnected a peer");
    assert_eq!(st.desyncs, 0, "desync after the blackout");
    assert!(st.interrupted > 0, "the blackout was never noticed");
    assert!(st.after_blackout >= 60, "play did not resume after the blackout: {st:?}");
}

/// Two directly connected sessions with a 1 s disconnect timeout (notify after 300 ms).
fn direct_pair() -> (NetSession, NetSession) {
    let (a, b) = (bind(), bind());
    let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
    let slots = [PeerId(0), PeerId(1)];
    let t = Duration::from_secs(1);
    let notify = Duration::from_millis(300);
    let p = MAX_PREDICTION;
    let sa = NetSession::with_tuning(crate::Link::udp(a, PeerId(0), &[(PeerId(1), ba)]), &slots, 2, 60, p, t, notify).unwrap();
    let sb = NetSession::with_tuning(crate::Link::udp(b, PeerId(1), &[(PeerId(0), aa)]), &slots, 2, 60, p, t, notify).unwrap();
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
