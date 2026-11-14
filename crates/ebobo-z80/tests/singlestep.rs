//! The Z80 core against the SingleStepTests suite (github.com/SingleStepTests/z80, folder `v1`):
//! one JSON file per opcode, 1000 cases each, every case one instruction from a known state.
//!
//! Fetch the data once (1.4 GB, git-ignored), then run:
//!
//! ```text
//! scripts/fetch-z80-tests.sh
//! cargo test -p ebobo-z80 --test singlestep -- --nocapture
//! Z80_FILES="00,cb 00,ed 40" Z80_CASES=100 cargo test -p ebobo-z80 --test singlestep -- --nocapture
//! ```
//!
//! `Z80_FILES` is a comma-separated list of file names without `.json` (the suite names them by
//! their opcode bytes with spaces, e.g. `dd cb __ 06`); `Z80_CASES` caps the cases per file.
//!
//! Per case, following the suite's README: the CPU and a 64 KB RAM bus are loaded from
//! `initial`, [`Cpu::step`] runs once, and then every register and flag field of `final`
//! (including `wz`, `q`, `p`, `ei`), every listed RAM byte and the port writes in `ports` are
//! compared, and the returned T-states against the length of `cycles` (one entry per T-state).
//! `ports` entries marked `r` are the values `port_in` returns. The README declares no case
//! undefined, so every case counts. Bus-cycle shapes inside `cycles` are not compared.
//!
//! With the data folder missing both tests print the fetch command and pass.

use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ebobo_z80::{Bus, Cpu};
use serde_json::Value;

const FETCH: &str = "scripts/fetch-z80-tests.sh";

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// All test files, sorted by name.
fn all_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> =
        rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    files
}

fn stem(path: &Path) -> String {
    path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
}

/// The opcode group of a file name such as `00`, `cb 1f`, `dd cb __ 06`.
fn group(stem: &str) -> &'static str {
    let parts: Vec<&str> = stem.split(' ').collect();
    match (parts[0], parts.get(1).copied()) {
        ("cb", _) => "CB",
        ("ed", _) => "ED",
        ("dd", Some("cb")) => "DDCB",
        ("fd", Some("cb")) => "FDCB",
        ("dd", _) => "DD",
        ("fd", _) => "FD",
        _ => "unprefixed",
    }
}

const GROUPS: [&str; 7] = ["unprefixed", "CB", "ED", "DD", "FD", "DDCB", "FDCB"];

/// 64 KB of RAM plus the port transactions of one case.
struct RamBus {
    ram: Box<[u8; 0x10000]>,
    /// Addresses written or loaded in this case, cleared before the next one.
    touched: Vec<u16>,
    /// `(port, value)` that `port_in` returns, in order.
    reads: Vec<(u16, u8)>,
    next_read: usize,
    /// `(port, value)` seen by `port_out`.
    writes: Vec<(u16, u8)>,
}

impl RamBus {
    fn new() -> RamBus {
        RamBus { ram: Box::new([0; 0x10000]), touched: Vec::new(), reads: Vec::new(), next_read: 0, writes: Vec::new() }
    }

    fn clear(&mut self) {
        for &a in &self.touched {
            self.ram[a as usize] = 0;
        }
        self.touched.clear();
        self.reads.clear();
        self.next_read = 0;
        self.writes.clear();
    }
}

impl Bus for RamBus {
    fn read(&mut self, addr: u16) -> u8 {
        self.ram[addr as usize]
    }
    fn write(&mut self, addr: u16, val: u8) {
        self.ram[addr as usize] = val;
        self.touched.push(addr);
    }
    fn port_in(&mut self, port: u16) -> u8 {
        // The listed value for this port, in the order the suite lists them; else an open bus.
        match self.reads[self.next_read.min(self.reads.len())..].iter().position(|&(p, _)| p == port) {
            Some(i) => {
                let v = self.reads[self.next_read + i].1;
                self.next_read += i + 1;
                v
            }
            None => 0xFF,
        }
    }
    fn port_out(&mut self, port: u16, val: u8) {
        self.writes.push((port, val));
    }
}

