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
    direct_pair_with(rom, "nestest.nes", guest_has_rom, 20);
}

/// Host and guest over loopback UDP in direct mode; the guest holds `rom` or receives it within
/// `secs`, then both start with the same bytes.
fn direct_pair_with(rom: Vec<u8>, rom_name: &str, guest_has_rom: bool, secs: u64) {
    let port = free_port();
    let mut host = Lobby::start(cfg(
        "Host",
        LobbyMode::HostDirect { port, rom: rom.clone(), rom_name: rom_name.into() },
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
    let deadline = Instant::now() + Duration::from_secs(secs);
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
        // `input_delay: None` on the host: Auto, announced as `"auto": true` to the guest.
        assert!(g.delay_auto);
        assert!(!g.four_score);
        assert!(g.state.is_none(), "no resume: power-on");
        assert_eq!(g.names, vec!["Host".to_string(), "Guest".to_string()]);
        assert_eq!(g.rom_name, rom_name);
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

/// A synthetic iNES image of 4 MB (255 × 16 KB PRG, 16 KB CHR, mapper 0, NTSC) with patterned
/// bytes: Mega Drive size, without any game in it.
fn big_rom() -> Vec<u8> {
    let mut rom = vec![b'N', b'E', b'S', 0x1A, 255, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    rom.extend(noise(255 * 16384 + 2 * 8192, 0xB16));
    rom
}

/// 4096 ROM chunks (64 windows) reach the guest over a real route, intact and verified.
#[test]
fn lobby_direct_transfers_four_megabyte_rom() {
    let rom = big_rom();
    assert_eq!(rom.len(), 4 * 1024 * 1024 + 16);
    let id = ebobo_console::identify(&rom).expect("a ROM");
    assert_eq!(id.region, Region::Ntsc);
    let started = Instant::now();
    direct_pair_with(rom, "big.nes", false, 60);
    println!("4 MB ROM handed over in {:?}", started.elapsed());
}

/// The `game` message carries the region as its one-byte code; a code no console has is a
/// malformed message: the guest fails, holds no game info and asks for no ROM.
#[test]
fn game_message_with_unknown_region_is_refused() {
    let msg = |region: Value| {
        json!({"t": "game", "proto": PROTOCOL_VERSION, "core": local_version("T"), "name": "H",
            "rom": {"crc32": "1a2b3c4d", "size": 16, "name": "t.nes"}, "region": region, "four_score": false})
    };
    for (code, region) in [(0, Region::Ntsc), (1, Region::Pal), (2, Region::Japan)] {
        assert_eq!(Region::from_code(region.code()), Some(region));
        let mut w = bare_worker(false, 1);
        w.on_game(&msg(json!(code)));
        assert_eq!(w.game.as_ref().map(|g| g.region), Some(region));
        assert_ne!(w.stage, Stage::Failed);
    }
    for bad in [json!(3), json!(255), json!(256), json!("ntsc"), Value::Null] {
        let mut w = bare_worker(false, 1);
        w.on_game(&msg(bad.clone()));
        assert_eq!(w.stage, Stage::Failed, "region {bad}");
        assert!(w.game.is_none());
        assert_eq!(w.my_rom, RomState::Unknown);
    }
}

/// Three players through the real server, optionally all relayed, optionally from a saved game:
/// every guest sees the host's choice before the start, and every `GameStart.state` is the
/// host's bytes (`None` for a new game). Prints the time from pressing start to the hand-over.
fn live_three(force_relay: bool, resume: Option<Resume>) {
    let Some(rom) = nestest() else { return };
    let with = |c: LobbyConfig| LobbyConfig { force_relay, ..c };
    let mut host = Lobby::start(with(cfg(
        "Host",
        LobbyMode::HostRoom { rom: rom.clone(), rom_name: "nestest.nes".into() },
        Box::new(|_| None),
    )));
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
    let info = resume.as_ref().map(|r| ResumeInfo { kind: r.kind, saved_at: r.saved_at });
    let state = resume.as_ref().map(|r| r.state.clone());
    host.set_resume(resume);
    let r1 = rom.clone();
    let mut g1 =
        Lobby::start(with(cfg("G1", LobbyMode::JoinRoom { code: code.clone() }, Box::new(move |_| Some(r1.clone())))));
    let mut g2 = Lobby::start(with(cfg("G2", LobbyMode::JoinRoom { code }, Box::new(|_| None))));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let st = [host.status(), g1.status(), g2.status()];
        assert!(st.iter().all(|s| s.error.is_none()), "{st:#?}");
        if st[0].can_start && st.iter().all(|s| s.players.len() == 3 && s.resume == info) {
            break;
        }
        assert!(Instant::now() < deadline, "not startable: {st:#?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    let pressed = Instant::now();
    host.start_game();
    let games = take_all(&mut [&mut host, &mut g1, &mut g2], 15);
    println!(
        "relay={force_relay} state={:?} bytes: start to hand-over {:?}",
        state.as_ref().map(Vec::len),
        pressed.elapsed()
    );
    for g in &games {
        assert_eq!(g.slots, vec![PeerId(0), PeerId(1), PeerId(2)]);
        assert!(g.four_score);
        assert_eq!(g.rom, rom);
        assert!(g.state == state, "state differs from the host's");
        if force_relay {
            assert!(g.slots.iter().filter(|p| **p != g.me).all(|p| g.link.route_kind(*p) == Some(RouteKind::Relay)));
        }
    }
    host.leave();
    g1.leave();
    g2.leave();
}

/// Deterministic pseudo-random bytes (xorshift64): a saved game without structure.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect()
}

fn live_resume() -> Resume {
    Resume { kind: ResumeKind::Last, saved_at: 1_763_100_000, state: noise(60 * 1024, 0x5EED) }
}

#[test]
#[ignore]
fn lobby_live_three_players() {
    live_three(false, None);
}

#[test]
#[ignore]
fn lobby_live_three_players_resume_direct() {
    live_three(false, Some(live_resume()));
}

#[test]
#[ignore]
fn lobby_live_three_players_resume_relay() {
    live_three(true, Some(live_resume()));
}

/// Direct-IP mode starts from a save like a room does: the guest shows the host's choice and
/// both sides get the host's bytes.
#[test]
fn lobby_direct_starts_from_resume() {
    let Some(rom) = nestest() else { return };
    let port = free_port();
    let mut host = Lobby::start(cfg(
        "Host",
        LobbyMode::HostDirect { port, rom: rom.clone(), rom_name: "nestest.nes".into() },
        Box::new(|_| None),
    ));
    let resume = Resume { kind: ResumeKind::Save, saved_at: 42, state: noise(70_001, 7) };
    host.set_resume(Some(resume.clone()));
    let r2 = rom.clone();
    let mut guest = Lobby::start(cfg(
        "Guest",
        LobbyMode::JoinDirect { addr: format!("127.0.0.1:{port}") },
        Box::new(move |_| Some(r2.clone())),
    ));
    let info = Some(ResumeInfo { kind: ResumeKind::Save, saved_at: 42 });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (h, g) = (host.status(), guest.status());
        assert!(h.error.is_none() && g.error.is_none(), "{h:#?} {g:#?}");
        if h.can_start && h.resume == info && g.resume == info {
            break;
        }
        assert!(Instant::now() < deadline, "not startable from the save: {h:#?} {g:#?}");
        std::thread::sleep(Duration::from_millis(1));
    }
    host.start_game();
    let games = take_all(&mut [&mut host, &mut guest], 15);
    for g in &games {
        assert!(g.state.as_deref() == Some(&resume.state[..]), "state differs from the host's");
        assert_eq!(g.resume, info, "the start names what the state is");
    }
    host.leave();
    guest.leave();
}

fn blank_status() -> LobbyStatus {
    LobbyStatus {
        stage: Stage::Connecting,
        code: None,
        me: None,
        is_host: false,
        players: Vec::new(),
        rom_name: None,
        can_start: false,
        error: None,
        resume: None,
        order: Vec::new(),
        saves: SavePolicy::Everyone,
        regrouped: false,
    }
}

/// A worker without a mesh (nothing is sent): the lobby logic driven by hand.
fn bare_worker(is_host: bool, me: u8) -> Worker {
    let (tx, _rx) = mpsc::channel();
    let mut w = Worker::new(Arc::new(Mutex::new(blank_status())), tx, "T".into(), None, Box::new(|_| None), is_host);
    w.me = Some(PeerId(me));
    w.stage = Stage::Lobby;
    w
}

/// Host with `guests` routed, ready and linked to everyone: startable.
fn host_worker(guests: &[u8]) -> Worker {
    let mut w = bare_worker(true, 0);
    w.host_rom =
        Some(HostRom { bytes: vec![0; 16], name: "t.nes".into(), crc: 0, region: Region::Ntsc, four_score: false });
    for &g in guests {
        add_guest(&mut w, g, guests);
    }
    assert!(w.can_start());
    w
}

fn add_guest(w: &mut Worker, g: u8, guests: &[u8]) {
    let mut p = Peer::new();
    p.route = Some(RouteKind::Direct);
    p.ready = true;
    p.links = [0].iter().chain(guests).copied().filter(|o| *o != g).collect();
    w.peers.insert(PeerId(g), p);
    w.admit(PeerId(g));
}

/// Guest with the ROM and game info, so a `start` gets as far as the saved game.
fn guest_worker() -> Worker {
    let mut w = bare_worker(false, 1);
    w.rom = Some(vec![0; 16]);
    w.game = Some(GameInfo { crc: 0, rom_name: "t.nes".into(), region: Region::Ntsc });
    w
}

fn chunk_packet(id: u32, state: &[u8], crc: u32, index: usize) -> Vec<u8> {
    let data = state_chunk(state, index);
    Packet::StateChunk { id, total: state.len() as u32, crc, index: index as u16, data }.to_vec()
}

/// The `StateAck` of a guest holding every chunk of the transfer under way.
fn full_ack(w: &Worker) -> Vec<u8> {
    let o = w.offer.as_ref().expect("transfer under way");
    let have = vec![0xFF; state_chunks(o.state.len()).div_ceil(8)];
    Packet::StateAck { id: o.id, have: &have }.to_vec()
}

fn start_msg(id: u32, size: usize, crc: u32) -> Value {
    json!({"t": "start", "slots": [0, 1], "delay": 2, "auto": false, "four_score": false,
        "state": {"id": id, "size": size, "crc32": format!("{crc:08x}")}})
}

#[test]
fn state_packets_round_trip_and_fit() {
    let data = [7u8; STATE_CHUNK_DATA];
    let p = Packet::StateChunk { id: 9, total: MAX_STATE as u32, crc: 0xDEAD_BEEF, index: 1023, data: &data };
    let bytes = p.to_vec();
    assert!(bytes.len() <= crate::packet::MAX_PACKET, "{}", bytes.len());
    assert_eq!(Packet::parse(&bytes), Some(p));
    let have = [0xFFu8; MAX_STATE / STATE_CHUNK_DATA / 8];
    let a = Packet::StateAck { id: 9, have: &have };
    assert!(a.to_vec().len() <= crate::packet::MAX_PACKET);
    assert_eq!(Packet::parse(&a.to_vec()), Some(a));
    // A chunk over STATE_CHUNK_DATA or without data is malformed.
    let mut big = bytes.clone();
    big.push(0);
    assert_eq!(Packet::parse(&big), None);
    assert_eq!(Packet::parse(&bytes[..16]), None);
}

/// The transfer over a link that drops 25 %, duplicates 10 % and delays 0-80 ms (reordering)
/// both ways delivers the exact bytes, verified, at every size up to `MAX_STATE`.
#[test]
fn state_transfer_survives_loss_duplicates_and_reordering() {
    for (n, &len) in [1usize, STATE_CHUNK_DATA, STATE_CHUNK_DATA + 1, 60_000, MAX_STATE].iter().enumerate() {
        let state = noise(len, 0xABCD + n as u64);
        let crc = ebobo_console::crc32(&[&state]);
        let mut rng = 0x1234_5678_9ABC_DEF0u64 + n as u64;
        let mut roll = move |m: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % m
        };
        let mut guest = guest_worker();
        let mut tx = StateTx::new(state_chunks(len));
        let t0 = Instant::now();
        let mut to_guest: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut to_host: Vec<(u64, Vec<u8>)> = Vec::new();
        let (mut sent, mut ms) = (0usize, 0u64);
        while !tx.done() {
            assert!(ms < 60_000, "len {len}: not delivered in 60 simulated s");
            let now = t0 + Duration::from_millis(ms);
            for i in tx.due(now, Duration::from_millis(200)) {
                sent += 1;
                for _ in 0..1 + (roll(10) == 0) as usize {
                    if roll(4) != 0 {
                        to_guest.push((ms + roll(80), chunk_packet(3, &state, crc, i as usize)));
                    }
                }
            }
            let (now_in, later): (Vec<_>, Vec<_>) = to_guest.drain(..).partition(|(at, _)| *at <= ms);
            to_guest = later;
            for (_, pkt) in now_in {
                guest.on_packet(PeerId(0), &pkt);
                assert!(guest.error.is_none(), "len {len}: {:?}", guest.error);
                let ack = guest.state_rx.as_ref().unwrap().ack();
                if roll(4) != 0 {
                    to_host.push((ms + roll(80), Packet::StateAck { id: 3, have: &ack }.to_vec()));
                }
            }
            let (now_in, later): (Vec<_>, Vec<_>) = to_host.drain(..).partition(|(at, _)| *at <= ms);
            to_host = later;
            for (_, pkt) in now_in {
                let Some(Packet::StateAck { id: 3, have }) = Packet::parse(&pkt) else { panic!() };
                tx.on_ack(have);
            }
            ms += 4;
        }
        let chunks = state_chunks(len);
        assert!(sent <= chunks * 3 + STATE_WINDOW, "len {len}: {sent} sends for {chunks} chunks");
        assert_eq!(guest.take_state(&start_msg(3, len, crc)["state"]), Some(state), "len {len}");
    }
}

/// On a clean link every chunk goes out once: a full window at once, the rest as acks free it.
#[test]
fn state_transfer_clean_link_sends_each_chunk_once() {
    let mut tx = StateTx::new(100);
    let t = Instant::now();
    let rto = Duration::from_millis(200);
    assert_eq!(tx.due(t, rto), (0..STATE_WINDOW as u16).collect::<Vec<_>>());
    assert!(tx.due(t + Duration::from_millis(100), rto).is_empty(), "window full, nothing timed out");
    let mut have = vec![0u8; 13];
    have[..8].fill(0xFF);
    tx.on_ack(&have);
    assert_eq!(tx.due(t + Duration::from_millis(110), rto), (64..100).collect::<Vec<_>>());
    tx.on_ack(&[0xFF; 13]);
    assert!(tx.done());
    assert!(tx.due(t + Duration::from_secs(5), rto).is_empty());
}

/// A state whose bytes do not match the host's CRC-32, a start announcing a state the guest does
/// not fully hold, or one of another size: the guest fails with `SaveTransfer` and hands nothing over.
#[test]
fn state_damaged_or_truncated_is_refused() {
    let state = noise(5000, 11);
    let crc = ebobo_console::crc32(&[&state]);
    let chunks = state_chunks(state.len());

    let mut damaged = state.clone();
    damaged[4100] ^= 1;
    let mut w = guest_worker();
    for i in 0..chunks {
        w.on_packet(PeerId(0), &chunk_packet(1, &damaged, crc, i));
    }
    assert_eq!((w.stage, w.error.clone()), (Stage::Failed, Some(NetError::SaveTransfer)));

    let mut w = guest_worker();
    for i in 0..chunks - 1 {
        w.on_packet(PeerId(0), &chunk_packet(1, &state, crc, i));
    }
    // A chunk of the wrong size for its index is not stored.
    w.on_packet(PeerId(0), &Packet::StateChunk { id: 1, total: 5000, crc, index: 4, data: &[0; 10] }.to_vec());
    assert!(w.error.is_none());
    w.on_ctrl(PeerId(0), &start_msg(1, state.len(), crc));
    assert_eq!((w.stage, w.error.clone()), (Stage::Failed, Some(NetError::SaveTransfer)), "truncated");

    let mut w = guest_worker();
    for i in 0..chunks {
        w.on_packet(PeerId(0), &chunk_packet(1, &state, crc, i));
    }
    assert!(w.error.is_none() && w.state_rx.as_ref().is_some_and(|r| r.verified));
    w.on_ctrl(PeerId(0), &start_msg(1, state.len() + 1, crc));
    assert_eq!(w.error, Some(NetError::SaveTransfer), "announced size differs");

    // A start without a state is a new game, whatever arrived before.
    let mut w = guest_worker();
    w.on_packet(PeerId(0), &chunk_packet(1, &state, crc, 0));
    let mut plain = start_msg(1, 0, 0);
    plain.as_object_mut().unwrap().remove("state");
    w.on_ctrl(PeerId(0), &plain);
    assert!(w.error.is_none() && w.state_rx.is_none());
}

/// Over `MAX_STATE`: a guest refuses the first chunk, a host does not start a transfer.
#[test]
fn state_over_one_mebibyte_is_refused() {
    let mut w = guest_worker();
    let pkt = Packet::StateChunk { id: 1, total: MAX_STATE as u32 + 1, crc: 0, index: 0, data: &[0; 1024] };
    w.on_packet(PeerId(0), &pkt.to_vec());
    assert_eq!((w.stage, w.error.clone()), (Stage::Failed, Some(NetError::SaveTransfer)));
    assert!(w.state_rx.is_none());

    let mut h = host_worker(&[1]);
    h.set_resume(Some(Resume { kind: ResumeKind::Save, saved_at: 1, state: vec![0; MAX_STATE + 1] }));
    h.start_game();
    assert!(h.offer.is_none() && h.pending_start.is_none());
    assert_eq!((h.stage, h.error.clone()), (Stage::Lobby, Some(NetError::SaveTransfer)));
    assert!(h.can_start());
}

/// A guest without the whole state 10 s after start: the start is called off, the host reports
/// `SaveTransfer` and stays startable; `set_resume` waits for that; the next start goes out.
#[test]
fn start_called_off_after_ten_seconds_leaves_host_startable() {
    let mut h = host_worker(&[1, 2]);
    let first = Resume { kind: ResumeKind::Last, saved_at: 5, state: noise(3000, 1) };
    h.set_resume(Some(first.clone()));
    h.start_game();
    let deadline = h.offer.as_ref().expect("transfer under way").deadline;
    assert!(!h.can_start());
    h.set_resume(None);
    assert_eq!(h.resume, Some(first.clone()), "set_resume during a start is ignored");
    h.on_packet(PeerId(1), &full_ack(&h));
    h.timers(deadline - Duration::from_millis(1));
    assert!(h.offer.is_some() && h.pending_start.is_none() && h.error.is_none());
    h.timers(deadline);
    assert!(h.offer.is_none() && h.pending_start.is_none());
    assert_eq!((h.stage, h.error.clone()), (Stage::Lobby, Some(NetError::SaveTransfer)));
    assert!(h.can_start());

    let second = Resume { kind: ResumeKind::Save, saved_at: 6, state: noise(2500, 2) };
    h.set_resume(Some(second.clone()));
    assert_eq!(h.resume, Some(second.clone()));
    h.start_game();
    assert!(h.error.is_none());
    let id = h.offer.as_ref().unwrap().id;
    // Acks of the called-off transfer count for nothing.
    h.on_packet(PeerId(2), &Packet::StateAck { id: id - 1, have: &[0xFF; 1] }.to_vec());
    h.timers(Instant::now());
    assert!(h.offer.is_some() && h.pending_start.is_none());
    for g in [1, 2] {
        h.on_packet(PeerId(g), &full_ack(&h));
    }
    // `drive_offer` alone: without a mesh `check_start` would hand over (to nobody) at once.
    h.drive_offer(Instant::now());
    let ps = h.pending_start.as_ref().expect("start sent");
    assert_eq!(ps.state.as_ref(), Some(&second.state));
    assert!(h.offer.is_none());
}

/// A guest leaving during the transfer does not hold the start back: it goes out to the rest.
#[test]
fn guest_leaving_mid_transfer_does_not_block_start() {
    let mut h = host_worker(&[1, 2]);
    h.set_resume(Some(Resume { kind: ResumeKind::Last, saved_at: 5, state: noise(9000, 3) }));
    h.start_game();
    h.on_packet(PeerId(1), &full_ack(&h));
    h.on_packet(PeerId(2), &Packet::StateAck { id: h.offer.as_ref().unwrap().id, have: &[1] }.to_vec());
    h.timers(Instant::now());
    assert!(h.offer.is_some());
    assert!(h.pending_start.is_none());
    h.on_event(MeshEvent::PeerLeft { id: PeerId(2) });
    h.drive_offer(Instant::now());
    let ps = h.pending_start.as_ref().expect("start sent without the leaver");
    assert_eq!(ps.slots, vec![PeerId(0), PeerId(1)]);
}

/// A guest refusing the state (`error save`) calls the start off at once; the lobby stays usable.
#[test]
fn guest_refusing_state_calls_start_off() {
    let mut h = host_worker(&[1]);
    h.set_resume(Some(Resume { kind: ResumeKind::Last, saved_at: 5, state: noise(900, 4) }));
    h.start_game();
    h.on_ctrl(PeerId(1), &json!({"t": "error", "msg": "save"}));
    assert!(h.offer.is_none() && h.pending_start.is_none());
    assert_eq!((h.stage, h.error.clone()), (Stage::Lobby, Some(NetError::SaveTransfer)));
}

/// The host's choice travels in `players`: a guest shows it, follows a change and a clear, and
/// a guest that joins later gets the current one.
#[test]
fn guests_see_the_hosts_resume_choice() {
    let mut h = host_worker(&[1]);
    let mut g = guest_worker();
    let deliver = |h: &mut Worker, g: &mut Worker, to: u8| {
        h.broadcast_players();
        let text = h.peers[&PeerId(to)].sent_players.clone().expect("players sent");
        g.on_ctrl(PeerId(0), &serde_json::from_str::<Value>(&text).unwrap());
        g.snapshot().resume
    };
    assert_eq!(deliver(&mut h, &mut g, 1), None);
    h.set_resume(Some(Resume { kind: ResumeKind::Last, saved_at: 100, state: vec![1] }));
    assert_eq!(deliver(&mut h, &mut g, 1), Some(ResumeInfo { kind: ResumeKind::Last, saved_at: 100 }));
    h.set_resume(Some(Resume { kind: ResumeKind::Save, saved_at: 200, state: vec![2] }));
    let save = Some(ResumeInfo { kind: ResumeKind::Save, saved_at: 200 });
    assert_eq!(deliver(&mut h, &mut g, 1), save);
    assert_eq!(h.snapshot().resume, save);

    add_guest(&mut h, 2, &[1, 2]);
    let mut late = guest_worker();
    assert_eq!(deliver(&mut h, &mut late, 2), save, "late joiner");

    h.set_resume(None);
    assert_eq!(deliver(&mut h, &mut g, 1), None);
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
    assert_eq!(c.rtt.samples as usize, samples.len());
    auto_input_delay(c.delay_rtt().unwrap_or(0.0), NTSC_MS)
}

#[test]
fn lobby_delay_lan_jitter() {
    let s: Vec<f64> = (0..16).map(|i| 33.0 + [3.0, -3.0, 0.0, 2.0][i % 4]).collect();
    let d = delay_after(&s);
    assert!((1..=2).contains(&d), "33 ± 3 ms, 16 samples: {d}");
}

#[test]
fn lobby_delay_hides_three_frames_and_leaves_six_to_rollback() {
    let d = |rtt: f64| auto_input_delay(rtt, NTSC_MS);
    // Up to 3 frames one way the delay hides the latency: no rollbacks.
    assert_eq!([d(0.0), d(30.0), d(60.0), d(99.0)], [1, 1, 2, 3]);
    // The next 6 frames go to prediction: the delay stays at 3.
    assert_eq!([d(120.0), d(200.0), d(299.0)], [3, 3, 3]);
    // Past 9 frames one way it grows again, up to 6.
    assert_eq!([d(300.0), d(340.0), d(370.0), d(600.0)], [4, 5, 6, 6]);
}

#[test]
fn lobby_delay_steady_120() {
    assert_eq!(delay_after(&[120.0; 16]), 3);
}

#[test]
fn lobby_delay_cold_start_not_inflated() {
    // RFC 6298 cold start (rttvar = R / 2) would plan for 240 ms after one 120 ms sample.
    let mut c = Peer::new().ctrl;
    c.add_sample(120.0);
    assert_eq!(c.delay_rtt(), Some(150.0));
    // Warm-up boundary: 7 samples use srtt * 1.25, the 8th switches to srtt + 2 * rttvar.
    let mut c = Peer::new().ctrl;
    for _ in 0..7 {
        c.add_sample(120.0);
    }
    assert_eq!(c.delay_rtt(), Some(150.0));
    c.add_sample(120.0);
    assert_eq!(c.delay_rtt(), Some(c.rtt.srtt.unwrap() + 2.0 * c.rtt.rttvar));
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
