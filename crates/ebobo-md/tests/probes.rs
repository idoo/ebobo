//! Timing probes: the small cartridges of `tools/md-oracle/probes/out/` run in the core and their
//! result words are compared with what the reference emulator gave (`probes/expect/<name>.txt` for
//! NTSC, `<name>.pal.txt` for PAL, recorded by `tools/md-oracle/probe.py --record`).
//! Each probe runs for the frame count in the first line of its source (`| frames: N`).
//! Every differing probe is printed with its words; the test fails unless the probe is listed in
//! `KNOWN_DIFF` with a reason.

use ebobo_md::{Md, Region};
use std::path::{Path, PathBuf};

/// Probes whose difference is understood and accepted for now: (name, reason).
const KNOWN_DIFF: &[(&str, &str)] = &[
    ("frame_cycles", "access timing inside an instruction (2 passes in 4202)"),
    ("vcounter", "H counter spacing: access timing inside an instruction"),
    ("blank_edges", "H-blank flag edges (both widths) agree on average within 0.5 H count over 12 lines each; single words differ by \
                    about 2 H counts, or by one poll-loop period (6-8) where the poll's phase lands on the other side of the edge: \
                    HV sample spacing, access timing inside an instruction"),
    ("hint_lines", "register 10 = 0, first setting right after start-up: ours takes an extra request at V = 0xFF; hint_pend shows latching across blanking matches, so the cause is in the start-up frame (open)"),
    ("hint_pend", "counts and V bytes match; only H counter bytes differ: access timing inside an instruction"),
    ("fifo_rate", "one write slot off at the start: FIFO slot timing"),
    ("fifo_status", "FIFO empty flag one write early: FIFO slot timing"),
    ("row_point", "HV words read after each write differ by a few H counts (poll phase: access timing inside an instruction); \
                   the rows each write changes match, see tools/md-oracle/row_point.py"),
    ("row_point_c", "as row_point"),
    ("boot_phase", "power-on HV word reads V = 0x9F (NTSC) / 0x84 (PAL), H = 0 in the reference although its first blanking \
                    comes 32 / 58 lines later; ours matches the blanking time within 1 poll pass, not the HV word"),
    ("boot_clock", "frame 1 within 2 passes of 210 clocks; ours loses 4 passes a frame (refresh/access timing, as frame_cycles)"),
    ("dma_rate", "68000 transfers of 128/64/40 words in the picture end on the reference's word; 16-word transfers end 1-2 slots early \
                  (slots are spread evenly in ours, unevenly in the chip); 1024 words while blanked end about 15 slots early"),
    ("dma_rate_h32", "as dma_rate, 256-pixel mode"),
];

/// Word 0 of the result block when the probe's `main` returned.
const DONE: u16 = 0xD0E1;

fn probes_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/md-oracle/probes")
}

/// Hex words of an expect file; `#` lines are comments.
fn expected(path: &Path) -> Vec<u16> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| u16::from_str_radix(l, 16).unwrap_or_else(|_| panic!("{}: bad word {l}", path.display())))
        .collect()
}

/// The `| frames: N` count of the first line of the probe's source, else 60.
fn frames_of(dir: &Path, name: &str) -> u32 {
    let src = std::fs::read_to_string(dir.join(format!("{name}.s"))).unwrap_or_default();
    src.lines()
        .next()
        .and_then(|l| l.trim_start_matches('|').trim().strip_prefix("frames:"))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(60)
}

/// Runs the probe and returns (done word, result words).
fn run(rom: &[u8], region: Region, frames: u32) -> (u16, Vec<u16>) {
    let mut md = Md::new(rom, Some(region)).expect("probe image loads");
    for _ in 0..frames {
        md.run_frame([0; 4]);
    }
    let ram = md.ram();
    let word = |i: usize| u16::from_be_bytes([ram[2 * i], ram[2 * i + 1]]);
    let count = (word(1) as usize).min(ram.len() / 2 - 2);
    (word(0), (0..count).map(|i| word(2 + i)).collect())
}

#[test]
fn probes_match_reference() {
    let dir = probes_dir();
    let mut bins: Vec<PathBuf> = std::fs::read_dir(dir.join("out"))
        .expect("probes/out exists")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect();
    bins.sort();
    let (mut ran, mut failed) = (0, Vec::new());
    for bin in &bins {
        let name = bin.file_stem().and_then(|s| s.to_str()).expect("utf-8 name").to_string();
        let rom = std::fs::read(bin).expect("probe image readable");
        for (suffix, region) in [("", Region::Americas), (".pal", Region::Europe)] {
            let expect_path = dir.join("expect").join(format!("{name}{suffix}.txt"));
            if !expect_path.exists() {
                continue;
            }
            ran += 1;
            let want = expected(&expect_path);
            let frames = frames_of(&dir, &name);
            let (done, got) = run(&rom, region, frames);
            let label = format!("{name}{suffix}");
            if done == DONE && got == want {
                println!("{label}: match ({} words)", want.len());
                continue;
            }
            let known = KNOWN_DIFF.iter().find(|(n, _)| *n == label || *n == name);
            println!(
                "{label}: DIFFERS{} (ours {}, {frames} frames)",
                known.map(|(_, why)| format!(" [known: {why}]")).unwrap_or_default(),
                if done == DONE { "done".to_string() } else { format!("not done, word 0 = {done:04X}") }
            );
            println!("   #    ref   ours");
            for i in 0..want.len().max(got.len()) {
                let r = want.get(i).map_or("----".to_string(), |w| format!("{w:04X}"));
                let o = got.get(i).map_or("----".to_string(), |w| format!("{w:04X}"));
                println!("  {i:2}   {r}   {o}{}", if r == o { "" } else { "  <<" });
            }
            if known.is_none() {
                failed.push(label);
            }
        }
    }
    println!("{ran} probe runs, {} differ without a known reason", failed.len());
    assert!(failed.is_empty(), "probes differ from the reference: {}", failed.join(" "));
}