fn num(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

/// Every register and flag field the suite names, read from or written to the CPU as `u64`.
const FIELDS: [&str; 25] = [
    "a", "f", "b", "c", "d", "e", "h", "l", "i", "r", "af_", "bc_", "de_", "hl_", "ix", "iy", "pc", "sp", "wz", "iff1", "iff2", "im",
    "ei", "p", "q",
];

fn get(cpu: &Cpu, name: &str) -> u64 {
    match name {
        "a" => cpu.a.into(),
        "f" => cpu.f.into(),
        "b" => cpu.b.into(),
        "c" => cpu.c.into(),
        "d" => cpu.d.into(),
        "e" => cpu.e.into(),
        "h" => cpu.h.into(),
        "l" => cpu.l.into(),
        "i" => cpu.i.into(),
        "r" => cpu.r.into(),
        "af_" => cpu.af_.into(),
        "bc_" => cpu.bc_.into(),
        "de_" => cpu.de_.into(),
        "hl_" => cpu.hl_.into(),
        "ix" => cpu.ix.into(),
        "iy" => cpu.iy.into(),
        "pc" => cpu.pc.into(),
        "sp" => cpu.sp.into(),
        "wz" => cpu.wz.into(),
        "iff1" => cpu.iff1.into(),
        "iff2" => cpu.iff2.into(),
        "im" => cpu.im.into(),
        "ei" => cpu.ei.into(),
        "p" => cpu.p.into(),
        "q" => cpu.q.into(),
        _ => unreachable!("unknown field {name}"),
    }
}

fn set(cpu: &mut Cpu, name: &str, v: u64) {
    let (b, w, f) = (v as u8, v as u16, v != 0);
    match name {
        "a" => cpu.a = b,
        "f" => cpu.f = b,
        "b" => cpu.b = b,
        "c" => cpu.c = b,
        "d" => cpu.d = b,
        "e" => cpu.e = b,
        "h" => cpu.h = b,
        "l" => cpu.l = b,
        "i" => cpu.i = b,
        "r" => cpu.r = b,
        "af_" => cpu.af_ = w,
        "bc_" => cpu.bc_ = w,
        "de_" => cpu.de_ = w,
        "hl_" => cpu.hl_ = w,
        "ix" => cpu.ix = w,
        "iy" => cpu.iy = w,
        "pc" => cpu.pc = w,
        "sp" => cpu.sp = w,
        "wz" => cpu.wz = w,
        "iff1" => cpu.iff1 = f,
        "iff2" => cpu.iff2 = f,
        "im" => cpu.im = b,
        "ei" => cpu.ei = f,
        "p" => cpu.p = f,
        "q" => cpu.q = b,
        _ => unreachable!("unknown field {name}"),
    }
}

fn ram_pairs(state: &Value) -> impl Iterator<Item = (u16, u8)> + '_ {
    state["ram"].as_array().into_iter().flatten().filter_map(|e| Some((e[0].as_u64()? as u16, e[1].as_u64()? as u8)))
}

fn ports(case: &Value, kind: &str) -> Vec<(u16, u8)> {
    case["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e[2].as_str() == Some(kind))
        .filter_map(|e| Some((e[0].as_u64()? as u16, e[1].as_u64()? as u8)))
        .collect()
}

/// Runs one case. Returns the differing state fields (empty when the state matches) and the
/// timing difference `(expected, got)` when the T-states differ.
fn run_case(case: &Value, bus: &mut RamBus) -> (Vec<String>, Option<(usize, u32)>) {
    let (init, fin) = (&case["initial"], &case["final"]);
    bus.clear();
    let mut cpu = Cpu::default();
    for name in FIELDS {
        if let Some(v) = num(init, name) {
            set(&mut cpu, name, v);
        }
    }
    for (a, v) in ram_pairs(init) {
        bus.ram[a as usize] = v;
        bus.touched.push(a);
    }
    bus.reads = ports(case, "r");

    let t = cpu.step(bus);

    let mut diffs = Vec::new();
    for name in FIELDS {
        if let Some(want) = num(fin, name) {
            let got = get(&cpu, name);
            if got != want {
                diffs.push(format!("{name}: expected {want:#x} got {got:#x}"));
            }
        }
    }
    for (a, want) in ram_pairs(fin) {
        let got = bus.ram[a as usize];
        if got != want {
            diffs.push(format!("ram[{a:#06x}]: expected {want:#04x} got {got:#04x}"));
        }
    }
    let want_out = ports(case, "w");
    if bus.writes != want_out {
        diffs.push(format!("port writes: expected {want_out:x?} got {:x?}", bus.writes));
    }
    let cycles = case["cycles"].as_array().map_or(0, Vec::len);
    let timing = (t as usize != cycles).then_some((cycles, t));
    (diffs, timing)
}

/// Result of one file.
struct FileResult {
    name: String,
    cases: usize,
    state_fail: usize,
    timing_fail: usize,
    /// `step` panicked (the core does not implement this opcode yet).
    panicked: bool,
    /// The first failing case: its name and what differed.
    first: Option<String>,
}

impl FileResult {
    fn failed(&self) -> bool {
        self.panicked || self.state_fail > 0 || self.timing_fail > 0
    }
    /// Ranking for the worst-files table: a panicking file counts every case as failed.
    fn badness(&self) -> usize {
        if self.panicked { 2 * self.cases } else { self.state_fail + self.timing_fail }
    }
}

fn load(path: &Path) -> Vec<Value> {
    let text = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_slice(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn run_file(path: &Path, max_cases: usize, bus: &mut RamBus) -> FileResult {
    let cases = load(path);
    let n = cases.len().min(max_cases);
    let mut r = FileResult { name: stem(path), cases: n, state_fail: 0, timing_fail: 0, panicked: false, first: None };
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut out = (0, 0, None);
        for case in &cases[..n] {
            let (diffs, timing) = run_case(case, bus);
            out.0 += usize::from(!diffs.is_empty());
            out.1 += usize::from(timing.is_some());
            if out.2.is_none() && (!diffs.is_empty() || timing.is_some()) {
                let mut what = diffs;
                if let Some((want, got)) = timing {
                    what.push(format!("t-states: expected {want} got {got}"));
                }
                out.2 = Some(format!("{}: {}", case["name"].as_str().unwrap_or("?"), what.join(", ")));
            }
        }
        out
    }));
    match outcome {
        Ok((s, t, first)) => (r.state_fail, r.timing_fail, r.first) = (s, t, first),
        Err(_) => r.panicked = true,
    }
    r
}

