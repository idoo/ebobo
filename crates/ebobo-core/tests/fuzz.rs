//! Fuzz-style robustness tests (seeded xorshift, no extra deps): hostile ROM bytes must never
//! panic the parser or the emulator. CPU jams and parse errors are fine; panics are not.

mod common;

use ebobo_core::cart::{self, Esx, EsxBank, RomBytes, ESX2_MAGIC, ESX_FOOTER, ESX_MAGIC};
use ebobo_core::Nes;
use std::sync::Arc;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
}

fn nestest() -> Option<Vec<u8>> {
    let path = common::roms()?.join("nestest/nestest.nes");
    Some(std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// Appends `chunk` (magic..banks) with its total_len and footer to `rom`.
fn with_chunk(rom: &[u8], chunk: &[u8], total: u32) -> Vec<u8> {
    let mut out = rom.to_vec();
    out.extend_from_slice(chunk);
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(ESX_FOOTER);
    out
}

#[test]
fn parse_random_bytes_never_panics() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for i in 0..20_000 {
        let len = rng.below(64 * 1024 + 64);
        let mut buf = rng.bytes(len);
        // Half the inputs get a valid magic so parsing goes past the first check.
        if i % 2 == 0 && buf.len() >= 4 {
            buf[..4].copy_from_slice(b"NES\x1A");
        }
        let _ = cart::parse(&buf);
    }
}

#[test]
fn parse_truncated_and_garbled_headers_never_panic() {
    let Some(rom) = nestest() else { return };
    let mut rng = Rng(0xDEAD_BEEF_0BAD_F00D);
    // Every truncation length of the header and around PRG/CHR boundaries.
    for len in (0..=64).chain((16 + 16 * 1024 - 32)..=(16 + 16 * 1024 + 32)).chain(rom.len() - 32..rom.len()) {
        let _ = cart::parse(&rom[..len.min(rom.len())]);
    }
    for _ in 0..20_000 {
        let mut buf = rom.clone();
        // Garble 1..4 header bytes (keep the magic most of the time).
        for _ in 0..1 + rng.below(4) {
            let at = if rng.below(8) == 0 { rng.below(16) } else { 4 + rng.below(12) };
            buf[at] = rng.byte();
        }
        if rng.below(4) == 0 {
            buf.truncate(rng.below(buf.len() + 1));
        }
        if let Ok(info) = cart::parse(&buf) {
            let _ = info.mapper;
        }
    }
}

#[test]
fn parse_random_esx_chunks_never_panic() {
    let Some(rom) = nestest() else { return };
    let mut rng = Rng(0x0F0F_1E1E_2D2D_3C3C);
    for i in 0..20_000 {
        let mut chunk = Vec::new();
        chunk.extend_from_slice(if rng.below(2) == 0 { ESX_MAGIC } else { ESX2_MAGIC });
        chunk.push(rng.below(9) as u8);
        let body_len = rng.below(600);
        chunk.extend(rng.bytes(body_len));
        let total = match i % 4 {
            0 => chunk.len() as u32,
            1 => rng.next() as u32,
            2 => (chunk.len() as u32).wrapping_add(rng.below(9) as u32).wrapping_sub(4),
            _ => (rom.len() + chunk.len() + rng.below(64)) as u32,
        };
        let bytes = with_chunk(&rom, &chunk, total);
        let _ = Esx::detect(&bytes);
        let _ = cart::parse(&bytes);
    }
    // Structurally valid chunks with flipped bytes: parse and, when accepted, run a few frames.
    for _ in 0..300 {
        let banks: Vec<EsxBank> = (0..1 + rng.below(7))
            .map(|_| {
                let first = rng.byte();
                let count = 1 + rng.below(256 - first as usize) as u8;
                let pals = 1 + rng.below(4);
                EsxBank {
                    palettes: (0..pals).map(|_| [rng.byte() & 0x3F, rng.byte() & 0x3F, rng.byte() & 0x3F]).collect(),
                    first_tile: first,
                    tile_count: count,
                    tile_palette: RomBytes(Arc::from((0..count).map(|_| rng.below(pals) as u8).collect::<Vec<_>>())),
                    tiles: RomBytes(Arc::from(rng.bytes(16 * count as usize))),
                }
            })
            .collect();
        let chunk = Esx::build(&banks).expect("valid banks build");
        let mut bytes = rom.clone();
        bytes.extend_from_slice(&chunk);
        let flips = rng.below(3);
        for _ in 0..flips {
            let at = rom.len() + rng.below(chunk.len());
            bytes[at] = rng.byte();
        }
        if flips == 0 {
            assert!(cart::parse(&bytes).is_ok(), "a built ESX chunk must parse");
        }
        if let Ok(mut nes) = Nes::new(&bytes, None) {
            for _ in 0..3 {
                nes.run_frame([0; 4]);
            }
        }
    }
}

#[test]
fn mutated_prg_runs_30_frames_without_panic() {
    let Some(rom) = nestest() else { return };
    let mut rng = Rng(0xA5A5_5A5A_C3C3_3C3C);
    let prg = 16..16 + 16 * 1024;
    for case in 0..40 {
        let mut buf = rom.clone();
        // From a few flipped bytes up to a fully random PRG.
        let n = if case % 10 == 9 { prg.len() } else { 1 + rng.below(256 << (case % 6)) };
        for _ in 0..n {
            let at = prg.start + rng.below(prg.len());
            buf[at] = rng.byte();
        }
        let mut nes = Nes::new(&buf, None).expect("header untouched");
        for f in 0..30u64 {
            let p = (rng.next() ^ f) as u8;
            nes.run_frame([p, p.rotate_left(1), p.rotate_left(2), p.rotate_left(3)]);
        }
        let _ = nes.state_hash();
    }
}
