//! Save states: `save_state` → `load_state` into a fresh console resumes bit-identically, and
//! foreign, truncated or garbled blobs are refused without panicking or touching the machine.

mod common;

use ebobo_core::{Nes, StateError};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pads(&mut self) -> [u8; 4] {
        let v = self.next();
        [v as u8, (v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8]
    }
}

/// A fetched test ROM; `None` when the test ROMs were not fetched.
fn file(path: &str) -> Option<Vec<u8>> {
    Some(std::fs::read(common::roms()?.join(path)).unwrap_or_else(|e| panic!("{path}: {e}")))
}

/// iNES ROM for `mapper` with 128 KiB PRG and 128 KiB CHR. Every 8 KiB PRG bank holds the
/// same program at $x000 offset $1000 (so any bank mapped at $E000 runs it) plus its bank
/// number at offset 0. The program enables NMI and rendering, then loops: reads pad 1, mixes
/// it into a running value, writes that value to every register in `regs`, and accumulates
/// reads of $8000/$A000/$C000 into RAM, so bank, IRQ and audio state all evolve.
fn synthetic(mapper: u8, regs: &[u16]) -> Vec<u8> {
    const BASE: u16 = 0xF000;
    let mut code: Vec<u8> = vec![
        0x78, 0xD8, 0xA2, 0xFF, 0x9A, // SEI CLD LDX #$FF TXS
        0xA9, 0x80, 0x8D, 0x00, 0x20, // LDA #$80 STA $2000
        0xA9, 0x1E, 0x8D, 0x01, 0x20, // LDA #$1E STA $2001
    ];
    let lp = BASE + code.len() as u16;
    code.extend_from_slice(&[
        0xA9, 0x01, 0x8D, 0x16, 0x40, 0xA9, 0x00, 0x8D, 0x16, 0x40, // strobe pads
        0xA2, 0x08, // LDX #8
        0xAD, 0x16, 0x40, 0x4A, 0x26, 0x10, 0xCA, 0xD0, 0xF7, // LDA $4016 LSR ROL $10 DEX BNE
        0xA5, 0x11, 0x0A, 0x65, 0x10, 0x45, 0x12, 0x85, 0x11, 0xE6, 0x12, // mix into $11
    ]);
    for (i, r) in regs.iter().enumerate() {
        let [lo, hi] = r.to_le_bytes();
        // LDA $11 ADC #k STA $11 STA reg
        code.extend_from_slice(&[0xA5, 0x11, 0x69, (i as u8).wrapping_mul(37) | 1, 0x85, 0x11]);
        code.extend_from_slice(&[0x8D, lo, hi]);
    }
    for hi in [0x80, 0xA0, 0xC0] {
        code.extend_from_slice(&[0xAD, 0x00, hi, 0x65, 0x13, 0x85, 0x13]); // acc into $13
    }
    let [l, h] = lp.to_le_bytes();
    code.extend_from_slice(&[0x4C, l, h]);
    let nmi = BASE + code.len() as u16;
    code.extend_from_slice(&[0xE6, 0x14, 0x40]); // INC $14 RTI
    let irq = BASE + code.len() as u16;
    code.extend_from_slice(&[0xE6, 0x15, 0x40]); // INC $15 RTI
    assert!(code.len() < 0xFF0);

    let mut rom = vec![b'N', b'E', b'S', 0x1A, 8, 16, (mapper & 0x0F) << 4, mapper & 0xF0];
    rom.resize(16, 0);
    for bank in 0..16u8 {
        let mut b = vec![bank; 0x2000];
        b[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        b[0x1FFA..0x1FFC].copy_from_slice(&nmi.to_le_bytes());
        b[0x1FFC..0x1FFE].copy_from_slice(&BASE.to_le_bytes());
        b[0x1FFE..0x2000].copy_from_slice(&irq.to_le_bytes());
        rom.extend_from_slice(&b);
    }
    let mut rng = Rng(0xC0FFEE ^ mapper as u64);
    rom.extend((0..128 * 1024).map(|_| rng.next() as u8));
    rom
}

fn fme7() -> Vec<u8> {
    synthetic(69, &[0x8000, 0xA000, 0x8000, 0xA000, 0x8000, 0xA000, 0xC000, 0xE000, 0xC000, 0xE000])
}

fn vrc6() -> Vec<u8> {
    synthetic(24, &[
        0x8000, 0xC000, 0xB003, 0xD000, 0xD001, 0xD002, 0xD003, 0xE000, 0xE001, 0xE002, 0xE003,
        0x9000, 0x9001, 0x9002, 0xA000, 0xA001, 0xA002, 0xB000, 0xB001, 0xB002, 0xF000, 0xF001,
        0xF002,
    ])
}

fn n163() -> Vec<u8> {
    synthetic(19, &[
        0x8000, 0x8800, 0x9000, 0x9800, 0xA000, 0xA800, 0xB000, 0xB800, 0xC000, 0xE000, 0xE800,
        0xF000, 0xF800, 0x4800, 0x5000, 0x5800,
    ])
}

fn mmc5() -> Vec<u8> {
    synthetic(5, &[
        0x5100, 0x5101, 0x5102, 0x5103, 0x5104, 0x5105, 0x5106, 0x5107, 0x5113, 0x5114, 0x5115,
        0x5116, 0x5120, 0x5123, 0x5127, 0x5128, 0x512B, 0x5130, 0x5200, 0x5203, 0x5204, 0x5205,
        0x5206, 0x5C10, 0x5000, 0x5002, 0x5003, 0x5015, 0x5011,
    ])
}

/// Hash of the state after every frame plus the final picture.
fn run(nes: &mut Nes, rng: &mut Rng, frames: u32) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for _ in 0..frames {
        nes.run_frame(rng.pads());
        h = (h ^ nes.state_hash()).wrapping_mul(0x100_0000_01B3);
    }
    for &px in nes.frame() {
        h = (h ^ px as u64).wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// Runs 300 frames, saves, runs 300 more; a fresh console loading the state must replay them
/// identically. Returns the state size.
fn roundtrip(name: &str, rom: &[u8]) -> usize {
    let mut a = Nes::new(rom, None).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    run(&mut a, &mut rng, 300);
    let state = a.save_state();
    let saved_hash = a.state_hash();
    let mut rng_b = Rng(rng.0);
    let hash_a = run(&mut a, &mut rng, 300);

    let mut b = Nes::new(rom, None).unwrap();
    b.load_state(&state).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert_eq!(b.state_hash(), saved_hash, "{name}: state hash right after load");
    let hash_b = run(&mut b, &mut rng_b, 300);
    assert_eq!(hash_a, hash_b, "{name}: replay after load diverged");
    assert_eq!(a.save_state(), b.save_state(), "{name}: final states differ");
    state.len()
}

#[test]
fn roundtrip_nestest() {
    let Some(rom) = file("nestest/nestest.nes") else { return };
    let n = roundtrip("nestest", &rom);
    println!("nestest state: {n} bytes");
}

#[test]
fn roundtrip_mmc1() {
    let Some(rom) = file("MMC1_A12/mmc1_a12.nes") else { return };
    roundtrip("mmc1", &rom);
}

#[test]
fn roundtrip_mmc3() {
    let Some(rom) = file("mmc3_test_2/rom_singles/4-scanline_timing.nes") else { return };
    roundtrip("mmc3", &rom);
}

#[test]
fn roundtrip_fme7() {
    roundtrip("fme7", &fme7());
}

#[test]
fn roundtrip_vrc6() {
    roundtrip("vrc6", &vrc6());
}

#[test]
fn roundtrip_n163() {
    roundtrip("n163", &n163());
}

#[test]
fn roundtrip_mmc5() {
    let n = roundtrip("mmc5", &mmc5());
    println!("mmc5 state: {n} bytes");
}

#[test]
fn foreign_and_damaged_states_are_refused() {
    let Some(nestest) = file("nestest/nestest.nes") else { return };
    let mut a = Nes::new(&nestest, None).unwrap();
    let mut rng = Rng(7);
    run(&mut a, &mut rng, 60);
    let state = a.save_state();

    let mut other = Nes::new(&mmc5(), None).unwrap();
    run(&mut other, &mut rng, 10);
    let other_hash = other.state_hash();
    assert_eq!(other.load_state(&state), Err(StateError::WrongRom));
    assert_eq!(other.state_hash(), other_hash);

    let mut fresh = Nes::new(&nestest, None).unwrap();
    let fresh_hash = fresh.state_hash();
    assert_eq!(fresh.load_state(&state[..state.len() - 1]), Err(StateError::Corrupt));
    assert_eq!(fresh.load_state(&state[..7]), Err(StateError::Corrupt));
    assert_eq!(fresh.load_state(b"EB"), Err(StateError::BadMagic));
    assert_eq!(fresh.load_state(&[]), Err(StateError::BadMagic));
    let mut bad = state.clone();
    bad[0] ^= 0xFF;
    assert_eq!(fresh.load_state(&bad), Err(StateError::BadMagic));
    let mut bad = state.clone();
    bad[4] = 9;
    assert_eq!(fresh.load_state(&bad), Err(StateError::Version(9)));
    let mut bad = state.clone();
    bad.push(0);
    assert_eq!(fresh.load_state(&bad), Err(StateError::Corrupt));
    assert_eq!(fresh.state_hash(), fresh_hash, "failed loads must not touch the machine");
    // Region mismatch.
    let mut pal = Nes::new(&nestest, Some(ebobo_core::Region::Pal)).unwrap();
    assert_eq!(pal.load_state(&state), Err(StateError::WrongRom));
}

#[test]
fn garbage_never_panics() {
    let rom = mmc5();
    let mut a = Nes::new(&rom, None).unwrap();
    let mut rng = Rng(42);
    run(&mut a, &mut rng, 30);
    let state = a.save_state();
    let mut target = Nes::new(&rom, None).unwrap();
    let before = target.state_hash();
    let mut tries = 0;
    let mut check = |target: &mut Nes, blob: &[u8]| {
        tries += 1;
        if target.load_state(blob).is_err() {
            assert_eq!(target.state_hash(), before, "failed load changed the machine");
        } else {
            // A lucky mutation decoded; reset the machine for the next try.
            *target = Nes::new(&rom, None).unwrap();
        }
    };
    for _ in 0..200 {
        let len = (rng.next() % 4096) as usize;
        let blob: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        check(&mut target, &blob);
        let mut with_header = state[..11].to_vec();
        with_header.extend_from_slice(&blob);
        check(&mut target, &with_header);
    }
    for _ in 0..200 {
        let cut = (rng.next() as usize) % state.len();
        check(&mut target, &state[..cut]);
    }
    for _ in 0..300 {
        let mut bad = state.clone();
        for _ in 0..1 + rng.next() % 8 {
            let i = 11 + (rng.next() as usize) % (bad.len() - 11);
            bad[i] ^= 1 << (rng.next() % 8);
        }
        check(&mut target, &bad);
    }
    assert_eq!(tries, 900);
}
