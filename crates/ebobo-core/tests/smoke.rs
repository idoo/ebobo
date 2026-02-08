//! End-to-end smoke tests through the public `ebobo_core::Nes` API.

mod common;

use ebobo_core::Nes;
use std::time::Instant;

/// `None` when the test ROMs were not fetched.
fn load(path: &str) -> Option<Nes> {
    let bytes = std::fs::read(common::roms()?.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"));
    Some(Nes::new(&bytes, None).unwrap_or_else(|e| panic!("{path}: {e:?}")))
}

fn text_at(nes: &Nes, mut addr: u16) -> String {
    let mut s = String::new();
    while s.len() < 1024 {
        let b = nes.peek(addr);
        if b == 0 {
            break;
        }
        s.push(b as char);
        addr += 1;
    }
    s
}

/// blargg $6000 protocol: returns (status, message).
fn run_blargg(path: &str, seconds: u32) -> Option<(u8, String)> {
    let mut nes = load(path)?;
    let frames = seconds * 60;
    let mut reset_at: Option<u32> = None;
    for f in 0..frames {
        nes.run_frame([0; 4]);
        let valid = nes.peek(0x6001) == 0xDE && nes.peek(0x6002) == 0xB0 && nes.peek(0x6003) == 0x61;
        if !valid {
            continue;
        }
        match nes.peek(0x6000) {
            0x80 => {}
            0x81 => match reset_at {
                None => reset_at = Some(f + 10),
                Some(t) if f >= t => {
                    nes.reset();
                    reset_at = None;
                }
                Some(_) => {}
            },
            status => return Some((status, text_at(&nes, 0x6004))),
        }
    }
    Some((0xFF, format!("timeout; text so far: {}", text_at(&nes, 0x6004))))
}

fn assert_blargg(path: &str) {
    let Some((status, text)) = run_blargg(path, 120) else { return };
    println!("{path}: status {status:02X}\n{text}");
    assert_eq!(status, 0, "{path} failed:\n{text}");
}

#[test]
fn instr_test_official_only() {
    assert_blargg("instr_test-v5/official_only.nes");
}

#[test]
fn ppu_vbl_basics() {
    assert_blargg("ppu_vbl_nmi/rom_singles/01-vbl_basics.nes");
}

#[test]
fn mmc3_clocking() {
    assert_blargg("mmc3_test_2/rom_singles/1-clocking.nes");
}

#[test]
fn sprite_hit_basics() {
    let Some(mut nes) = load("sprite_hit_tests_2005.10.05/01.basics.nes") else { return };
    for _ in 0..600 {
        nes.run_frame([0; 4]);
    }
    // The 2005 tests keep their result code in $00F8 (1 = pass, N = failed test N);
    // $F0/$F1 hold a text pointer, not the result.
    let r = nes.peek(0x00F8);
    println!("sprite_hit 01.basics: $F8 = {r}");
    assert_eq!(r, 1);
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 24) as u8
    }
    fn pads(&mut self) -> [u8; 4] {
        [self.next(), self.next(), self.next(), self.next()]
    }
}

#[test]
fn determinism_and_rollback() {
    let Some(mut a) = load("nestest/nestest.nes") else { return };
    let mut b = load("nestest/nestest.nes").unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..300 {
        let p = rng.pads();
        a.run_frame(p);
        b.run_frame(p);
        assert_eq!(a.state_hash(), b.state_hash());
    }
    let snap = a.snapshot();
    let inputs: Vec<[u8; 4]> = (0..300).map(|_| rng.pads()).collect();
    let mut per_frame = Vec::new();
    for &p in &inputs {
        a.run_frame(p);
        b.run_frame(p);
        assert_eq!(a.state_hash(), b.state_hash());
        per_frame.push(a.state_hash());
    }
    let hash_a = a.state_hash();
    a.restore(&snap);
    for (i, &p) in inputs.iter().enumerate() {
        a.run_frame(p);
        assert_eq!(a.state_hash(), per_frame[i], "replay diverged at frame {i}");
    }
    assert_eq!(a.state_hash(), hash_a);
}

/// NROM-128 with CHR RAM: strobes $4016, then reads $4016 and $4017 24 times each,
/// storing bit 0 at $0300,X and $0320,X.
fn four_score_rom() -> Vec<u8> {
    let mut rom = vec![b'N', b'E', b'S', 0x1A, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut prg = vec![0xEAu8; 0x4000];
    #[rustfmt::skip]
    let code = [
        0xA9, 0x01, 0x8D, 0x16, 0x40, // LDA #1; STA $4016
        0xA9, 0x00, 0x8D, 0x16, 0x40, // LDA #0; STA $4016
        0xA2, 0x00,                   // LDX #0
        0xAD, 0x16, 0x40, 0x29, 0x01, 0x9D, 0x00, 0x03, // loop: LDA $4016; AND #1; STA $0300,X
        0xAD, 0x17, 0x40, 0x29, 0x01, 0x9D, 0x20, 0x03, // LDA $4017; AND #1; STA $0320,X
        0xE8, 0xE0, 0x18, 0xD0, 0xEB, // INX; CPX #24; BNE loop
        0x4C, 0x21, 0x80,             // JMP *
        0x40,                         // RTI
    ];
    prg[..code.len()].copy_from_slice(&code);
    prg[0x3FFA..].copy_from_slice(&[0x24, 0x80, 0x00, 0x80, 0x24, 0x80]);
    rom.extend_from_slice(&prg);
    rom
}

#[test]
fn four_score_sequence() {
    let mut nes = Nes::new(&four_score_rom(), None).unwrap();
    nes.set_four_score(true);
    let pads = [0b1010_0101, 0b0011_1100, 0b1100_0011, 0b0101_1010];
    nes.run_frame(pads);
    nes.run_frame(pads);
    let bits = |base: u16| -> Vec<u8> { (0..24).map(|i| nes.peek(base + i)).collect() };
    let expect = |lo: u8, hi: u8, sig: u8| -> Vec<u8> {
        (0..24).map(|i| ([lo, hi, sig][i / 8] >> (i % 8)) & 1).collect()
    };
    // nesdev Four Score: signature reads 0,0,0,1,0,0,0,0 ($4016) / 0,0,1,0,0,0,0,0 ($4017).
    assert_eq!(bits(0x0300), expect(pads[0], pads[2], 0x08), "$4016");
    assert_eq!(bits(0x0320), expect(pads[1], pads[3], 0x04), "$4017");
}

#[test]
fn timing() {
    let Some(mut nes) = load("scrolltest/scroll.nes") else { return };
    let mut rng = Rng(1);
    for _ in 0..60 {
        nes.run_frame(rng.pads());
    }
    let t = Instant::now();
    for _ in 0..600 {
        nes.run_frame(rng.pads());
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0 / 600.0;
    println!("timing: {ms:.4} ms/frame over 600 frames (scroll.nes)");
}
