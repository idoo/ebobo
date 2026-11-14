//! The 68000 core against the SingleStepTests 68000 suite (github.com/SingleStepTests/680x0, folder
//! `68000/v1`): one gzipped JSON file per instruction group, about 8000 single-instruction cases each.
//!
//! Fetch the data once (203 MB, git-ignored), then run:
//!   scripts/fetch-m68k-tests.sh
//!   cargo test -p ebobo-m68k --test singlestep -- --nocapture
//! Quick runs: `M68K_FILES=ADD.b,MOVE.l` limits the files (names without `.json.gz`), `M68K_CASES=500` the cases per file:
//!   M68K_FILES=ADD.b,MOVE.l M68K_CASES=500 cargo test -p ebobo-m68k --test singlestep -- --nocapture
//!
//! How the suite's state maps onto ours (from its README):
//! - `pc` is "the formal program counter, providing a pointer to the location that the next instruction resides
//!   at", in `initial` and in `final`. That is exactly our `Cpu::pc`, so it is set and compared unchanged.
//! - `prefetch` is the two words the real chip already holds: the words at `pc` and `pc + 2` (the opcode and the
//!   word after it). Those addresses are usually not in `ram`, so the harness writes them into memory at `pc` and
//!   `pc + 2` (after `ram`, so the opcode is the one the chip ran); our core reads them from there. The final
//!   `prefetch` is the queue refilled for the next instruction: it is bus traffic, not state, and is not compared.
//! - `ram` lists `[address, byte]`; the final list holds every byte the instruction touched, all compared.
//! - `length` is the cycles of the whole instruction with immediate DTACK, compared with what `Cpu::step` returns.
//! - Address errors are not special: the case's `final` is the state after the exception was taken.
//! - `transactions` (bus order) is not compared.

use std::fs::File;
use std::io::BufReader;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ebobo_m68k::{Bus, Cpu};
use serde::Deserialize;

/// Files or cases the suite's README declares wrong, with its reason. The README at the pinned commit
/// (scripts/fetch-m68k-tests.sh) declares none, so this is empty. These files still run and print, but do not fail.
const KNOWN_BAD: &[(&str, &str)] = &[];

#[derive(Deserialize)]
struct State {
    d0: u32,
    d1: u32,
    d2: u32,
    d3: u32,
    d4: u32,
    d5: u32,
    d6: u32,
    d7: u32,
    a0: u32,
    a1: u32,
    a2: u32,
    a3: u32,
    a4: u32,
    a5: u32,
    a6: u32,
    usp: u32,
    ssp: u32,
    sr: u16,
    pc: u32,
    prefetch: [u16; 2],
    ram: Vec<(u32, u8)>,
}

impl State {
    fn d(&self) -> [u32; 8] {
        [self.d0, self.d1, self.d2, self.d3, self.d4, self.d5, self.d6, self.d7]
    }
    fn a(&self) -> [u32; 7] {
        [self.a0, self.a1, self.a2, self.a3, self.a4, self.a5, self.a6]
    }
}

#[derive(Deserialize)]
struct Case {
    name: String,
    initial: State,
    #[serde(rename = "final")]
    fin: State,
    length: u32,
}

/// 16 MB flat memory (24-bit addresses), reused between cases: written addresses are remembered and zeroed after.
struct Ram {
    mem: Vec<u8>,
    dirty: Vec<u32>,
}

const MASK: u32 = 0x00FF_FFFF;

impl Ram {
    fn new() -> Ram {
        Ram { mem: vec![0; 1 << 24], dirty: Vec::new() }
    }
    fn poke(&mut self, addr: u32, val: u8) {
        let addr = addr & MASK;
        self.mem[addr as usize] = val;
        self.dirty.push(addr);
    }
    fn clear(&mut self) {
        for &a in &self.dirty {
            self.mem[a as usize] = 0;
        }
        self.dirty.clear();
    }
}

impl Bus for Ram {
    fn read8(&mut self, addr: u32) -> u8 {
        self.mem[(addr & MASK) as usize]
    }
    fn read16(&mut self, addr: u32) -> u16 {
        let a = addr & MASK;
        u16::from_be_bytes([self.mem[a as usize], self.mem[((a + 1) & MASK) as usize]])
    }
    fn write8(&mut self, addr: u32, val: u8) {
        self.poke(addr, val);
    }
    fn write16(&mut self, addr: u32, val: u16) {
        let [hi, lo] = val.to_be_bytes();
        self.poke(addr, hi);
        self.poke(addr.wrapping_add(1), lo);
    }
}

fn data_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data"));
    if dir.is_dir() {
        Some(dir)
    } else {
        println!("68000 single-step tests missing: run scripts/fetch-m68k-tests.sh");
        None
    }
}

/// The `*.json.gz` files of `dir`, sorted, limited by `M68K_FILES`.
fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    let only: Option<Vec<String>> = std::env::var("M68K_FILES").ok().map(|s| s.split(',').map(|f| f.trim().to_string()).collect());
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .expect("read tests/data")
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_name()?.to_str()?.strip_suffix(".json.gz")?.to_string();
            Some((name, path))
        })
        .filter(|(name, _)| only.as_ref().is_none_or(|o| o.contains(name)))
        .collect();
    out.sort();
    out
}

