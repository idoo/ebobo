//! ESX1/ESX2 extra sprite banks (`docs/EXTENSIONS.md`) through the public API.

use ebobo_core::cart::{self, Esx, EsxBank, RomBytes};
use ebobo_core::{Nes, RomError, HEIGHT, WIDTH};
use std::sync::Arc;

const BG: u16 = 0x10;
const SPRITE: u16 = 0x30;
const ESX_C2: u8 = 0x2A;

/// NROM-128: palette, a nametable of solid tile 2, sprite 0 (tile 1, attr bits 2-4 = 1) at
/// (40, 40) and sprite 1 (tile 1, attr 0) at (80, 40); rendering on, then waits for sprite-0
/// hit and stores 1 at $0300.
fn rom() -> Vec<u8> {
    #[rustfmt::skip]
    let code: [u8; 0x6E] = [
        0x78, 0xD8, 0xA2, 0xFF, 0x9A,                   // SEI CLD LDX #$FF TXS
        0x2C, 0x02, 0x20, 0x10, 0xFB,                   // vblank wait
        0x2C, 0x02, 0x20, 0x10, 0xFB,                   // vblank wait
        0xA9, 0x3F, 0x8D, 0x06, 0x20, 0xA9, 0x00, 0x8D, 0x06, 0x20, // $2006 = $3F00
        0xA2, 0x00,                                     // LDX #0
        0xBD, 0x00, 0xC1, 0x8D, 0x07, 0x20, 0xE8, 0xE0, 0x20, 0xD0, 0xF5, // palette x32
        0xA9, 0x20, 0x8D, 0x06, 0x20, 0xA9, 0x00, 0x8D, 0x06, 0x20, // $2006 = $2000
        0xA0, 0x04, 0xA2, 0x00, 0xA9, 0x02,             // LDY #4 LDX #0 LDA #2
        0x8D, 0x07, 0x20, 0xCA, 0xD0, 0xFA, 0x88, 0xD0, 0xF7, // 1024 x tile 2
        0xA9, 0x00, 0x8D, 0x03, 0x20, 0xA2, 0x00,       // OAMADDR = 0, LDX #0
        0xBD, 0x00, 0xC2, 0x8D, 0x04, 0x20, 0xE8, 0xD0, 0xF7, // OAM x256
        0xA9, 0x00, 0x8D, 0x05, 0x20, 0x8D, 0x05, 0x20, 0x8D, 0x00, 0x20, // scroll, ctrl
        0xA9, 0x1E, 0x8D, 0x01, 0x20,                   // mask: bg + sprites, no clipping
        0xAD, 0x02, 0x20, 0x29, 0x40, 0xF0, 0xF9,       // wait for sprite-0 hit
        0xA9, 0x01, 0x8D, 0x00, 0x03,                   // $0300 = 1
        0x4C, 0x6B, 0xC0,                               // JMP *
    ];
    let mut prg = vec![0u8; 0x4000];
    prg[..code.len()].copy_from_slice(&code);
    let mut pal = [0u8; 32];
    pal[0] = 0x0F;
    for p in [1, 5, 9, 13] {
        pal[p] = BG as u8; // the attribute bytes are tile 2 too: every bg palette alike
    }
    pal[16] = 0x0F;
    pal[17] = SPRITE as u8;
    pal[18] = 0x27;
    pal[19] = 0x17;
    prg[0x100..0x120].copy_from_slice(&pal);
    let mut oam = [0xF0u8; 256];
    oam[..8].copy_from_slice(&[40, 1, 0x04, 40, 40, 1, 0x00, 80]);
    prg[0x200..0x300].copy_from_slice(&oam);
    prg[0x3FFC] = 0x00; // reset vector $C000
    prg[0x3FFD] = 0xC0;
    prg[0x3FFA] = 0x6B; // NMI (unused)
    prg[0x3FFB] = 0xC0;

    let mut chr = vec![0u8; 0x2000];
    chr[16..24].fill(0xFF); // tile 1: pixel value 1
    chr[32..40].fill(0xFF); // tile 2: pixel value 1

    let mut rom = b"NES\x1A\x01\x01\x00\x00".to_vec();
    rom.resize(16, 0);
    rom.extend_from_slice(&prg);
    rom.extend_from_slice(&chr);
    rom
}

