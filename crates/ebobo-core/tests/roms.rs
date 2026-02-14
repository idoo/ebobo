//! Test-ROM suites (blargg et al.). Prints a PASS/FAIL table with `--nocapture`; asserts only
//! on `MUST_PASS` so the suite guards regressions while the rest stays informational.

mod common;

use ebobo_core::{Nes, Region};
use std::path::{Path, PathBuf};

/// ROMs (path relative to tests/roms) that passed on the first full run.
const MUST_PASS: &[&str] = &[
    "instr_test-v5/rom_singles/01-basics.nes",
    "instr_test-v5/rom_singles/02-implied.nes",
    "instr_test-v5/rom_singles/04-zero_page.nes",
    "instr_test-v5/rom_singles/05-zp_xy.nes",
    "instr_test-v5/rom_singles/06-absolute.nes",
    "instr_test-v5/rom_singles/08-ind_x.nes",
    "instr_test-v5/rom_singles/09-ind_y.nes",
    "instr_test-v5/rom_singles/10-branches.nes",
    "instr_test-v5/rom_singles/11-stack.nes",
    "instr_test-v5/rom_singles/12-jmp_jsr.nes",
    "instr_test-v5/rom_singles/13-rts.nes",
    "instr_test-v5/rom_singles/14-rti.nes",
    "instr_test-v5/rom_singles/15-brk.nes",
    "instr_test-v5/rom_singles/16-special.nes",
    "cpu_timing_test6/cpu_timing_test.nes",
    "branch_timing_tests/1.Branch_Basics.nes",
    "branch_timing_tests/2.Backward_Branch.nes",
    "branch_timing_tests/3.Forward_Branch.nes",
    "sprite_hit_tests_2005.10.05/01.basics.nes",
    "sprite_hit_tests_2005.10.05/02.alignment.nes",
    "sprite_hit_tests_2005.10.05/03.corners.nes",
    "sprite_hit_tests_2005.10.05/04.flip.nes",
    "sprite_hit_tests_2005.10.05/05.left_clip.nes",
    "sprite_hit_tests_2005.10.05/06.right_edge.nes",
    "sprite_hit_tests_2005.10.05/07.screen_bottom.nes",
    "sprite_hit_tests_2005.10.05/08.double_height.nes",
    "sprite_hit_tests_2005.10.05/09.timing_basics.nes",
    "sprite_hit_tests_2005.10.05/10.timing_order.nes",
    "sprite_hit_tests_2005.10.05/11.edge_timing.nes",
    "blargg_ppu_tests_2005.09.15b/palette_ram.nes",
    "blargg_ppu_tests_2005.09.15b/sprite_ram.nes",
    "blargg_ppu_tests_2005.09.15b/vbl_clear_time.nes",
    "blargg_ppu_tests_2005.09.15b/vram_access.nes",
    "ppu_read_buffer/test_ppu_read_buffer.nes",
    "ppu_vbl_nmi/rom_singles/02-vbl_set_time.nes",
    "ppu_vbl_nmi/rom_singles/06-suppression.nes",
    "ppu_vbl_nmi/rom_singles/08-nmi_off_timing.nes",
    "ppu_vbl_nmi/rom_singles/01-vbl_basics.nes",
    "ppu_vbl_nmi/rom_singles/03-vbl_clear_time.nes",
    "ppu_vbl_nmi/rom_singles/04-nmi_control.nes",
    "ppu_vbl_nmi/rom_singles/09-even_odd_frames.nes",
    "ppu_vbl_nmi/rom_singles/10-even_odd_timing.nes",
    "ppu_vbl_nmi/rom_singles/05-nmi_timing.nes",
    "ppu_vbl_nmi/rom_singles/07-nmi_on_timing.nes",
    "mmc3_test_2/rom_singles/1-clocking.nes",
    "mmc3_test_2/rom_singles/2-details.nes",
    "mmc3_test_2/rom_singles/3-A12_clocking.nes",
    "mmc3_test_2/rom_singles/5-MMC3.nes",
    "cpu_interrupts_v2/rom_singles/2-nmi_and_brk.nes",
    "cpu_interrupts_v2/rom_singles/1-cli_latency.nes",
    "cpu_interrupts_v2/rom_singles/5-branch_delays_irq.nes",
    "cpu_interrupts_v2/rom_singles/3-nmi_and_irq.nes",
    "apu_test/rom_singles/1-len_ctr.nes",
    "apu_test/rom_singles/2-len_table.nes",
    "apu_test/rom_singles/3-irq_flag.nes",
    "apu_test/rom_singles/4-jitter.nes",
    "apu_test/rom_singles/5-len_timing.nes",
    "apu_test/rom_singles/6-irq_flag_timing.nes",
    "apu_test/rom_singles/7-dmc_basics.nes",
    "apu_test/rom_singles/8-dmc_rates.nes",
    "apu_reset/4015_cleared.nes",
    "apu_reset/4017_timing.nes",
    "apu_reset/4017_written.nes",
    "apu_reset/irq_flag_cleared.nes",
    "apu_reset/len_ctrs_enabled.nes",
    "apu_reset/works_immediately.nes",
    "blargg_apu_2005.07.30/01.len_ctr.nes",
    "blargg_apu_2005.07.30/02.len_table.nes",
    "blargg_apu_2005.07.30/03.irq_flag.nes",
    "blargg_apu_2005.07.30/04.clock_jitter.nes",
    "blargg_apu_2005.07.30/05.len_timing_mode0.nes",
    "blargg_apu_2005.07.30/06.len_timing_mode1.nes",
    "blargg_apu_2005.07.30/07.irq_flag_timing.nes",
    "blargg_apu_2005.07.30/08.irq_timing.nes",
    "blargg_apu_2005.07.30/09.reset_timing.nes",
    "blargg_apu_2005.07.30/10.len_halt_timing.nes",
    "blargg_apu_2005.07.30/11.len_reload_timing.nes",
    "pal_apu_tests/01.len_ctr.nes",
    "pal_apu_tests/02.len_table.nes",
    "pal_apu_tests/03.irq_flag.nes",
    "pal_apu_tests/04.clock_jitter.nes",
    "pal_apu_tests/05.len_timing_mode0.nes",
    "pal_apu_tests/06.len_timing_mode1.nes",
    "pal_apu_tests/07.irq_flag_timing.nes",
    "pal_apu_tests/08.irq_timing.nes",
    "pal_apu_tests/10.len_halt_timing.nes",
    "pal_apu_tests/11.len_reload_timing.nes",
];

