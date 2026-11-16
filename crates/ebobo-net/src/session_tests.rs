//! Session tests: lossy-network determinism, bad-network profiles (mobile, far, asymmetric,
//! blackout), timeout disconnect, BYE.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ebobo_console::Console;

use super::*;

const PROBE_FRAME: i32 = 900;
/// Random netplay inputs: pad bits plus both turbo bits (a wrong turbo phase after a rollback
/// then shows up as a desync); never reset.
const RANDOM_INPUT: u64 = 0x6_00FF;
/// Delays `run_lossy` forces in turn when `shuffle_delay` is set: rises and falls of 1-5 frames.
const DELAY_CYCLE: [usize; 6] = [4, 1, 6, 2, 5, 3];
/// Advanced frames between two forced delay changes on one peer.
const DELAY_EVERY: u32 = 40;

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
fn nes() -> Option<Console> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ebobo-core/tests/roms/nestest/nestest.nes");
    let Ok(rom) = std::fs::read(path) else {
        println!("test ROMs missing: run scripts/fetch-test-roms.sh");
        return None;
    };
    Some(Console::new(&rom, None).expect("load nestest"))
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
    /// A good link (10 ± 3 ms) that becomes `Mobile` mid-run (`Proxy::degraded`).
    Shift,
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
            Profile::Shift => "shift",
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
            Profile::Shift => LinkModel { base_ms: 10, jitter_ms: 3, ..m },
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
    /// While set, every link behaves like `Profile::Mobile` (`Profile::Shift`).
    degraded: Arc<AtomicBool>,
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
    let degraded = Arc::new(AtomicBool::new(false));
    let (stop2, blackout2, degraded2) = (stop.clone(), blackout.clone(), degraded.clone());
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
                    let m = if degraded2.load(Ordering::Relaxed) { Profile::Mobile.link(l.from, l.to) } else { l.model };
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
    (transports, Proxy { stop, blackout, degraded, thread: Some(thread) })
}

/// Runs `n` peers through the lossy proxy until every peer advanced past `probe` and confirmed
/// it; asserts equal state hashes at `probe`, no desync events and bounded queues/lag.
/// `shuffle_delay`: every peer's input delay is forced through `DELAY_CYCLE` every
/// `DELAY_EVERY` frames, peers out of step with each other.
fn run_lossy(n: usize, seed: u64, probe: i32, timeout: Duration, shuffle_delay: bool) {
    let Some(first) = nes() else { return };
    let (transports, _proxy) = lossy_mesh(n, seed, Profile::Lossy);
    let slots: Vec<PeerId> = (0..n as u8).map(PeerId).collect();
    let mut sessions: Vec<NetSession> = transports
        .into_iter()
        .map(|t| {
            let mut s = NetSession::new(t, &slots, 2, false, 60, None, SavePolicy::Everyone).expect("session");
            s.probe = (probe, None);
            s
        })
        .collect();
    let mut emus: Vec<Console> = std::iter::once(first).chain((1..n).map(|_| nes().unwrap())).collect();
    let mut rngs: Vec<XorShift> = (0..n).map(|i| XorShift(seed ^ (0x9E37_79B9 * (i as u64 + 1)))).collect();
    let mut advanced = vec![0u32; n];
    let mut rollbacks = vec![0u64; n];
    let mut desyncs = Vec::new();
    let mut max_pending = 0usize;
    let mut max_behind = 0i32;
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        // Past the probe: the state of frame `probe` is saved by the call that simulates it.
        let done = advanced.iter().all(|&a| a as i32 > probe)
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
                (rngs[i].next() & RANDOM_INPUT) as u32
            };
            let out = s.advance(&mut emus[i], &[input]).expect("advance");
            if out.advanced {
                advanced[i] += 1;
                if shuffle_delay && advanced[i] % DELAY_EVERY == 0 {
                    s.force_delay = Some(DELAY_CYCLE[((advanced[i] / DELAY_EVERY) as usize + i) % DELAY_CYCLE.len()]);
                }
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
    run_lossy(2, 0xEB0B0_2, PROBE_FRAME, Duration::from_secs(120), false);
}

#[test]
fn session_lossy_determinism_4p() {
    run_lossy(4, 0xEB0B0_4, PROBE_FRAME, Duration::from_secs(120), false);
}

/// Input delay changes mid-game (fill frames on a rise, dropped inputs on a fall) under loss,
/// with random held and turbo input: no desync, every peer keeps advancing, equal hashes.
#[test]
fn session_delay_changes_keep_sync_2p() {
    run_lossy(2, 0xDE1A_0002, PROBE_FRAME, Duration::from_secs(120), true);
}

#[test]
fn session_delay_changes_keep_sync_4p() {
    run_lossy(4, 0xDE1A_0004, PROBE_FRAME, Duration::from_secs(120), true);
}

/// Soak: 4 peers, 10 % loss, 0-80 ms jitter, 2 % duplicates for 5000 frames.
/// `cargo test --release -p ebobo-net session_lossy_soak_4p -- --ignored --nocapture`
#[test]
#[ignore]
fn session_lossy_soak_4p() {
    run_lossy(4, 0x50A4_50A4, 5000, Duration::from_secs(600), false);
}

/// The predictor repeats held pad and turbo bits but never a one-frame bit: a repeated save or
/// load would save or load again on every predicted frame until the real input arrives.
#[test]
fn predictor_repeats_held_and_turbo_not_one_frame_bits() {
    use ggrs::InputPredictor;
    let held = ebobo_console::nes::pad::A as u32 | ebobo_console::nes::pad::START as u32 | INPUT_TURBO_A | INPUT_TURBO_B;
    assert_eq!(PredictHeld::predict(held), held);
    for bit in [INPUT_RESET, INPUT_SAVE, INPUT_LOAD] {
        assert_eq!(PredictHeld::predict(held | bit), held, "bit {bit:#x}");
        assert_eq!(PredictHeld::predict(bit), 0, "bit {bit:#x}");
    }
    assert_eq!(PredictHeld::predict(held | INPUT_RESET | INPUT_SAVE | INPUT_LOAD), held);
    assert_eq!(PredictHeld::predict(0), 0);
    assert_eq!(PredictHeld::predict(u32::MAX), u32::MAX & !(INPUT_RESET | INPUT_SAVE | INPUT_LOAD));
}

#[test]
fn turbo_expansion_follows_frame_phase() {
    use ebobo_console::nes::pad;
    const A: u16 = pad::A as u16;
    const B: u16 = pad::B as u16;
    const START: u16 = pad::START as u16;
    // Phase on for frames 0, 1, 4, 5, 8, ...; off for 2, 3, 6, 7, ...
    let on: Vec<bool> = (0..8).map(|f| expand_turbo(INPUT_TURBO_A, f) & A != 0).collect();
    assert_eq!(on, [true, true, false, false, true, true, false, false]);
    assert_eq!(expand_turbo(INPUT_TURBO_A, 1001), A, "frame 1001: (1001 / 2) % 2 == 0");
    assert_eq!(expand_turbo(INPUT_TURBO_A, 1002), 0);
    // Held wins over turbo: A stays pressed in the off phase.
    assert_eq!(expand_turbo(A as u32 | INPUT_TURBO_A, 2), A);
    assert_eq!(expand_turbo(INPUT_TURBO_A | INPUT_TURBO_B, 4), A | B);
    assert_eq!(expand_turbo(INPUT_TURBO_A | INPUT_TURBO_B, 6), 0);
    // Other bits pass through untouched; reset and turbo bits never reach the pad bits.
    assert_eq!(expand_turbo(START as u32 | 0x80 | INPUT_TURBO_B, 2), START | 0x80);
    assert_eq!(expand_turbo(START as u32 | 0x80 | INPUT_TURBO_B | INPUT_RESET, 0), START | 0x80 | B);
}

