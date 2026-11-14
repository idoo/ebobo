use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const LIVE_SERVER: &str = "https://ebobo-signal.mc-se.workers.dev";

/// `None` (after a notice) when the test ROMs were not fetched.
fn nestest() -> Option<Vec<u8>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ebobo-core/tests/roms/nestest/nestest.nes");
    let rom = std::fs::read(path).ok();
    if rom.is_none() {
        println!("test ROMs missing: run scripts/fetch-test-roms.sh");
    }
    rom
}

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn cfg(nick: &str, mode: LobbyMode, lookup: Box<dyn Fn(u32) -> Option<Vec<u8>> + Send>) -> LobbyConfig {
    LobbyConfig {
        server: LIVE_SERVER.into(),
        nickname: nick.into(),
        mode,
        force_relay: false,
        input_delay: None,
        rom_lookup: lookup,
    }
}

fn take_all(lobbies: &mut [&mut Lobby], secs: u64) -> Vec<GameStart> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut games: Vec<Option<GameStart>> = lobbies.iter().map(|_| None).collect();
    while games.iter().any(Option::is_none) {
        for (l, g) in lobbies.iter_mut().zip(games.iter_mut()) {
            if g.is_none() {
                *g = l.take_game();
            }
        }
        let st: Vec<LobbyStatus> = lobbies.iter().map(|l| l.status()).collect();
        assert!(Instant::now() < deadline, "no game: {st:#?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    games.into_iter().map(Option::unwrap).collect()
}

fn direct_pair(guest_has_rom: bool) {
    let Some(rom) = nestest() else { return };
    let port = free_port();
    let mut host = Lobby::start(cfg(
        "Host",
        LobbyMode::HostDirect { port, rom: rom.clone(), rom_name: "nestest.nes".into() },
        Box::new(|_| None),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let (r2, c2) = (rom.clone(), calls.clone());
    let mut guest = Lobby::start(cfg(
        "Guest",
        LobbyMode::JoinDirect { addr: format!("127.0.0.1:{port}") },
        Box::new(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
            guest_has_rom.then(|| r2.clone())
        }),
    ));
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen_receiving = false;
    loop {
        let (h, g) = (host.status(), guest.status());
        assert!(h.error.is_none() && g.error.is_none(), "{h:#?} {g:#?}");
        if h.players.iter().chain(&g.players).any(|p| matches!(p.rom, RomState::Receiving { .. })) {
            seen_receiving = true;
        }
        if h.can_start {
            assert_eq!(h.players.len(), 2, "{h:#?}");
            break;
        }
        assert!(Instant::now() < deadline, "can_start never true: {h:#?} {g:#?}");
        std::thread::sleep(Duration::from_millis(1));
    }
    println!("seen_receiving={seen_receiving} guest_has_rom={guest_has_rom}");
    assert_eq!(seen_receiving, !guest_has_rom);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    host.start_game();
    let games = take_all(&mut [&mut host, &mut guest], 10);
    let (hg, gg) = (&games[0], &games[1]);
    assert_eq!(hg.me, PeerId(0));
    assert_eq!(gg.me, PeerId(1));
    for g in &games {
        assert_eq!(g.slots, vec![PeerId(0), PeerId(1)]);
        assert_eq!(g.rom, rom);
        assert_eq!(g.region, Region::Ntsc);
        assert!(g.input_delay >= 1);
        assert!(!g.four_score);
        assert_eq!(g.names, vec!["Host".to_string(), "Guest".to_string()]);
        assert_eq!(g.rom_name, "nestest.nes");
    }
    assert_eq!(hg.input_delay, gg.input_delay);
    assert_eq!(host.status().stage, Stage::Starting);
    assert!(host.take_game().is_none());
    host.leave();
    guest.leave();
}

#[test]
fn lobby_direct_transfers_rom() {
    direct_pair(false);
}

#[test]
fn lobby_direct_guest_has_rom() {
    direct_pair(true);
}

#[test]
#[ignore]
fn lobby_live_three_players() {
    let Some(rom) = nestest() else { return };
    let mut host = Lobby::start(cfg(
        "Host",
        LobbyMode::HostRoom { rom: rom.clone(), rom_name: "nestest.nes".into() },
        Box::new(|_| None),
    ));
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        let s = host.status();
        assert!(s.error.is_none(), "{s:#?}");
        if let Some(c) = s.code {
            break c;
        }
        assert!(Instant::now() < deadline, "no room code");
        std::thread::sleep(Duration::from_millis(20));
    };
    println!("room {code}");
    let r1 = rom.clone();
    let mut g1 = Lobby::start(cfg("G1", LobbyMode::JoinRoom { code: code.clone() }, Box::new(move |_| Some(r1.clone()))));
    let mut g2 = Lobby::start(cfg("G2", LobbyMode::JoinRoom { code }, Box::new(|_| None)));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let st = [host.status(), g1.status(), g2.status()];
        assert!(st.iter().all(|s| s.error.is_none()), "{st:#?}");
        if st[0].can_start && st.iter().all(|s| s.players.len() == 3) {
            break;
        }
        assert!(Instant::now() < deadline, "not startable: {st:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    host.start_game();
    let games = take_all(&mut [&mut host, &mut g1, &mut g2], 10);
    for g in &games {
        assert_eq!(g.slots, vec![PeerId(0), PeerId(1), PeerId(2)]);
        assert!(g.four_score);
        assert_eq!(g.rom, rom);
    }
    host.leave();
    g1.leave();
    g2.leave();
}

/// Direct mode (no server): a guest on another ebobo version refuses the host's `game`
/// (`NetError::Version`) and tells the host, which reports it too and never can start.
#[test]
fn direct_version_mismatch_refused() {
    let Some(rom) = nestest() else { return };
    let port = free_port();
    let host = Lobby::start(cfg(
        "Host",
        LobbyMode::HostDirect { port, rom: rom.clone(), rom_name: "nestest.nes".into() },
        Box::new(|_| None),
    ));
    let guest = Lobby::start(cfg(
        "OldVer Guest",
        LobbyMode::JoinDirect { addr: format!("127.0.0.1:{port}") },
        Box::new(move |_| Some(rom.clone())),
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (h, g) = (host.status(), guest.status());
        assert!(!h.can_start, "{h:#?}");
        if g.error == Some(NetError::Version) && h.error == Some(NetError::Version) {
            assert_eq!(g.stage, Stage::Failed);
            break;
        }
        assert!(Instant::now() < deadline, "no version refusal: {h:#?} {g:#?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    host.leave();
    guest.leave();
}

#[test]
fn version_rules() {
    let mine = env!("CARGO_PKG_VERSION");
    let proto = Some(u64::from(PROTOCOL_VERSION));
    assert!(same_version(proto, Some(mine), mine));
    assert!(!same_version(proto, Some("0.0.0-oldver"), mine));
    assert!(!same_version(proto, None, mine), "a guest without `ver` is an older build");
    assert!(!same_version(Some(u64::from(PROTOCOL_VERSION) + 1), Some(mine), mine));
    assert_eq!(local_version("Host"), mine);
    assert_eq!(local_version("OldVer x"), "0.0.0-oldver");
}

const NTSC_MS: f64 = 16.64;

/// Auto input delay for one peer that produced `samples` (ms).
fn delay_after(samples: &[f64]) -> usize {
    let mut c = Peer::new().ctrl;
    for &s in samples {
        c.add_sample(s);
    }
    assert_eq!(c.samples as usize, samples.len());
    auto_input_delay(c.delay_rtt().unwrap_or(0.0), NTSC_MS)
}

#[test]
fn lobby_delay_lan_jitter() {
    let s: Vec<f64> = (0..16).map(|i| 33.0 + [3.0, -3.0, 0.0, 2.0][i % 4]).collect();
    let d = delay_after(&s);
    assert!((1..=2).contains(&d), "33 ± 3 ms, 16 samples: {d}");
}

#[test]
fn lobby_delay_steady_120() {
    let d = delay_after(&[120.0; 16]);
    assert!((4..=5).contains(&d), "steady 120 ms: {d}");
}

#[test]
fn lobby_delay_cold_start_not_inflated() {
    // RFC 6298 cold start (rttvar = R / 2) would plan for 240 ms → 8 → cap 6.
    let d = delay_after(&[120.0]);
    assert!(d < 6, "one 120 ms sample: {d}");
    assert!(d >= 4, "one 120 ms sample still covers the one-way latency: {d}");
    // Warm-up boundary: 7 samples use srtt * 1.25, the 8th switches to srtt + 2 * rttvar.
    let mut c = Peer::new().ctrl;
    for _ in 0..7 {
        c.add_sample(120.0);
    }
    assert_eq!(c.delay_rtt(), Some(150.0));
    c.add_sample(120.0);
    assert_eq!(c.delay_rtt(), Some(c.rtt.unwrap() + 2.0 * c.rttvar));
}

#[test]
fn lobby_delay_jittery_300_caps() {
    let s: Vec<f64> = (0..16).map(|i| if i % 2 == 0 { 220.0 } else { 380.0 }).collect();
    assert_eq!(delay_after(&s), 6);
    assert_eq!(delay_after(&[300.0]), 6);
}

#[test]
fn lobby_delay_no_samples_never_blocks() {
    assert_eq!(delay_after(&[]), 1);
}

#[test]
fn lobby_ctrl_rto_tracks_rtt() {
    let mut c = Peer::new().ctrl;
    assert_eq!(c.rto(), CTRL_RETRY);
    c.add_sample(300.0);
    // 300 + 4 * 150 = 900 ms: a 300 ms link is not retransmitted before its ack (Karn).
    assert_eq!(c.rto(), Duration::from_millis(900));
    c.add_sample(5000.0);
    assert_eq!(c.rto(), Duration::from_secs(1));
}