fn load(path: &Path) -> Vec<Case> {
    let gz = flate2::read::GzDecoder::new(BufReader::new(File::open(path).expect("open test file")));
    serde_json::from_reader(BufReader::new(gz)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Runs `f` over every file on all cores; results come back in file order.
fn par_map<T: Send>(files: &[(String, PathBuf)], f: impl Fn(&str, &Path) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(files.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((name, path)) = files.get(i) else { break };
                    let r = f(name, path);
                    out.lock().unwrap().push((i, r));
                }
            });
        }
    });
    let mut out = out.into_inner().unwrap();
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, r)| r).collect()
}

/// Differences between the core after one step and the case's `final`, as `field exp X got Y`.
fn diff(case: &Case, cpu: &Cpu, ram: &Ram) -> Vec<String> {
    let f = &case.fin;
    let mut out = Vec::new();
    let mut reg = |name: String, exp: u32, got: u32| {
        if exp != got {
            out.push(format!("{name} exp {exp:08x} got {got:08x}"));
        }
    };
    for (i, (e, g)) in f.d().iter().zip(cpu.d).enumerate() {
        reg(format!("d{i}"), *e, g);
    }
    for (i, (e, g)) in f.a().iter().zip(cpu.a).enumerate() {
        reg(format!("a{i}"), *e, g);
    }
    reg("usp".into(), f.usp, cpu.usp);
    reg("ssp".into(), f.ssp, cpu.ssp);
    reg("sr".into(), f.sr as u32, cpu.sr as u32);
    reg("pc".into(), f.pc, cpu.pc);
    for &(addr, exp) in &f.ram {
        let got = ram.mem[(addr & MASK) as usize];
        if got != exp {
            out.push(format!("ram[{addr:06x}] exp {exp:02x} got {got:02x}"));
        }
    }
    out
}

struct FileResult {
    name: String,
    cases: usize,
    state_fail: usize,
    cycle_fail: usize,
    /// First failing case and its differences, or the panic.
    first: Option<String>,
    panicked: bool,
}

fn run_file(name: &str, path: &Path, limit: usize) -> FileResult {
    let cases = load(path);
    let cases = &cases[..cases.len().min(limit)];
    let mut r = FileResult { name: name.to_string(), cases: cases.len(), state_fail: 0, cycle_fail: 0, first: None, panicked: false };
    let mut ram = Ram::new();
    let current = AtomicUsize::new(0);
    let run = catch_unwind(AssertUnwindSafe(|| {
        for (i, case) in cases.iter().enumerate() {
            current.store(i, Ordering::Relaxed);
            let init = &case.initial;
            let mut cpu = Cpu::new();
            cpu.d = init.d();
            cpu.a = init.a();
            cpu.usp = init.usp;
            cpu.ssp = init.ssp;
            cpu.sr = init.sr;
            cpu.pc = init.pc;
            for &(addr, val) in &init.ram {
                ram.poke(addr, val);
            }
            for (k, word) in init.prefetch.iter().enumerate() {
                ram.write16(init.pc.wrapping_add(2 * k as u32), *word);
            }
            let cycles = cpu.step(&mut ram);
            let mut bad = diff(case, &cpu, &ram);
            if !bad.is_empty() {
                r.state_fail += 1;
            }
            if cycles != case.length {
                r.cycle_fail += 1;
                bad.push(format!("cycles exp {} got {cycles}", case.length));
            }
            if !bad.is_empty() && r.first.is_none() {
                r.first = Some(format!("{}: {}", case.name, bad.join(", ")));
            }
            ram.clear();
        }
    }));
    if let Err(e) = run {
        let msg = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default();
        let at = &cases[current.load(Ordering::Relaxed)].name;
        r.panicked = true;
        r.first = Some(format!("not implemented (panic at {at}: {msg})"));
    }
    r
}

#[test]
fn singlestep() {
    let Some(dir) = data_dir() else { return };
    let files = files(&dir);
    let limit = std::env::var("M68K_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let results = par_map(&files, |name, path| run_file(name, path, limit));
    std::panic::set_hook(hook);

    println!("{:<14} {:>6} {:>7} {:>7}  first failure", "file", "cases", "state", "cycles");
    let mut failing = Vec::new();
    let (mut passed, mut not_impl) = (0, 0);
    for r in &results {
        let ok = !r.panicked && r.state_fail == 0 && r.cycle_fail == 0;
        let known = KNOWN_BAD.iter().find(|(f, _)| *f == r.name);
        let first = r.first.as_deref().unwrap_or("");
        let note = known.map_or(String::new(), |(_, why)| format!(" [known bad: {why}]"));
        let (state, cycles) = if r.panicked { ("-".to_string(), "-".to_string()) } else { (r.state_fail.to_string(), r.cycle_fail.to_string()) };
        println!("{:<14} {:>6} {:>7} {:>7}  {first}{note}", r.name, r.cases, state, cycles);
        if ok {
            passed += 1;
        } else if r.panicked {
            not_impl += 1;
        }
        if !ok && known.is_none() {
            failing.push(r.name.as_str());
        }
    }
    println!("{} files: {passed} pass, {not_impl} not implemented, {} with failures", results.len(), failing.len());
    assert!(failing.is_empty(), "failing files: {}", failing.join(" "));
}

#[test]
fn data_loads() {
    let Some(dir) = data_dir() else { return };
    let files = files(&dir);
    assert!(!files.is_empty(), "no *.json.gz in {}", dir.display());
    let counts = par_map(&files, |_, path| load(path).len());
    for ((name, _), n) in files.iter().zip(&counts) {
        assert!(*n > 0, "{name}: no cases");
    }
    println!("{} files, {} cases", files.len(), counts.iter().sum::<usize>());
}