/// Bank 1 replaces tile 1 with an all-value-2 pattern drawn in palette [$16, $2A, $12].
fn bank() -> EsxBank {
    let mut tile = [0u8; 16];
    tile[8..].fill(0xFF);
    EsxBank::single([0x16, ESX_C2, 0x12], 1, RomBytes(Arc::from(&tile[..])))
}

const ESX2_C2: u8 = 0x21;

/// ESX2 bank 1: tiles 1 and 2 (all value 2), tile 1 → palette 0, tile 2 → palette 1.
fn bank2() -> EsxBank {
    let mut tiles = [0u8; 32];
    tiles[8..16].fill(0xFF);
    tiles[24..32].fill(0xFF);
    EsxBank {
        palettes: vec![[0x16, ESX_C2, 0x12], [0x19, ESX2_C2, 0x13]],
        first_tile: 1,
        tile_count: 2,
        tile_palette: RomBytes(Arc::from(&[0u8, 1][..])),
        tiles: RomBytes(Arc::from(&tiles[..])),
    }
}

fn with_chunk(rom: &[u8]) -> Vec<u8> {
    let mut out = rom.to_vec();
    out.extend_from_slice(&Esx::build(&[bank()]).unwrap());
    out
}

fn run(rom: &[u8]) -> Nes {
    let mut nes = Nes::new(rom, None).unwrap();
    for _ in 0..6 {
        nes.run_frame([0; 4]);
    }
    nes
}

fn px(nes: &Nes, x: usize, y: usize) -> u16 {
    nes.frame()[y * WIDTH + x] & 0x3F
}

/// Sprite at OAM (x, y) covers screen x..x+8, y+1..y+9.
fn in_box(x: usize, y: usize, sx: usize) -> bool {
    (sx..sx + 8).contains(&x) && (41..49).contains(&y)
}

#[test]
fn esx_replaces_only_flagged_sprite() {
    let plain = run(&rom());
    let esx = run(&with_chunk(&rom()));
    assert_eq!(plain.esx_banks(), 0);
    assert_eq!(esx.esx_banks(), 1);

    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (p, e) = (px(&plain, x, y), px(&esx, x, y));
            if in_box(x, y, 40) {
                assert_eq!(p, SPRITE, "plain sprite 0 at ({x},{y})");
                assert_eq!(e, ESX_C2 as u16, "ESX sprite 0 at ({x},{y})");
            } else {
                assert_eq!(p, e, "unflagged pixels identical at ({x},{y})");
                let want = if in_box(x, y, 80) { SPRITE } else { BG };
                assert_eq!(p, want, "known picture at ({x},{y})");
            }
        }
    }
}

#[test]
fn esx_sprite_zero_hit_still_works() {
    assert_eq!(run(&rom()).peek(0x0300), 1);
    assert_eq!(run(&with_chunk(&rom())).peek(0x0300), 1, "flagged sprite 0 still hits");
}

#[test]
fn esx_rollback_restores_identical_state() {
    let mut nes = Nes::new(&with_chunk(&rom()), None).unwrap();
    nes.run_frame([0; 4]);
    let snap = nes.snapshot();
    nes.run_frame([0; 4]);
    nes.run_frame([0; 4]);
    let (hash, frame) = (nes.state_hash(), nes.frame().to_vec());
    nes.restore(&snap);
    nes.run_frame([0; 4]);
    nes.run_frame([0; 4]);
    assert_eq!(nes.state_hash(), hash);
    assert_eq!(nes.frame(), &frame[..]);
}

#[test]
fn esx_crc_covers_chunk() {
    let a = cart::parse(&rom()).unwrap();
    let b = cart::parse(&with_chunk(&rom())).unwrap();
    assert!(a.esx.is_none());
    assert_eq!(b.esx.as_ref().unwrap().banks.len(), 1);
    assert_ne!(a.crc32, b.crc32);
    assert_eq!(a.prg_rom[..], b.prg_rom[..]);
    assert_eq!(a.chr_rom[..], b.chr_rom[..]);

    let mut other = bank();
    other.palettes[0][0] = 0x17;
    let mut c = rom();
    c.extend_from_slice(&Esx::build(&[other]).unwrap());
    assert_ne!(cart::parse(&c).unwrap().crc32, b.crc32, "different art, different identity");
}