const TIMEOUT_FRAMES: u32 = 3600;

fn list(roms: &Path, sub: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(roms.join(sub))
        .unwrap_or_else(|e| panic!("{sub}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "nes"))
        .collect();
    v.sort();
    v
}

enum Outcome {
    Pass,
    Fail(String),
    /// Ran to the end; result only visible on screen (frame hash for golden pinning).
    Ran(u64),
}

/// How a suite reports its result.
#[derive(Clone, Copy)]
enum Mode {
    /// $6000 protocol (signature DE B0 61 at $6001).
    Blargg,
    /// Older blargg ROMs: result byte in zero page (1 = pass) once it stops changing.
    Legacy(u16),
    /// Result only drawn on screen: run N frames, then compare the frame hash with
    /// `SCREEN_GOLDEN` (hashes of frames visually confirmed to show "passed").
    Screen(u32),
}

/// (ROM, frame hash) of screen-only ROMs whose final frame was checked to read "PASSED".
const SCREEN_GOLDEN: &[(&str, u64)] = &[
    ("cpu_timing_test6/cpu_timing_test.nes", 0x37a1_13c2_4609_4088),
    ("cpu_dummy_reads/cpu_dummy_reads.nes", 0x9bbb_c82d_4896_7bb8),
];

fn blargg_valid(nes: &Nes) -> bool {
    nes.peek(0x6001) == 0xDE && nes.peek(0x6002) == 0xB0 && nes.peek(0x6003) == 0x61
}

fn blargg_text(nes: &Nes) -> String {
    let mut s = Vec::new();
    for a in 0x6004u16..0x7FFF {
        let b = nes.peek(a);
        if b == 0 {
            break;
        }
        s.push(b);
    }
    String::from_utf8_lossy(&s).trim().replace('\n', " | ")
}

fn frame_hash(nes: &Nes) -> u64 {
    nes.frame().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &px| {
        (h ^ px as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

fn run(path: &Path, region: Option<Region>, mode: Mode) -> Outcome {
    let rom = std::fs::read(path).unwrap();
    let mut nes = match Nes::new(&rom, region) {
        Ok(n) => n,
        Err(e) => return Outcome::Fail(format!("load error: {e:?}")),
    };
    match mode {
        Mode::Screen(frames) => {
            for _ in 0..frames {
                nes.run_frame([0; 4]);
            }
            Outcome::Ran(frame_hash(&nes))
        }
        Mode::Blargg => {
            // `armed` is cleared by a reset and set again once the ROM reports $80 (running),
            // so a stale $81 right after the reset doesn't trigger a second one.
            let mut reset_in: Option<u32> = None;
            let mut armed = true;
            for _ in 0..TIMEOUT_FRAMES {
                nes.run_frame([0; 4]);
                if !blargg_valid(&nes) {
                    continue;
                }
                match nes.peek(0x6000) {
                    0x80 => armed = true,
                    0x81 if armed => match reset_in {
                        None => reset_in = Some(6),
                        Some(0) => {
                            nes.reset();
                            reset_in = None;
                            armed = false;
                        }
                        Some(n) => reset_in = Some(n - 1),
                    },
                    0x81 => {}
                    0 => return Outcome::Pass,
                    code => return Outcome::Fail(format!("${code:02X}: {}", blargg_text(&nes))),
                }
            }
            if blargg_valid(&nes) {
                let status = nes.peek(0x6000);
                Outcome::Fail(format!("timeout ($6000=${status:02X}): {}", blargg_text(&nes)))
            } else {
                Outcome::Fail("timeout (no $6000 signature)".into())
            }
        }
        Mode::Legacy(addr) => {
            let (mut last, mut stable) = (0u8, 0u32);
            for frame in 0..TIMEOUT_FRAMES {
                nes.run_frame([0; 4]);
                let v = nes.peek(addr);
                stable = if v != 0 && v == last { stable + 1 } else { 0 };
                last = v;
                if stable >= 60 && frame >= 120 {
                    return if v == 1 {
                        Outcome::Pass
                    } else {
                        Outcome::Fail(format!("${addr:02X} = {v}"))
                    };
                }
            }
            Outcome::Fail(format!("timeout (${addr:02X}={})", nes.peek(addr)))
        }
    }
}

fn suite(sub: &str, region: Option<Region>, mode: Mode) {
    let Some(dir) = common::roms() else { return };
    let roms = list(&dir, sub);
    let (mut pass, mut fail, mut ran, mut regressions) = (0, 0, 0, Vec::new());
    println!("== {sub} ==");
    for p in &roms {
        let rel = p.strip_prefix(&dir).unwrap().to_string_lossy().into_owned();
        let out = match run(p, region, mode) {
            Outcome::Ran(hash) => match SCREEN_GOLDEN.iter().find(|g| g.0 == rel) {
                Some(&(_, want)) if want == hash => Outcome::Pass,
                Some(&(_, want)) => {
                    Outcome::Fail(format!("frame hash {hash:016x}, golden {want:016x}"))
                }
                None => Outcome::Ran(hash),
            },
            other => other,
        };
        match &out {
            Outcome::Pass => {
                pass += 1;
                println!("PASS {rel}");
            }
            Outcome::Ran(hash) => {
                ran += 1;
                println!("RAN  {rel} (screen only, frame hash {hash:016x})");
            }
            Outcome::Fail(m) => {
                fail += 1;
                println!("FAIL {rel}: {m}");
            }
        }
        if !matches!(out, Outcome::Pass) && MUST_PASS.contains(&rel.as_str()) {
            regressions.push(rel);
        }
    }
    println!("-- {sub}: {pass} pass, {fail} fail, {ran} screen-only of {}", roms.len());
    assert!(regressions.is_empty(), "MUST_PASS regressions: {regressions:?}");
}

macro_rules! suites {
    ($($name:ident => $dir:expr, $region:expr, $mode:expr;)*) => {
        $(#[test] fn $name() { suite($dir, $region, $mode); })*
    };
}

// Result conventions per suite: readme.txt files; the 2005 ROMs only draw the result, but
// keep it in zero page too ($F0 for blargg_ppu/blargg_apu, $F8 for the others, found by RAM
// dumps of finished runs).
suites! {
    instr_test_v5 => "instr_test-v5/rom_singles", None, Mode::Blargg;
    instr_misc => "instr_misc/rom_singles", None, Mode::Blargg;
    instr_timing => "instr_timing/rom_singles", None, Mode::Blargg;
    cpu_timing_test6 => "cpu_timing_test6", None, Mode::Screen(1200);
    branch_timing_tests => "branch_timing_tests", None, Mode::Legacy(0xF8);
    cpu_dummy_reads => "cpu_dummy_reads", None, Mode::Screen(600);
    cpu_dummy_writes => "cpu_dummy_writes", None, Mode::Blargg;
    cpu_interrupts_v2 => "cpu_interrupts_v2/rom_singles", None, Mode::Blargg;
    cpu_reset => "cpu_reset", None, Mode::Blargg;
    ppu_vbl_nmi => "ppu_vbl_nmi/rom_singles", None, Mode::Blargg;
    ppu_read_buffer => "ppu_read_buffer", None, Mode::Blargg;
    ppu_open_bus => "ppu_open_bus", None, Mode::Blargg;
    sprite_hit_tests => "sprite_hit_tests_2005.10.05", None, Mode::Legacy(0xF8);
    sprite_overflow_tests => "sprite_overflow_tests", None, Mode::Legacy(0xF8);
    blargg_ppu_tests => "blargg_ppu_tests_2005.09.15b", None, Mode::Legacy(0xF0);
    oam_read => "oam_read", None, Mode::Blargg;
    oam_stress => "oam_stress", None, Mode::Blargg;
    mmc3_test_2 => "mmc3_test_2/rom_singles", None, Mode::Blargg;
    mmc1_a12 => "MMC1_A12", None, Mode::Screen(300);
    apu_test => "apu_test/rom_singles", None, Mode::Blargg;
    apu_reset => "apu_reset", None, Mode::Blargg;
    blargg_apu => "blargg_apu_2005.07.30", None, Mode::Legacy(0xF0);
    pal_apu_tests => "pal_apu_tests", Some(Region::Pal), Mode::Legacy(0xF8);
    sprdma_and_dmc_dma => "sprdma_and_dmc_dma", None, Mode::Blargg;
    dmc_dma_during_read4 => "dmc_dma_during_read4", None, Mode::Screen(600);
    read_joy3 => "read_joy3", None, Mode::Screen(600);
}