/// Feeds `target` to `c` every 100 ms from `t0 + from_ms` to `t0 + to_ms` (exclusive) and returns
/// the changes as (ms, new delay).
fn drive(c: &mut DelayControl, t0: Instant, from_ms: u64, to_ms: u64, target: Option<usize>) -> Vec<(u64, usize)> {
    (from_ms..to_ms)
        .step_by(100)
        .filter_map(|ms| c.update(target, t0 + Duration::from_millis(ms)).map(|d| (ms, d)))
        .collect()
}

#[test]
fn delay_control_raises_after_one_second_one_step_at_a_time() {
    let t0 = Instant::now();
    let mut c = DelayControl::new(2, true);
    // Above for 1 s: one step; still above: one step per `DELAY_MIN_GAP`, never two at once.
    assert_eq!(drive(&mut c, t0, 0, 3500, Some(5)), [(1000, 3), (2000, 4), (3000, 5)]);
    assert_eq!(drive(&mut c, t0, 3500, 6000, Some(5)), []);
    assert_eq!(c.delay(), 5);
}

#[test]
fn delay_control_above_needs_an_unbroken_second() {
    let t0 = Instant::now();
    let mut c = DelayControl::new(2, true);
    assert_eq!(drive(&mut c, t0, 0, 900, Some(4)), []);
    // One sample at the current delay breaks the run.
    assert_eq!(c.update(Some(2), t0 + Duration::from_millis(900)), None);
    assert_eq!(drive(&mut c, t0, 1000, 2000, Some(4)), []);
    assert_eq!(c.update(Some(4), t0 + Duration::from_millis(2000)), Some(3));
}

#[test]
fn delay_control_lowers_only_after_ten_seconds() {
    let t0 = Instant::now();
    let mut c = DelayControl::new(5, true);
    assert_eq!(drive(&mut c, t0, 0, 10_000, Some(1)), []);
    assert_eq!(drive(&mut c, t0, 10_000, 12_500, Some(1)), [(10_000, 4), (11_000, 3), (12_000, 2)]);
    // A break (target above) restarts the 10 s wait for the next fall.
    let mut c = DelayControl::new(5, true);
    assert_eq!(drive(&mut c, t0, 0, 9_000, Some(3)), []);
    assert_eq!(c.update(Some(6), t0 + Duration::from_millis(9_000)), None);
    assert_eq!(drive(&mut c, t0, 9_100, 19_000, Some(3)), []);
    assert_eq!(c.update(Some(3), t0 + Duration::from_millis(19_100)), Some(4));
}

#[test]
fn delay_control_respects_gap_between_changes() {
    let t0 = Instant::now();
    let mut c = DelayControl::new(1, true);
    assert_eq!(c.update(Some(6), t0), None);
    assert_eq!(c.update(Some(6), t0 + Duration::from_millis(1000)), Some(2));
    // Still above for well over 1 s, but the last change was 999 ms ago.
    assert_eq!(c.update(Some(6), t0 + Duration::from_millis(1999)), None);
    assert_eq!(c.update(Some(6), t0 + Duration::from_millis(2000)), Some(3));
    // Long silence between samples still moves one frame per call.
    assert_eq!(c.update(Some(6), t0 + Duration::from_millis(9000)), Some(4));
    assert_eq!(c.delay(), 4);
}

#[test]
fn delay_control_clamps_to_bounds() {
    let t0 = Instant::now();
    let mut c = DelayControl::new(5, true);
    assert_eq!(drive(&mut c, t0, 0, 10_000, Some(40)), [(1000, 6)]);
    assert_eq!(c.delay(), 6);
    let mut c = DelayControl::new(2, true);
    assert_eq!(drive(&mut c, t0, 0, 30_000, Some(0)), [(10_000, 1)]);
    assert_eq!(c.delay(), 1);
}

#[test]
fn delay_control_never_moves_when_fixed_or_cold() {
    let t0 = Instant::now();
    let mut fixed = DelayControl::new(3, false);
    assert_eq!(drive(&mut fixed, t0, 0, 30_000, Some(6)), []);
    assert_eq!(drive(&mut fixed, t0, 30_000, 60_000, Some(1)), []);
    assert_eq!(fixed.delay(), 3);
    // No warm peer: no target, and a cold spell breaks a run in progress.
    let mut cold = DelayControl::new(3, true);
    assert_eq!(drive(&mut cold, t0, 0, 30_000, None), []);
    assert_eq!(drive(&mut cold, t0, 30_000, 30_900, Some(6)), []);
    assert_eq!(cold.update(None, t0 + Duration::from_millis(30_900)), None);
    assert_eq!(drive(&mut cold, t0, 31_000, 31_900, Some(6)), []);
    assert_eq!(cold.delay(), 3);
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
    /// Advanced ticks that re-simulated at least one frame.
    rollbacks: u64,
    max_rollback: u32,
    /// Frames advanced after the blackout ended (`Profile::Blackout` only).
    after_blackout: u64,
    desyncs: u32,
    interrupted: u32,
    disconnected: u32,
    /// Mean input delay of the peers when the run ended.
    delay_end: f64,
    /// Mean smoothed ping the peers reported when the run ended.
    ping_ms: f64,
}

/// What a paced run feeds its sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// A new random word every frame: nearly every late remote frame is a misprediction (the
    /// worst case for rollback).
    Random,
    /// Like a player: a direction held for 20-90 frames, A or B pressed for 4-12 frames.
    Play,
    /// `Play` with turbo B held, sent as the held bit.
    TurboHeld,
    /// `Play` with turbo B held, sent as B flipping every 2 frames (the wire form before 0.3.3).
    TurboFlip,
}

/// How a paced run drives its sessions.
#[derive(Clone, Copy, Debug)]
struct Paced {
    /// Every peer adapts its input delay (`delay` is the start value); false pins it.
    auto: bool,
    /// The network is polled every `POLL_EVERY` between ticks, as the frontend does; false
    /// polls once per tick (the frontend before 0.3.3).
    poll_between: bool,
    feed: Feed,
}

impl Paced {
    /// Pinned delay, random input: the matrix and the blackout regression.
    const WORST: Paced = Paced { auto: false, poll_between: true, feed: Feed::Random };
}

const POLL_EVERY: Duration = Duration::from_millis(2);

/// One peer's input source.
struct Player {
    rng: XorShift,
    held: u8,
    held_left: u32,
    button: u8,
    button_left: u32,
    tick: u32,
}

impl Player {
    fn new(seed: u64) -> Player {
        Player { rng: XorShift(seed), held: 0, held_left: 0, button: 0, button_left: 0, tick: 0 }
    }

    fn next(&mut self, feed: Feed) -> u32 {
        use ebobo_console::nes::pad;
        if feed == Feed::Random {
            return (self.rng.next() & RANDOM_INPUT) as u32;
        }
        self.tick += 1;
        if self.held_left == 0 {
            self.held = [0, pad::LEFT, pad::RIGHT, pad::RIGHT, pad::DOWN][(self.rng.next() % 5) as usize];
            self.held_left = 20 + (self.rng.next() % 71) as u32;
        }
        self.held_left -= 1;
        if self.button_left == 0 {
            // A press of 4-12 frames, then 10-60 frames released.
            self.button = if self.button == 0 { [pad::A, pad::A, pad::B][(self.rng.next() % 3) as usize] } else { 0 };
            self.button_left =
                if self.button == 0 { 10 + (self.rng.next() % 51) as u32 } else { 4 + (self.rng.next() % 9) as u32 };
        }
        self.button_left -= 1;
        let word = (self.held | self.button) as u32;
        match feed {
            Feed::TurboHeld => word | INPUT_TURBO_B,
            Feed::TurboFlip if (self.tick / 2) % 2 == 0 => word | pad::B as u32,
            _ => word,
        }
    }
}

