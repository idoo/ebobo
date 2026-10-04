//! Holy Mapperel (pinobatch/holy-mapperel 0.02 prebuilt `testroms/*.nes`) as a real-ROM check of
//! the mapper boards. The ROMs are not vendored: set `HOLY_MAPPEREL_DIR` to the directory holding
//! them, else the test is skipped.
//!
//! After its tests finish the ROM leaves its results in zero page (README "Displayed result",
//! addresses found from RAM dumps of 0.02): `cur_mapper` $15, `last_prg_bank` $17 (4 KiB units),
//! `is_chrrom` $18, `last_chr_bank` $19 (8 KiB units), `chr_test_result` $1A, `has_wram` $1C,
//! `last_wram_bank` $1D, `wram_test_result` $1E, `driver_prg_result` $1F, `driver_chr_result` $20.
//! Each is checked against the configuration encoded in the file name
//! (`M<mapper>[.<sub>]_P<n>K[_C<n>K|_CR<n>K][_S<n>K|_W<n>K][_H|_V]`).

use ebobo_core::Nes;

/// Every ROM of the 0.02 release the emulator must pass. Missing: M28 (Action 53, mapper not
/// implemented).
const MUST_PASS: &[&str] = &[
    "M0_P32K_C8K_V",
    "M0_P32K_CR32K_V",
    "M0_P32K_CR8K_V",
    "M1_P128K",
    "M1_P128K_C128K",
    "M1_P128K_C128K_S8K",
    "M1_P128K_C128K_W8K",
    "M1_P128K_C32K",
    "M1_P128K_C32K_S8K",
    "M1_P128K_C32K_W8K",
    "M1_P128K_CR8K",
    "M1_P512K_CR8K_S32K",
    "M1_P512K_CR8K_S8K",
    "M1_P512K_S32K",
    "M1_P512K_S8K",
    "M2_P128K_CR8K_V",
    "M2_P128K_V",
    "M3_P32K_C32K_H",
    "M4_P128K",
    "M4_P128K_CR32K",
    "M4_P128K_CR8K",
    "M4_P256K_C256K",
    "M7_P128K",
    "M7_P128K_CR8K",
    "M9_P128K_C64K",
    "M10_P128K_C64K_S8K",
    "M10_P128K_C64K_W8K",
    "M11_P64K_C64K_V",
    "M11_P64K_CR32K_V",
    "M28_P512K",
    "M28_P512K_CR32K",
    "M34_P128K_CR8K_H",
    "M34_P128K_H",
    "M66_P64K_C16K_V",
    "M69_P128K_C64K_S8K",
    "M69_P128K_C64K_W8K",
    "M78.3_P128K_C64K",
    "M118_P128K_C64K",
    "M180_P128K_CR8K_H",
    "M180_P128K_H",
];

/// Long enough for the slowest ROM (SXROM: 512K PRG + 32K WRAM) to finish its RAM tests.
const FRAMES: u32 = 900;

struct Expect {
    mapper: u8,
    last_prg: u8,
    chr_rom: bool,
    last_chr: u8,
    last_wram: Option<u8>,
}

fn kib(s: &str) -> u32 {
    s.strip_suffix('K').and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("bad size {s}"))
}

fn expect(name: &str) -> Expect {
    let mut parts = name.split('_');
    let mapper = parts.next().unwrap()[1..].split('.').next().unwrap().parse().unwrap();
    let mut e = Expect { mapper, last_prg: 0, chr_rom: false, last_chr: 0, last_wram: None };
    for p in parts {
        if let Some(s) = p.strip_prefix("CR") {
            // NROM has no CHR banking: only the first 8 KiB of a larger CHR RAM is visible.
            e.last_chr = if mapper == 0 { 0 } else { (kib(s) / 8 - 1) as u8 };
        } else if let Some(s) = p.strip_prefix('C') {
            e.chr_rom = true;
            e.last_chr = (kib(s) / 8 - 1) as u8;
        } else if let Some(s) = p.strip_prefix('P') {
            e.last_prg = (kib(s) / 4 - 1) as u8;
        } else if let Some(s) = p.strip_prefix('S').or_else(|| p.strip_prefix('W')) {
            e.last_wram = Some((kib(s) / 8 - 1) as u8);
        }
    }
    e
}

/// Mismatches between the ROM's self-report and the file name, empty when it passed.
fn check(dir: &str, name: &str) -> Vec<String> {
    let path = format!("{dir}/{name}.nes");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut nes = Nes::new(&bytes, None).unwrap_or_else(|e| panic!("{path}: {e:?}"));
    for _ in 0..FRAMES {
        nes.run_frame([0; 4]);
    }
    let e = expect(name);
    let mut bad = Vec::new();
    let mut want = |what: &str, addr: u16, ok: &dyn Fn(u8) -> bool| {
        let v = nes.peek(addr);
        if !ok(v) {
            bad.push(format!("{what}=${v:02X}"));
        }
    };
    want("cur_mapper", 0x15, &|v| v == e.mapper);
    want("last_prg_bank", 0x17, &|v| v == e.last_prg);
    want("is_chrrom", 0x18, &|v| (v != 0) == e.chr_rom);
    want("last_chr_bank", 0x19, &|v| v == e.last_chr);
    want("chr_test_result", 0x1A, &|v| v == 0);
    want("has_wram", 0x1C, &|v| (v != 0) == e.last_wram.is_some());
    if let Some(last) = e.last_wram {
        want("last_wram_bank", 0x1D, &|v| v == last);
        want("wram_test_result", 0x1E, &|v| v == 0);
    }
    want("driver_prg_result", 0x1F, &|v| v == 0);
    want("driver_chr_result", 0x20, &|v| v == 0);
    bad
}

#[test]
fn holy_mapperel() {
    let Ok(dir) = std::env::var("HOLY_MAPPEREL_DIR") else {
        eprintln!("HOLY_MAPPEREL_DIR not set; skipping Holy Mapperel");
        return;
    };
    // One thread per ROM: each is an independent 900-frame run.
    let failures: Vec<String> = std::thread::scope(|s| {
        let runs: Vec<_> = MUST_PASS.iter().map(|name| (name, s.spawn(|| check(&dir, name)))).collect();
        runs.into_iter()
            .filter_map(|(name, run)| {
                let bad = run.join().unwrap();
                (!bad.is_empty()).then(|| format!("{name}: {}", bad.join(" ")))
            })
            .collect()
    });
    assert!(failures.is_empty(), "Holy Mapperel failures:\n{}", failures.join("\n"));
}

#[test]
fn file_name_expectations() {
    let e = expect("M78.3_P128K_C64K");
    assert_eq!((e.mapper, e.last_prg, e.chr_rom, e.last_chr, e.last_wram), (78, 31, true, 7, None));
    let e = expect("M1_P512K_CR8K_S32K");
    assert_eq!((e.mapper, e.last_prg, e.chr_rom, e.last_chr, e.last_wram), (1, 127, false, 0, Some(3)));
}