#[test]
fn esx_malformed_chunk_rejected() {
    let base = rom();
    let good = Esx::build(&[bank()]).unwrap();
    // Offsets inside the chunk: 0 magic, 4 count, 5..8 palette, 8 first_tile, 9 tile_count.
    let bad = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut chunk = good.clone();
        edit(&mut chunk);
        let mut f = base.clone();
        f.extend_from_slice(&chunk);
        f
    };
    let set_len = |c: &mut Vec<u8>, n: u32| {
        let at = c.len() - 12;
        c[at..at + 4].copy_from_slice(&n.to_le_bytes());
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("magic", bad(&|c| c[0] = b'X')),
        ("count 0", bad(&|c| c[4] = 0)),
        ("count 8", bad(&|c| c[4] = 8)),
        ("count beyond data", bad(&|c| c[4] = 2)),
        ("tile_count 0", bad(&|c| c[9] = 0)),
        ("tile range past 255", bad(&|c| { c[8] = 0xFF; c[9] = 2 })),
        ("tiles past chunk", bad(&|c| c[9] = 2)),
        ("total_len too long", bad(&|c| set_len(c, 1 << 30))),
        ("total_len short", bad(&|c| set_len(c, 4))),
        ("extra byte", bad(&|c| c.insert(c.len() - 12, 0))),
        ("colour above $3F", bad(&|c| c[5] = 0x40)),
    ];
    for (name, file) in cases {
        assert!(
            matches!(cart::parse(&file), Err(RomError::Invalid(_))),
            "{name}: {:?}",
            cart::parse(&file).map(|i| i.crc32)
        );
        assert!(Nes::new(&file, None).is_err(), "{name}");
    }
    assert!(Esx::build(&[]).is_err());
    assert!(Esx::build(&vec![bank(); 8]).is_err());
}

#[test]
fn esx2_tiles_pick_their_own_palette() {
    // Sprite 1 also flagged (attr bits 2-4 = 1) and drawn with tile 2.
    let mut r = rom();
    r[16 + 0x200 + 5] = 2;
    r[16 + 0x200 + 6] = 0x04;
    let chunk = Esx::build(&[bank2()]).unwrap();
    assert_eq!(&chunk[..4], b"ESX2");
    r.extend_from_slice(&chunk);
    let info = cart::parse(&r).unwrap();
    assert_eq!(info.esx.as_ref().unwrap().banks[0].palettes.len(), 2);
    let nes = run(&r);
    assert_eq!(nes.peek(0x0300), 1, "sprite-0 hit");
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let want = if in_box(x, y, 40) {
                ESX_C2 as u16
            } else if in_box(x, y, 80) {
                ESX2_C2 as u16
            } else {
                BG
            };
            assert_eq!(px(&nes, x, y), want, "({x},{y})");
        }
    }
}

#[test]
fn esx2_malformed_chunk_rejected() {
    let base = rom();
    let good = Esx::build(&[bank2()]).unwrap();
    let mut ok = base.clone();
    ok.extend_from_slice(&good);
    assert!(cart::parse(&ok).is_ok());
    // Offsets: 0 magic, 4 count, 5 palette_count, 6..12 palettes, 12 first_tile,
    // 13 tile_count, 14..16 tile_palette.
    let bad = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut chunk = good.clone();
        edit(&mut chunk);
        let mut f = base.clone();
        f.extend_from_slice(&chunk);
        f
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("tile palette index out of range", bad(&|c| c[15] = 2)),
        ("palette count 0", bad(&|c| c[5] = 0)),
        ("palette count 5", bad(&|c| c[5] = 5)),
        ("palette count 3, data for 2", bad(&|c| c[5] = 3)),
        ("colour above $3F", bad(&|c| c[9] = 0x40)),
        ("tile palettes truncated", bad(&|c| c[13] = 3)),
    ];
    for (name, file) in cases {
        assert!(
            matches!(cart::parse(&file), Err(RomError::Invalid(_))),
            "{name}: {:?}",
            cart::parse(&file).map(|i| i.crc32)
        );
        assert!(Nes::new(&file, None).is_err(), "{name}");
    }
    let mut b = bank2();
    b.tile_palette = RomBytes(Arc::from(&[0u8, 2][..]));
    assert!(Esx::build(&[b]).is_err());
    let mut b = bank2();
    b.palettes = vec![[0, 0, 0]; 5];
    assert!(Esx::build(&[b]).is_err());
    let mut b = bank2();
    b.palettes.clear();
    assert!(Esx::build(&[b]).is_err());
}