/// Plays `n` peers on `profile` for `secs` after all are synchronized, paced like the frontend:
/// one `advance` per 60 fps tick, the tick stretched 2 % per frame ahead (max 10 %) or
/// shortened 1 % per frame behind (max 3 %), frame debt above 3 ticks dropped. The blackout and
/// `Shift`'s turn for the worse start at 30 % of `secs`.
fn run_paced(profile: Profile, n: usize, delay: usize, max_prediction: usize, secs: f64, seed: u64, how: Paced) -> Option<RunStats> {
    let first = nes()?;
    let (transports, proxy) = lossy_mesh(n, seed, profile);
    let slots: Vec<PeerId> = (0..n as u8).map(PeerId).collect();
    let mut sessions: Vec<NetSession> = transports
        .into_iter()
        .map(|t| {
            NetSession::with_tuning(t, &slots, delay, how.auto, 60, SavePolicy::Everyone, max_prediction, DISCONNECT_TIMEOUT, DISCONNECT_NOTIFY)
                .expect("session")
        })
        .collect();
    let mut emus: Vec<Console> = std::iter::once(first).chain((1..n).map(|_| nes().unwrap())).collect();
    let mut players: Vec<Player> = (0..n).map(|i| Player::new(seed ^ (0x9E37_79B9 * (i as u64 + 1)))).collect();
    let mut polled = vec![Instant::now(); n];
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
        proxy.degraded.store(profile == Profile::Shift && t >= dark_from, Ordering::Relaxed);
        for i in 0..n {
            let s = &mut sessions[i];
            // Before the start nothing ticks, so synchronization needs the poll.
            if t < 0.0 || (how.poll_between && now - polled[i] >= POLL_EVERY) {
                s.poll();
                polled[i] = now;
            }
            if t >= 0.0 && now >= next[i] {
                if !how.poll_between {
                    s.poll();
                }
                let running = s.is_running();
                let out = s.advance(&mut emus[i], &[players[i].next(how.feed)]).expect("advance");
                if running {
                    st.ticks += 1;
                    if out.advanced {
                        streak[i] = 0;
                        st.advanced += 1;
                        st.rollback_frames += out.rollback_frames as u64;
                        st.rollbacks += (out.rollback_frames > 0) as u64;
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
                // Mirrors `ebobo::app::logic::{frame_stretch, frame_period, next_deadline}`.
                let ahead = s.frames_ahead() as f64;
                let stretch = if ahead > 0.0 { (0.02 * ahead).min(0.10) } else { (0.01 * ahead).max(-0.03) };
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
    let stats: Vec<NetStats> = sessions.iter().map(|s| s.stats()).collect();
    st.delay_end = stats.iter().map(|s| s.input_delay as f64).sum::<f64>() / n as f64;
    let pings: Vec<f64> = stats.iter().flat_map(|s| s.peers.iter().map(|p| p.ping_ms as f64)).collect();
    st.ping_ms = pings.iter().sum::<f64>() / pings.len().max(1) as f64;
    Some(st)
}

/// Mean time of one 12-frame rollback on the real core (load, 12 × save + hash + run, the last
/// frame rendered) and the worst of `reps`.
fn rollback_cost(nes: &mut Console, reps: u32) -> (Duration, Duration) {
    for f in 0..120u32 {
        nes.run_frame([f as u8 as u16, 0, 0, 0]);
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
            nes.run_frame([f.wrapping_mul(37) as u16, 0, 0, 0]);
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

/// Bad-network matrix: profiles × 2/4 peers × input delay 1-6 × max prediction 8/10/12/16/20,
/// paced at 60 fps. Prints stalls, rollback depth, desyncs and Interrupted/Disconnected counts.
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
                for pred in [8, 10, 12, 16, 20] {
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
                let st = run_paced(profile, n, delay, pred, secs, seed, Paced::WORST).expect("ROMs present");
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

/// What 0.3.3 changed, measured on the emulated networks in pairs that share a seed: the input
/// delay as 0.3.2 pinned it or adapting, on a link that turns bad mid-game and on a mobile link;
/// turbo sent as flipping B or as a held bit; the network polled once per tick or between ticks.
/// `cargo test --release -p ebobo-net session_netcode_gains -- --ignored --nocapture`
/// (`EBOBO_PROFILE_SECS`, default 20, run length; `EBOBO_PROFILE_JOBS`, default 6, parallel runs;
/// `EBOBO_PROFILE_ONLY`, a part of the case name, runs just those cases).
#[test]
#[ignore]
fn session_netcode_gains() {
    if nes().is_none() {
        return;
    }
    let secs: f64 = env_or("EBOBO_PROFILE_SECS", 20.0);
    let workers: usize = env_or("EBOBO_PROFILE_JOBS", 6);
    let play = Paced { auto: false, poll_between: true, feed: Feed::Play };
    let auto = Paced { auto: true, ..play };
    let cases: Vec<(&str, Profile, usize, usize, Paced)> = vec![
        ("link turns bad, delay pinned at 1", Profile::Shift, 2, 1, play),
        ("link turns bad, delay adapts", Profile::Shift, 2, 1, auto),
        ("4 players, pinned at 1", Profile::Shift, 4, 1, play),
        ("4 players, adapts", Profile::Shift, 4, 1, auto),
        ("mobile, pinned at 6 (0.3.2 auto)", Profile::Mobile, 2, 6, play),
        ("mobile, adapts", Profile::Mobile, 2, 3, auto),
        ("far, pinned at 6 (0.3.2 auto)", Profile::Far, 2, 6, play),
        ("far, adapts", Profile::Far, 2, 3, auto),
        ("mobile, turbo as flipping B", Profile::Mobile, 2, 3, Paced { feed: Feed::TurboFlip, ..play }),
        ("mobile, turbo as a held bit", Profile::Mobile, 2, 3, Paced { feed: Feed::TurboHeld, ..play }),
        ("mobile, adapts, poll per tick", Profile::Mobile, 2, 1, Paced { poll_between: false, ..auto }),
        ("mobile, adapts, poll every 2 ms", Profile::Mobile, 2, 1, auto),
    ];
    let only: String = env_or("EBOBO_PROFILE_ONLY", String::new());
    let jobs = cases.iter().map(|c| (c.1, c.2, c.3, c.4)).enumerate().filter(|(i, _)| cases[*i].0.contains(only.as_str()));
    let queue = Arc::new(Mutex::new(jobs.collect::<Vec<_>>()));
    let results = Arc::new(Mutex::new(Vec::new()));
    let handles: Vec<_> = (0..workers)
        .map(|_| {
            let (queue, results) = (queue.clone(), results.clone());
            std::thread::spawn(move || loop {
                let Some((idx, (profile, n, delay, how))) = queue.lock().unwrap().pop() else { return };
                // Both runs of a pair see the same network.
                let seed = 0x6A1A_0000 + (idx as u64 / 2) * 0x1_0001;
                let st = run_paced(profile, n, delay, max_prediction(n), secs, seed, how).expect("ROMs present");
                results.lock().unwrap().push((idx, st));
            })
        })
        .collect();
    for h in handles {
        h.join().expect("gains worker panicked");
    }
    let mut rows = std::mem::take(&mut *results.lock().unwrap());
    rows.sort_by_key(|r| r.0);
    println!("{secs} s per run; rb % = frames that needed a rollback; depth = frames re-simulated per rollback");
    println!("{:<33} | stall%  streak |   rb %  depth  rb max | delay  ping ms | desync", "case");
    let mut desyncs = 0;
    for (idx, st) in rows {
        desyncs += st.desyncs;
        println!(
            "{:<33} | {:>6.2} {:>7} | {:>6.1} {:>6.1} {:>7} | {:>5.1} {:>8.0} | {:>6}",
            cases[idx].0,
            100.0 * st.stalls as f64 / st.ticks.max(1) as f64,
            st.max_stall_streak,
            100.0 * st.rollbacks as f64 / st.advanced.max(1) as f64,
            st.rollback_frames as f64 / st.rollbacks.max(1) as f64,
            st.max_rollback,
            st.delay_end,
            st.ping_ms,
            st.desyncs,
        );
    }
    assert_eq!(desyncs, 0, "desyncs in the gains runs");
}

/// Bad-internet regression: 2 peers on a mobile network, one cut off for 4 s, finish without a
/// disconnect or desync and keep playing after the blackout.
#[test]
fn session_profile_blackout_2p() {
    let Some(st) = run_paced(Profile::Blackout, 2, 3, MAX_PREDICTION, 8.0, 0xB1AC_0002, Paced::WORST) else { return };
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
    let e = SavePolicy::Everyone;
    let sa = NetSession::with_tuning(crate::Link::udp(a, PeerId(0), &[(PeerId(1), ba)]), &slots, 2, false, 60, e, p, t, notify).unwrap();
    let sb = NetSession::with_tuning(crate::Link::udp(b, PeerId(1), &[(PeerId(0), aa)]), &slots, 2, false, 60, e, p, t, notify).unwrap();
    (sa, sb)
}

/// Drives both until running and `frames` frames advanced on each.
fn warm_up(sa: &mut NetSession, sb: &mut NetSession, na: &mut Console, nb: &mut Console, frames: u32) {
    let (mut fa, mut fb) = (0, 0);
    let deadline = Instant::now() + Duration::from_secs(20);
    while fa < frames || fb < frames {
        assert!(Instant::now() < deadline, "warm-up timed out");
        sa.poll();
        sb.poll();
        fa += sa.advance(na, &[1]).unwrap().advanced as u32;
        fb += sb.advance(nb, &[2]).unwrap().advanced as u32;
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
        let out = sa.advance(&mut na, &[3]).unwrap();
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
        let _ = sa.advance(&mut na, &[0]).unwrap();
        if sa.events().contains(&NetEvent::Disconnected { peer: PeerId(1) }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("BYE noticed after {:?}", start.elapsed());
}

/// A peer leaves at the moment the other one has every input of it up to the frame before the
/// current one: nothing was predicted, so nothing is rolled back and play goes on. GGRS 0.13.0
/// asked to reload the current frame here and panicked ("must load frame in the past").
#[test]
fn session_peer_leaving_caught_up_keeps_playing() {
    let (mut sa, mut sb) = direct_pair();
    let Some(mut na) = nes() else { return };
    let mut nb = nes().unwrap();
    warm_up(&mut sa, &mut sb, &mut na, &mut nb, 30);
    // `sb` runs ahead and stops: its last input is for a frame `sa` has not reached yet.
    // (`confirmed_frame` also counts `sa`'s own queued input, which reaches one frame ahead.)
    let deadline = Instant::now() + Duration::from_secs(5);
    while sa.ggrs.confirmed_frame() <= sa.ggrs.current_frame() {
        assert!(Instant::now() < deadline, "sb never got ahead");
        sb.poll();
        let _ = sb.advance(&mut nb, &[2]).unwrap();
        sa.poll();
        std::thread::sleep(Duration::from_millis(1));
    }
    // `sa` catches up to exactly one frame past `sb`'s last input.
    while sa.ggrs.current_frame() <= sa.ggrs.confirmed_frame() {
        assert!(sa.advance(&mut na, &[1]).unwrap().advanced);
    }
    assert_eq!(sa.ggrs.current_frame(), sa.ggrs.confirmed_frame() + 1);
    let _ = sa.events();
    sb.leave();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !sa.events().contains(&NetEvent::Disconnected { peer: PeerId(1) }) {
        assert!(Instant::now() < deadline, "the BYE never arrived");
        sa.poll();
        std::thread::sleep(Duration::from_millis(1));
    }
    for _ in 0..30 {
        assert!(sa.advance(&mut na, &[1]).unwrap().advanced);
    }
}

/// Drives both for `frames` more frames on each and returns the `Saved` events of each.
fn saved_events(sa: &mut NetSession, sb: &mut NetSession, na: &mut Console, nb: &mut Console, frames: u32) -> [Vec<NetEvent>; 2] {
    let (mut fa, mut fb) = (0, 0);
    let mut saved = [Vec::new(), Vec::new()];
    let deadline = Instant::now() + Duration::from_secs(20);
    while fa < frames || fb < frames {
        assert!(Instant::now() < deadline, "timed out");
        fa += sa.advance(na, &[1]).unwrap().advanced as u32;
        fb += sb.advance(nb, &[2]).unwrap().advanced as u32;
        for (s, out) in [&mut *sa, &mut *sb].into_iter().zip(&mut saved) {
            out.extend(s.events().into_iter().filter(|e| matches!(e, NetEvent::Saved { .. })));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    saved
}

/// A save pressed on the frame right after the input delay fell: GGRS drops that frame's input
/// (it would land on a frame that already has one), and the save went with it.
#[test]
fn session_one_frame_bit_survives_a_delay_drop() {
    let (mut sa, mut sb) = direct_pair();
    let Some(mut na) = nes() else { return };
    let mut nb = nes().unwrap();
    warm_up(&mut sa, &mut sb, &mut na, &mut nb, 60);
    sa.force_delay = Some(1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while sa.force_delay.is_some() {
        assert!(Instant::now() < deadline, "the delay never changed");
        let _ = sa.advance(&mut na, &[1]).unwrap();
        let _ = sb.advance(&mut nb, &[2]).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = sa.advance(&mut na, &[1 | INPUT_SAVE]).unwrap();
    let saved = saved_events(&mut sa, &mut sb, &mut na, &mut nb, 120);
    assert_eq!(saved, [vec![NetEvent::Saved { slot: 0 }], vec![NetEvent::Saved { slot: 0 }]]);
}

/// A save pressed while the session waits at the prediction window: GGRS already holds an
/// input for that frame and ignores the repeat, and the save went with it.
#[test]
fn session_one_frame_bit_survives_a_stall() {
    let (mut sa, mut sb) = direct_pair();
    let Some(mut na) = nes() else { return };
    let mut nb = nes().unwrap();
    warm_up(&mut sa, &mut sb, &mut na, &mut nb, 60);
    // `sb` goes quiet (well under the disconnect notice) until `sa` cannot advance.
    let deadline = Instant::now() + Duration::from_millis(250);
    while sa.advance(&mut na, &[1]).unwrap().advanced {
        assert!(Instant::now() < deadline, "sa never reached the prediction window");
    }
    assert!(!sa.advance(&mut na, &[1 | INPUT_SAVE]).unwrap().advanced);
    let saved = saved_events(&mut sa, &mut sb, &mut na, &mut nb, 120);
    assert_eq!(saved, [vec![NetEvent::Saved { slot: 0 }], vec![NetEvent::Saved { slot: 0 }]]);
}

/// Input delay of the save/load runs. GGRS keeps the input of an `advance` that hits the
/// prediction window and drops the repeat on the next call, so a bit sent once, on a call made
/// at GGRS frame `k`, lands on frame `k + SAVE_DELAY`.
const SAVE_DELAY: usize = 2;

/// A one-frame bit `peer` sends once, on its first `advance` at GGRS frame `after`; it lands on
/// frame `after + SAVE_DELAY`.
#[derive(Clone, Copy, Debug)]
struct Press {
    peer: usize,
    after: u32,
    bits: u32,
}

impl Press {
    fn frame(self) -> i32 {
        (self.after as usize + SAVE_DELAY) as i32
    }
}

/// The frame procedure of `NetSession::advance`, run on a plain `Console` without a network over the
/// final input words of every frame.
struct Reference {
    events: Vec<NetEvent>,
    /// State hash of every save, in frame order.
    saved: Vec<u64>,
    /// State hash of the checkpoints at frames 300, 600, ...
    checkpoints: Vec<u64>,
    /// State hash at the start of every frame, before its procedure.
    at_start: Vec<u64>,
}

fn reference(log: &[[u32; 4]], start: Option<&ebobo_console::Snapshot>) -> Reference {
    let mut nes = nes().expect("ROMs present");
    if let Some(s) = start {
        nes.restore(s);
    }
    let mut save = start.cloned();
    let mut r = Reference { events: Vec::new(), saved: Vec::new(), checkpoints: Vec::new(), at_start: Vec::new() };
    for (frame, words) in log.iter().enumerate() {
        let frame = frame as i32;
        r.at_start.push(nes.state_hash());
        let lowest = |bit: u32| words.iter().position(|w| w & bit != 0);
        // The save a load of this frame finds: the one before a save of this same frame.
        let mut before = None;
        if let Some(slot) = lowest(INPUT_SAVE) {
            r.events.push(NetEvent::Saved { slot });
            r.saved.push(nes.state_hash());
            before = Some(save.replace(nes.snapshot()));
        }
        if frame > 0 && frame % 300 == 0 {
            r.checkpoints.push(nes.state_hash());
        }
        if let Some(slot) = lowest(INPUT_LOAD) {
            if let Some(s) = before.as_ref().unwrap_or(&save) {
                nes.restore(s);
                r.events.push(NetEvent::Loaded { slot });
            }
        }
        if words.iter().any(|w| w & INPUT_RESET != 0) {
            nes.reset();
        }
        nes.run_frame(std::array::from_fn(|i| expand_turbo(words[i], frame)));
    }
    r
}

fn state_hash_of(snapshot: &ebobo_console::Snapshot) -> u64 {
    let mut scratch = nes().expect("ROMs present");
    scratch.restore(snapshot);
    scratch.state_hash()
}

/// What `run_saves` saw; every peer agreed on it.
struct SaveRun {
    /// Final input words of every frame up to the last frame final on every peer.
    log: Vec<[u32; 4]>,
    reference: Reference,
    /// Frames re-simulated by all peers together.
    rollback_frames: u64,
    /// State hash at the start of `probe`.
    probe_hash: u64,
}

/// Runs `n` peers on `profile`, each advancing at most once per 60 fps tick like the frontend,
/// with random input on every frame (most late remote frames mispredict, so save and load frames
/// get re-simulated) plus the one-frame `presses`, until every peer has `probe` and every pressed
/// frame final. `start`: the shared save every peer loaded before the first frame. `observe` sees
/// every peer after each `advance`.
///
/// Asserts no desync, equal final inputs on every peer, the pressed bits on their frames, and
/// against `reference` over those inputs: every peer's `Saved`/`Loaded` sequence, the state of
/// every `take_saved` and `take_checkpoint` snapshot, and the state at `probe`.
fn run_saves(
    n: usize,
    profile: Profile,
    seed: u64,
    presses: &[Press],
    probe: i32,
    start: Option<&ebobo_console::Snapshot>,
    observe: &mut dyn FnMut(usize, &NetSession),
) -> Option<SaveRun> {
    let first = nes()?;
    let (transports, _proxy) = lossy_mesh(n, seed, profile);
    let slots: Vec<PeerId> = (0..n as u8).map(PeerId).collect();
    let mut sessions: Vec<NetSession> = transports
        .into_iter()
        .map(|t| {
            let mut s = NetSession::new(t, &slots, SAVE_DELAY, false, 60, start.cloned(), SavePolicy::Everyone).expect("session");
            s.probe = (probe, None);
            s
        })
        .collect();
    let mut emus: Vec<Console> = std::iter::once(first).chain((1..n).map(|_| nes().unwrap())).collect();
    if let Some(s) = start {
        emus.iter_mut().for_each(|e| e.restore(s));
    }
    let mut rngs: Vec<XorShift> = (0..n).map(|i| XorShift(seed ^ (0x9E37_79B9 * (i as u64 + 1)))).collect();
    // Per peer, latest first so `pop` gives the next one.
    let mut queues: Vec<Vec<Press>> = (0..n)
        .map(|i| {
            let mut q: Vec<Press> = presses.iter().filter(|p| p.peer == i).copied().collect();
            q.sort_by_key(|p| std::cmp::Reverse(p.after));
            q
        })
        .collect();
    let until = presses.iter().map(|p| p.frame()).fold(probe, i32::max);
    let mut advanced = vec![0u32; n];
    let mut rollback_frames = 0u64;
    let mut events = vec![Vec::new(); n];
    let mut saved = vec![Vec::new(); n];
    let mut checkpoints = vec![Vec::new(); n];
    let mut desyncs = Vec::new();
    let period = Duration::from_secs_f64(1.0 / 60.0);
    let mut next = vec![Instant::now(); n];
    let deadline = Instant::now() + Duration::from_secs(120);
    while !sessions.iter().all(|s| s.final_through >= until) {
        let finals: Vec<i32> = sessions.iter().map(|s| s.final_through).collect();
        let now = Instant::now();
        assert!(now < deadline, "timed out: advanced {advanced:?}, final through {finals:?}");
        for i in 0..n {
            let s = &mut sessions[i];
            s.poll();
            if s.is_running() {
                if now < next[i] {
                    continue;
                }
                // Frame debt above 3 ticks is dropped, as the frontend does.
                next[i] = if now.saturating_duration_since(next[i]) > period * 3 { now + period } else { next[i] + period };
            }
            let mut input = (rngs[i].next() & RANDOM_INPUT) as u32;
            // Only a running session takes the input; it lands `SAVE_DELAY` after the current frame.
            while s.is_running() && queues[i].last().is_some_and(|p| s.ggrs.current_frame() >= p.after as i32) {
                input |= queues[i].pop().unwrap().bits;
            }
            let out = s.advance(&mut emus[i], &[input]).expect("advance");
            advanced[i] += out.advanced as u32;
            rollback_frames += out.rollback_frames as u64;
            if let Some(snap) = s.take_saved() {
                saved[i].push(state_hash_of(&snap));
            }
            if let Some(snap) = s.take_checkpoint() {
                checkpoints[i].push(state_hash_of(&snap));
            }
            observe(i, s);
            for e in s.events() {
                match e {
                    NetEvent::Saved { .. } | NetEvent::Loaded { .. } => events[i].push(e),
                    NetEvent::Desync { frame } => desyncs.push((i, frame)),
                    _ => {}
                }
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    assert!(desyncs.is_empty(), "desync events: {desyncs:?}");
    let cut = sessions.iter().map(|s| s.final_through).min().unwrap() as usize + 1;
    let log = sessions[0].input_log[..cut].to_vec();
    for (i, s) in sessions.iter().enumerate() {
        assert!(s.input_log[..cut] == log[..], "peer {i} simulated other final inputs");
    }
    for p in presses {
        let word = log[p.frame() as usize][p.peer];
        assert_eq!(word & p.bits, p.bits, "{p:?} is not on frame {}: {word:#x}", p.frame());
    }
    let r = reference(&log, start);
    let probe_hash = r.at_start[probe as usize];
    for i in 0..n {
        assert_eq!(events[i], r.events, "peer {i} events");
        assert_eq!(saved[i], r.saved, "peer {i} saved states");
        // A peer may have one more final checkpoint past the common cut.
        assert!(checkpoints[i].len() >= r.checkpoints.len(), "peer {i}: {} checkpoints", checkpoints[i].len());
        assert_eq!(checkpoints[i][..r.checkpoints.len()], r.checkpoints[..], "peer {i} checkpoints");
        assert_eq!(sessions[i].probe.1, Some(probe_hash), "peer {i} state at frame {probe}");
    }
    Some(SaveRun { log, reference: r, rollback_frames, probe_hash })
}

fn press(peer: usize, after: u32, bits: u32) -> Press {
    Press { peer, after, bits }
}

/// Both peers save and load at different frames on a lossy, jittery link with input that
/// mispredicts nearly every late frame: both report the same saves and loads, hand out the same
/// saved and checkpoint states, and every one of them, and the state after the loads, is what a
/// plain `Console` replaying the final inputs with the same frame procedure has. A snapshot handed out
/// before the rollback that corrected its frame would differ from the reference. The load on
/// frame 32 finds no save and does nothing.
#[test]
fn session_saves_and_loads_match_reference_on_lossy_link_2p() {
    let presses = [
        press(0, 30, INPUT_LOAD),
        press(0, 100, INPUT_SAVE),
        press(1, 250, INPUT_LOAD),
        press(1, 400, INPUT_SAVE),
        press(0, 520, INPUT_LOAD),
    ];
    let Some(run) = run_saves(2, Profile::Lossy, 0x5A7E_0002, &presses, 700, None, &mut |_, _| {}) else { return };
    use NetEvent::{Loaded, Saved};
    assert_eq!(run.reference.events, [Saved { slot: 0 }, Loaded { slot: 1 }, Saved { slot: 1 }, Loaded { slot: 0 }]);
    assert_eq!(run.reference.checkpoints.len(), 2, "checkpoints at 300 and 600");
    assert!(run.rollback_frames > 100, "the link caused only {} re-simulated frames", run.rollback_frames);
    println!("lossy 2p: {} frames final, {} rollback frames, probe {:x}", run.log.len(), run.rollback_frames, run.probe_hash);
}

/// A load two frames after the other player's save, 150 ms away: the loading peer first
/// simulates its load before the save's input arrived (no save yet: nothing to load), and the
/// rollback must re-run both so the load restores that save. Then a save and a load on the same
/// frame by different players: the save keeps the state the frame starts with, the load restores
/// the save before it, and a later load restores the new save.
#[test]
fn session_load_inside_save_rollback_window_and_same_frame_save_load() {
    let presses = [
        press(0, 100, INPUT_SAVE),
        press(1, 102, INPUT_LOAD),
        press(0, 300, INPUT_SAVE),
        press(1, 300, INPUT_LOAD),
        press(0, 400, INPUT_LOAD),
    ];
    // Peer 1's `has_save` when it first simulated past its load on frame 104.
    let mut blind = None;
    let mut observe = |i: usize, s: &NetSession| {
        if i == 1 && s.sim_frame > 104 && blind.is_none() {
            blind = Some(!s.has_save());
        }
    };
    let Some(run) = run_saves(2, Profile::Far, 0x5A7E_0003, &presses, 520, None, &mut observe) else { return };
    use NetEvent::{Loaded, Saved};
    assert_eq!(
        run.reference.events,
        [Saved { slot: 0 }, Loaded { slot: 1 }, Saved { slot: 0 }, Loaded { slot: 1 }, Loaded { slot: 0 }]
    );
    assert_eq!(blind, Some(true), "peer 1 knew of the save before simulating its load: no rollback tested");
    let log = &run.log;
    assert_eq!(log[302][0] & INPUT_SAVE, INPUT_SAVE);
    assert_eq!(log[302][1] & INPUT_LOAD, INPUT_LOAD);
    // Independent of `reference`: the load on 302 restores the save of 102, so the next frame
    // starts with frame 102's state run with frame 302's pads.
    let mut nes = nes().unwrap();
    for (f, words) in log[..=102].iter().enumerate() {
        if f == 102 {
            assert_eq!(nes.state_hash(), run.reference.saved[0]);
        } else {
            nes.run_frame(std::array::from_fn(|i| expand_turbo(words[i], f as i32)));
        }
    }
    let after_102 = nes.snapshot();
    nes.run_frame(std::array::from_fn(|i| expand_turbo(log[302][i], 302)));
    assert_eq!(nes.state_hash(), run.reference.at_start[303], "the load on 302 did not restore the save of 102");
    // And the load on 104 restored the same save.
    nes.restore(&after_102);
    nes.run_frame(std::array::from_fn(|i| expand_turbo(log[104][i], 104)));
    assert_eq!(nes.state_hash(), run.reference.at_start[105], "the load on 104 did not restore the save of 102");
}

/// A game continued from a save: `has_save` is true from the first moment, and a load before any
/// save restores the start state on both peers.
#[test]
fn session_start_save_is_loaded_before_any_save() {
    let Some(mut nes) = nes() else { return };
    for f in 0..200u32 {
        nes.run_frame([(f * 7) as u8 as u16, (f * 13) as u8 as u16, 0, 0]);
    }
    let start = nes.snapshot();
    let presses = [press(1, 60, INPUT_LOAD), press(0, 200, INPUT_SAVE)];
    let mut always = true;
    let Some(run) = run_saves(2, Profile::Lossy, 0x5A7E_0004, &presses, 63, Some(&start), &mut |_, s: &NetSession| {
        always &= s.has_save()
    }) else {
        return;
    };
    use NetEvent::{Loaded, Saved};
    assert_eq!(run.reference.events, [Loaded { slot: 1 }, Saved { slot: 0 }]);
    assert!(always, "has_save was false in a game started from a save");
    // Independent of `reference`: frame 63 starts with the start state run with frame 62's pads.
    nes.restore(&start);
    nes.run_frame(std::array::from_fn(|i| expand_turbo(run.log[62][i], 62)));
    assert_eq!(nes.state_hash(), run.probe_hash, "the load on 62 did not restore the start save");
}

/// A game from power-on: `has_save` stays false until a save exists, a load before it does
/// nothing and reports nothing, and it is true once the save is final.
#[test]
fn session_power_on_has_no_save_until_the_first() {
    let presses = [press(1, 40, INPUT_LOAD), press(0, 150, INPUT_SAVE)];
    let save_frame = presses[1].frame();
    let mut bad = Vec::new();
    let mut observe = |i: usize, s: &NetSession| {
        let has = s.has_save();
        // A save only exists once its frame was simulated; once final it never goes away.
        if (has && s.sim_frame <= save_frame) || (!has && s.final_through >= save_frame) {
            bad.push((i, s.sim_frame, s.final_through, has));
        }
    };
    let Some(run) = run_saves(2, Profile::Lossy, 0x5A7E_0005, &presses, 250, None, &mut observe) else { return };
    assert_eq!(run.reference.events, [NetEvent::Saved { slot: 0 }]);
    assert!(bad.is_empty(), "has_save wrong at (peer, sim frame, final through, has): {bad:?}");
}

/// Four peers on the lossy link: player 3 saves, player 4 loads; all four agree with the
/// reference.
#[test]
fn session_saves_and_loads_match_reference_4p() {
    let presses = [press(2, 150, INPUT_SAVE), press(3, 260, INPUT_LOAD)];
    let Some(run) = run_saves(4, Profile::Lossy, 0x5A7E_0006, &presses, 400, None, &mut |_, _| {}) else { return };
    assert_eq!(run.reference.events, [NetEvent::Saved { slot: 2 }, NetEvent::Loaded { slot: 3 }]);
}

/// What the two remaining peers of `run_uneven_leaver` saw.
struct LeaverRun {
    sessions: Vec<NetSession>,
    /// Hash of the newest `take_saved` snapshot of peers 0 and 1.
    saves: Vec<Option<u64>>,
    /// `Saved`/`Loaded` events of peers 0 and 1.
    events: Vec<Vec<NetEvent>>,
    /// `(peer, frame)` of every `Desync` event of peers 0 and 1.
    desyncs: Vec<(usize, i32)>,
}

/// Three peers on the `Shift` link; player 3 leaves after the two others received different
/// amounts of its input. Peer 1 (input delay 8) stops polling at frame 200, holding the leaver's
/// input to about 202 while it already sent its own to 208, so peer 0 receives more of the
/// leaver's input than peer 1 reports to hold. After 500 ms both drop the leaver (driven through
/// `disconnect_player` directly; the emulated network has no one-way cut), and GGRS moves peer
/// 0's disconnect down to peer 1's frame and re-simulates from there. Runs until both finalized
/// frame 262; the state at frame 260 is in `probe`. With `save_load`, player 1 saves on 205 and
/// loads on 242: the save lies between the frames the two peers hold of the leaver.
fn run_uneven_leaver(seed: u64, save_load: bool) -> Option<LeaverRun> {
    let first = nes()?;
    let (transports, _proxy) = lossy_mesh(3, seed, Profile::Shift);
    let slots: Vec<PeerId> = (0..3u8).map(PeerId).collect();
    let mut sessions: Vec<NetSession> = transports
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            let mut s = NetSession::new(t, &slots, if i == 1 { 8 } else { 2 }, false, 60, None, SavePolicy::Everyone).expect("session");
            s.probe = (260, None);
            s
        })
        .collect();
    let mut emus: Vec<Console> = std::iter::once(first).chain((1..3).map(|_| nes().unwrap())).collect();
    let mut rngs: Vec<XorShift> = (0..3).map(|i| XorShift(0x1EA7 ^ (0x9E37_79B9 * (i as u64 + 1)))).collect();
    let (mut saves, mut events, mut desyncs) = (vec![None; 2], vec![Vec::new(); 2], Vec::new());
    let (mut paused_at, mut dropped) = (None::<Instant>, false);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !(dropped && sessions[..2].iter().all(|s| s.final_through >= 262)) {
        assert!(Instant::now() < deadline, "timed out: final {:?}", sessions.iter().map(|s| s.final_through).collect::<Vec<_>>());
        if paused_at.is_none() && sessions[1].ggrs.current_frame() >= 200 {
            paused_at = Some(Instant::now());
        }
        let paused = paused_at.is_some() && !dropped;
        if paused && paused_at.unwrap().elapsed() > Duration::from_millis(500) {
            // The leaver vanishes; each remaining peer drops it at the input it has.
            for s in &mut sessions[..2] {
                s.ggrs.disconnect_player(2).expect("disconnect");
                s.gone[2] = true;
            }
            dropped = true;
            continue;
        }
        for i in 0..3 {
            if (i == 1 && paused) || (i == 2 && dropped) {
                continue;
            }
            let s = &mut sessions[i];
            s.poll();
            let mut input = (rngs[i].next() & RANDOM_INPUT) as u32;
            let f = s.ggrs.current_frame();
            if save_load && s.is_running() && i == 0 && s.input_log.len() <= 205 && f == 203 {
                input |= INPUT_SAVE;
            }
            if save_load && s.is_running() && i == 0 && f == 240 && s.input_log.len() <= 242 {
                input |= INPUT_LOAD;
            }
            s.advance(&mut emus[i], &[input]).expect("advance");
            if i < 2 {
                if let Some(snap) = s.take_saved() {
                    saves[i] = Some(state_hash_of(&snap));
                }
                for e in s.events() {
                    match e {
                        NetEvent::Saved { .. } | NetEvent::Loaded { .. } => events[i].push(e),
                        NetEvent::Desync { frame } => desyncs.push((i, frame)),
                        _ => {}
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Some(LeaverRun { sessions, saves, events, desyncs })
}

/// A player leaving after the two others received different amounts of its input crashes
/// neither of them and leaves both in the same state, without a desync. GGRS `25bd998` panicked
/// here (`input_queue.rs:140`, or `sync_layer.rs:238` with a wider gap): peer 0 had confirmed,
/// and discarded, inputs above the frame peer 1's disconnect moved it back to.
#[test]
fn session_leaver_with_uneven_input_keeps_others_in_step() {
    let Some(run) = run_uneven_leaver(0x1EA7_0004, false) else { return };
    let (a, b) = (&run.sessions[0], &run.sessions[1]);
    assert!(run.desyncs.is_empty(), "desync events: {:?}", run.desyncs);
    assert!(a.probe.1.is_some() && a.probe.1 == b.probe.1, "states at 260 differ: {:x?} / {:x?}", a.probe.1, b.probe.1);
    assert!(run.events.iter().all(Vec::is_empty), "save events without saves: {:?}", run.events);
}

/// The same leaver with a save between the frames the two peers hold of its input and a load
/// after: neither crashes, both end with the same shared save, one `Saved` and one `Loaded`
/// each, the same state after the load and no desync.
#[test]
fn session_leaver_with_uneven_input_keeps_shared_save_in_step() {
    let Some(run) = run_uneven_leaver(0x1EA7_0003, true) else { return };
    let (a, b) = (&run.sessions[0], &run.sessions[1]);
    let saves = &run.saves;
    assert_eq!(a.input_log[205][0] & INPUT_SAVE, INPUT_SAVE, "save not on 205");
    assert_eq!(a.input_log[242][0] & INPUT_LOAD, INPUT_LOAD, "load not on 242");
    println!("final through {} / {}, saves {saves:x?}, events {:?}", a.final_through, b.final_through, run.events);
    assert!(run.desyncs.is_empty(), "desync events: {:?}", run.desyncs);
    assert!(saves[0].is_some() && saves[0] == saves[1], "shared saves differ: {saves:x?}");
    let (sa, sb) = (&a.saves.last().unwrap().state, &b.saves.last().unwrap().state);
    assert_eq!(state_hash_of(sa), state_hash_of(sb), "in-memory shared saves differ");
    assert!(a.probe.1.is_some() && a.probe.1 == b.probe.1, "states after the load differ");
    use NetEvent::{Loaded, Saved};
    assert_eq!(run.events[0], [Saved { slot: 0 }, Loaded { slot: 0 }]);
    assert_eq!(run.events[1], run.events[0]);
}

/// Input delay of the several-seat runs: a word passed at GGRS frame `k` lands on `k + SEAT_DELAY`.
const SEAT_DELAY: usize = 2;

/// The word controller `slot` sends when its device calls `advance` at GGRS frame `frame`:
/// random pad and turbo bits, different per controller.
fn seat_word(slot: usize, frame: i32) -> u32 {
    let seed = ((frame as u64 + 1) << 8) ^ (slot as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut rng = XorShift(seed | 1);
    rng.next();
    (rng.next() & RANDOM_INPUT) as u32
}

/// Every device's word for each of its controllers at its current frame (`seat_word`).
fn seat_words(s: &NetSession) -> Vec<u32> {
    let frame = s.ggrs.current_frame();
    s.local_slots().iter().map(|&slot| seat_word(slot, frame)).collect()
}

/// Polls and advances every session with `input(device, session)` until `done`; collects each
/// device's events.
fn drive_until(
    sessions: &mut [NetSession],
    emus: &mut [Console],
    mut input: impl FnMut(usize, &NetSession) -> Vec<u32>,
    done: impl Fn(&[NetSession]) -> bool,
) -> Vec<Vec<NetEvent>> {
    let mut events = vec![Vec::new(); sessions.len()];
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done(sessions) {
        assert!(Instant::now() < deadline, "timed out: final {:?}", sessions.iter().map(|s| s.final_through).collect::<Vec<_>>());
        for (i, s) in sessions.iter_mut().enumerate() {
            s.poll();
            let words = input(i, s);
            s.advance(&mut emus[i], &words).expect("advance");
            events[i].extend(s.events());
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    events
}

/// The devices of `slots` (peer ids 0..n) directly connected over loopback UDP, with a 1 s
/// disconnect timeout; one NES each.
fn direct_mesh(slots: &[PeerId], saves: SavePolicy) -> Option<(Vec<NetSession>, Vec<Console>)> {
    let first = nes()?;
    let n = slots.iter().map(|p| p.0 as usize + 1).max().unwrap();
    let socks: Vec<UdpSocket> = (0..n).map(|_| bind()).collect();
    let addrs: Vec<SocketAddr> = socks.iter().map(|s| s.local_addr().unwrap()).collect();
    let sessions = socks
        .into_iter()
        .enumerate()
        .map(|(i, sock)| {
            let peers: Vec<(PeerId, SocketAddr)> = (0..n).filter(|&j| j != i).map(|j| (PeerId(j as u8), addrs[j])).collect();
            let link = crate::Link::udp(sock, PeerId(i as u8), &peers);
            let (t, notify) = (Duration::from_secs(1), Duration::from_millis(300));
            NetSession::with_tuning(link, slots, SEAT_DELAY, false, 60, saves, max_prediction(slots.len()), t, notify).unwrap()
        })
        .collect();
    let emus = std::iter::once(first).chain((1..n).map(|_| nes().unwrap())).collect();
    Some((sessions, emus))
}

/// Devices sharing controllers per `slots` over the lossy network, each feeding `seat_word` for
/// its own controllers: every device simulates exactly the words each controller's player sent,
/// on that controller, and reaches the state of one plain console fed the same pads.
fn run_seats(slots: &[PeerId], seed: u64, probe: i32) {
    let Some(first) = nes() else { return };
    let n = slots.iter().map(|p| p.0 as usize + 1).max().unwrap();
    let (transports, _proxy) = lossy_mesh(n, seed, Profile::Lossy);
    let mut sessions: Vec<NetSession> = transports
        .into_iter()
        .map(|t| {
            let mut s = NetSession::new(t, slots, SEAT_DELAY, false, 60, None, SavePolicy::Everyone).expect("session");
            s.probe = (probe, None);
            s
        })
        .collect();
    let mut emus: Vec<Console> = std::iter::once(first).chain((1..n).map(|_| nes().unwrap())).collect();
    let events = drive_until(&mut sessions, &mut emus, |_, s| seat_words(s), |ss| ss.iter().all(|s| s.final_through > probe));
    let desyncs: Vec<&NetEvent> = events.iter().flatten().filter(|e| matches!(e, NetEvent::Desync { .. })).collect();
    assert!(desyncs.is_empty(), "desync events: {desyncs:?}");
    let log: Vec<[u32; 4]> = (0..=probe)
        .map(|f| std::array::from_fn(|slot| if slot < slots.len() && f >= SEAT_DELAY as i32 { seat_word(slot, f - SEAT_DELAY as i32) } else { 0 }))
        .collect();
    let expected = reference(&log, None).at_start[probe as usize];
    for (i, s) in sessions.iter().enumerate() {
        let first_wrong = (0..log.len()).find(|&f| s.input_log[f] != log[f]);
        assert_eq!(first_wrong, None, "device {i} simulated other words: {:x?}", first_wrong.map(|f| (s.input_log[f], log[f])));
        assert_eq!(s.probe.1, Some(expected), "device {i} state at frame {probe}");
    }
}

/// The host plays controllers 1 and 2, the guest controller 3: each local word reaches its own
/// controller on both machines.
#[test]
fn session_two_seats_on_one_device_reach_their_controllers() {
    run_seats(&[PeerId(0), PeerId(0), PeerId(1)], 0x5EA7_0001, 400);
}

/// Three devices, the middle one with two players, over the lossy network: equal states.
#[test]
fn session_two_seat_guest_among_three_devices_stays_in_sync() {
    run_seats(&[PeerId(0), PeerId(1), PeerId(1), PeerId(2)], 0x5EA7_0002, 400);
}

/// A device with two players leaving with BYE: the others keep playing in step, both of its
/// controllers read released, and each reports it gone once.
#[test]
fn session_two_seat_leaver_releases_both_controllers() {
    let slots = [PeerId(0), PeerId(1), PeerId(1), PeerId(2)];
    let Some((mut sessions, mut emus)) = direct_mesh(&slots, SavePolicy::Everyone) else { return };
    for s in &mut sessions {
        s.probe = (300, None);
    }
    drive_until(&mut sessions, &mut emus, |_, s| seat_words(s), |ss| ss.iter().all(|s| s.final_through >= 150));
    sessions.remove(1).leave();
    emus.remove(1);
    let events = drive_until(&mut sessions, &mut emus, |_, s| seat_words(s), |ss| ss.iter().all(|s| s.final_through > 300));
    for (i, e) in events.iter().enumerate() {
        let gone: Vec<&NetEvent> = e.iter().filter(|e| matches!(e, NetEvent::Disconnected { .. } | NetEvent::Desync { .. })).collect();
        assert_eq!(gone, [&NetEvent::Disconnected { peer: PeerId(1) }], "device {i} events");
    }
    let (a, b) = (&sessions[0], &sessions[1]);
    assert!(a.probe.1.is_some() && a.probe.1 == b.probe.1, "states at 300 differ: {:x?} / {:x?}", a.probe.1, b.probe.1);
    assert_eq!(a.input_log[..=300], b.input_log[..=300], "the remaining devices simulated other words");
    assert!(a.input_log[..150].iter().any(|w| w[1] != 0) && a.input_log[..150].iter().any(|w| w[2] != 0));
    for f in 250..=300 {
        let w = a.input_log[f];
        assert_eq!((w[1], w[2]), (0, 0), "the leaver's controllers still pressed on frame {f}");
    }
}

/// Host and guest each press save once (the guest at frame 60, the host at 160); returns the
/// `Saved` events of both devices once frame 330 is final, and the sessions.
fn saves_under(policy: SavePolicy) -> Option<(Vec<Vec<NetEvent>>, Vec<NetSession>)> {
    let (mut sessions, mut emus) = direct_mesh(&[PeerId(0), PeerId(1)], policy)?;
    let mut pressed = [false; 2];
    let input = |i: usize, s: &NetSession| {
        let at = if i == 0 { 160 } else { 60 };
        let mut words = seat_words(s);
        if s.is_running() && s.ggrs.current_frame() >= at && !std::mem::replace(&mut pressed[i], true) {
            words[0] |= INPUT_SAVE;
        }
        words
    };
    let events = drive_until(&mut sessions, &mut emus, input, |ss| ss.iter().all(|s| s.final_through >= 330));
    let saved = events.into_iter().map(|e| e.into_iter().filter(|e| matches!(e, NetEvent::Saved { .. })).collect()).collect();
    for s in &sessions {
        assert!(s.input_log[..330].iter().any(|w| w[1] & INPUT_SAVE != 0), "the guest's save never travelled");
    }
    Some((saved, sessions))
}

/// Host only: the guest's save does nothing anywhere, the host's saves for both.
#[test]
fn session_host_only_saves_ignore_the_guest() {
    let Some((saved, _)) = saves_under(SavePolicy::Host) else { return };
    assert_eq!(saved, [vec![NetEvent::Saved { slot: 0 }], vec![NetEvent::Saved { slot: 0 }]]);
}

/// Saves off: no save from anybody and no "last moment" checkpoint past frame 300.
#[test]
fn session_saves_off_saves_nothing() {
    let Some((saved, mut sessions)) = saves_under(SavePolicy::Off) else { return };
    assert_eq!(saved, [vec![], vec![]]);
    for s in &mut sessions {
        assert!(s.take_saved().is_none() && s.take_checkpoint().is_none());
    }
}