#[test]
fn singlestep() {
    let dir = data_dir();
    let mut files = all_files(&dir);
    if files.is_empty() {
        println!("no Z80 test data in {}; fetch it with {FETCH}", dir.display());
        return;
    }
    if let Ok(list) = std::env::var("Z80_FILES") {
        let want: Vec<&str> = list.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        files.retain(|p| want.contains(&stem(p).as_str()));
        assert_eq!(files.len(), want.len(), "Z80_FILES names a file that is not in {}", dir.display());
    }
    let max_cases = std::env::var("Z80_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);

    // Panics are expected while the core is unfinished: keep them quiet and count them.
    let hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(files.len()));
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut bus = RamBus::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = files.get(i) else { break };
                    let r = run_file(path, max_cases, &mut bus);
                    results.lock().unwrap().push(r);
                }
            });
        }
    });
    panic::set_hook(hook);
    let mut results = results.into_inner().unwrap();
    results.sort_by(|a, b| a.name.cmp(&b.name));

    // Per group: files, failed files, not implemented files, cases, state and timing failures.
    let mut groups: BTreeMap<&str, [usize; 6]> = BTreeMap::new();
    for r in &results {
        let g = groups.entry(group(&r.name)).or_default();
        g[0] += 1;
        g[1] += usize::from(r.failed());
        g[2] += usize::from(r.panicked);
        g[3] += r.cases;
        g[4] += r.state_fail;
        g[5] += r.timing_fail;
    }
    println!("\n{:<11} {:>6} {:>7} {:>8} {:>9} {:>11} {:>12}", "group", "files", "failed", "not impl", "cases", "state fail", "timing fail");
    for name in GROUPS {
        if let Some(g) = groups.get(name) {
            println!("{name:<11} {:>6} {:>7} {:>8} {:>9} {:>11} {:>12}", g[0], g[1], g[2], g[3], g[4], g[5]);
        }
    }

    let mut worst: Vec<&FileResult> = results.iter().filter(|r| r.failed()).collect();
    worst.sort_by(|a, b| b.badness().cmp(&a.badness()).then(a.name.cmp(&b.name)));
    if !worst.is_empty() {
        println!("\nworst files:\n{:<14} {:>6} {:>11} {:>12}", "file", "cases", "state fail", "timing fail");
        for r in worst.iter().take(20) {
            if r.panicked {
                println!("{:<14} {:>6} {:>24}", r.name, r.cases, "not implemented");
            } else {
                println!("{:<14} {:>6} {:>11} {:>12}", r.name, r.cases, r.state_fail, r.timing_fail);
            }
        }
        let not_impl: Vec<&str> = results.iter().filter(|r| r.panicked).map(|r| r.name.as_str()).collect();
        if !not_impl.is_empty() {
            println!("\nnot implemented ({} files): {}", not_impl.len(), not_impl.join(", "));
        }
        println!("\nfirst failing case per file:");
        for r in results.iter().filter(|r| r.first.is_some()) {
            println!("{:<14} {}", r.name, r.first.as_deref().unwrap_or_default());
        }
    }
    let failed = worst.len();
    assert_eq!(failed, 0, "{failed} of {} Z80 test files fail", results.len());
}

#[test]
fn data_loads() {
    let dir = data_dir();
    let files = all_files(&dir);
    if files.is_empty() {
        println!("no Z80 test data in {}; fetch it with {FETCH}", dir.display());
        return;
    }
    assert!(files.len() >= 1500, "only {} test files in {}; rerun {FETCH}", files.len(), dir.display());
    let path = dir.join("00.json");
    let cases = load(&path);
    assert_eq!(cases.len(), 1000, "{}", path.display());
    let case = &cases[0];
    assert!(case["name"].is_string() && case["initial"]["pc"].is_u64() && case["final"]["ram"].is_array());
    assert!(!case["cycles"].as_array().expect("cycles").is_empty());
}
