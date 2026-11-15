//! The 68000 core against the MAME-generated single-step suite (github.com/SingleStepTests/m68000, folder `v1`):
//! one `*.json.bin` file per instruction group, generated from MAME's microcoded 68000. Its README: "all of the
//! tests except TAS and TRAPV are verified as good". This is the reference suite; `tests/singlestep.rs` runs the
//! older one, which has known wrong cases.
//!
//! Fetch the data once (git-ignored, into `tests/data/mame`), then run:
//!   scripts/fetch-m68k-tests.sh
//!   cargo test -p ebobo-m68k --test mame -- --nocapture
//! Quick runs: `M68K_FILES=ADD.b,MOVE.l` limits the files (names without `.json.bin`), `M68K_CASES=500` the cases per file.
//!
//! The file format is the one `decode.py` of that repository reads (all little-endian, u32 unless said):
//!   file:  magic 1A3F5D71, case count, cases
//!   case:  byte count, magic ABC12367, name, initial state, final state, transactions
//!   name:  byte count, magic 89ABCDEF, length, UTF-8 bytes
//!   state: byte count, magic 01234567, d0-d7, a0-a6, usp, ssp, sr, pc, prefetch 0, prefetch 1, RAM count,
//!          then per RAM entry: address (u32), big-endian word value (u16)
//!   transactions: byte count, magic 456789AB, cycle count (`length`), transaction count, then per transaction:
//!          kind (u8: 0 idle, 1 write, 2 read, 3 TAS, 4 read address error, 5 write address error), cycles, and
//!          unless idle: function code, address bus, data bus, UDS, LDS
//!
//! How the suite's state maps onto ours (from its README, and from the cases):
//! - RAM is listed as 16-bit words: the high byte lives at the address, the low byte at address | 1.
//! - `pc` is MAME's `m_au`, the next prefetch address: in `initial` it is 4 past the instruction that runs. Our
//!   `Cpu::pc` is the address of the instruction, so the harness sets `pc - 4`.
//! - `prefetch` is the two words at the instruction's address: written to memory at `pc - 4` and `pc - 2` (after
//!   `ram`). The final `prefetch` is bus traffic, not compared.
//! - Final `pc`: see `final_pc` below for the rule found in the cases.
//! - Trace: the suite's `final` is the instruction boundary; for a case that starts with T set the trace exception
//!   is still pending there. `Cpu::step` stops at the same point (`Cpu::trace_pending`), so these compare directly.
//! - `transactions` are not compared. Kinds `re` / `we` are address-error cycles that never reach memory (AS is not
//!   asserted); the harness only notes that a case has one, to split the failures into address-error cases and others.
//! - `length` (the cycle count) is compared with what `Cpu::step` returns.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ebobo_m68k::{Bus, Cpu};

/// Instructions the suite's README declares not verified, with its reason. Matched against the file name up to the
/// first dot. These files still run and print, but do not fail the test.
const KNOWN: &[(&str, &str)] = &[
    ("TAS", "README: TAS doesn't properly handle the special 5-cycle TAS read-modify-write timing"),
    ("TRAPV", "README: strange issue with the TRAPV tests, appears to trigger incorrectly based on the S bit"),
];

/// What our `Cpu::pc` must be after the step, given the suite's final `pc`.
///
/// The suite's `pc` is always the next prefetch address (MAME's `m_au`) at the instruction boundary, when the two
/// prefetch words of the next instruction are already loaded: 4 past the instruction that runs next. That holds
/// for a straight-line instruction (final = address after the instruction + 4), a taken branch (target + 4) and
/// an exception (handler address from the vector + 4). So the conversion is the same as on the way in.
/// Checked over whole files with decode.py: NOP (2500 of 2500: initial + 2), Bcc (1156 not taken: next + 4,
/// 674 taken: target + 4, 670 address errors on an odd target: vector 3 handler + 4), TRAP (2500: vector handler + 4),
/// MOVE.w (1013 straight: initial + instruction length, 1487 address errors: vector 3 handler + 4).
///
/// STOP is the one exception: a CPU that stopped has read the immediate word but does not refill the queue for
/// the instruction after it, so `m_au` stays where it was when STOP began: STOP's address + 4, which is the address
/// of the instruction after STOP (all 598 supervisor-mode cases of STOP.json.bin: final `pc` = initial `pc`,
/// `length` 4). That is the state of the address counter only: an interrupt out of STOP stacks STOP's address + 4,
/// which is exactly what our `pc` holds, so for a stopped CPU the suite's `pc` is compared unchanged.
fn final_pc(suite_pc: u32, stopped: bool) -> u32 {
    if stopped { suite_pc } else { suite_pc.wrapping_sub(4) }
}

struct State {
    d: [u32; 8],
    a: [u32; 7],
    usp: u32,
    ssp: u32,
    sr: u32,
    pc: u32,
    prefetch: [u32; 2],
    /// `(address, word)`: high byte at `address`, low byte at `address | 1`.
    ram: Vec<(u32, u16)>,
}

struct Case {
    name: String,
    initial: State,
    fin: State,
    /// Cycles of the whole instruction.
    length: u32,
    /// The transactions hold an `re` or `we`: the case ends in an address error.
    addr_error: bool,
}

/// Little-endian reader over a test file.
struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl Rd<'_> {
    fn u8(&mut self) -> u8 {
        self.p += 1;
        self.b[self.p - 1]
    }
    fn u16(&mut self) -> u16 {
        self.p += 2;
        u16::from_le_bytes([self.b[self.p - 2], self.b[self.p - 1]])
    }
    fn u32(&mut self) -> u32 {
        self.p += 4;
        u32::from_le_bytes(self.b[self.p - 4..self.p].try_into().unwrap())
    }
    /// Skips the byte count of a block and checks its magic number.
    fn block(&mut self, magic: u32) {
        self.u32();
        let got = self.u32();
        assert_eq!(got, magic, "bad magic at byte {}", self.p - 4);
    }
    fn state(&mut self) -> State {
        self.block(0x0123_4567);
        let mut r = [0u32; 19];
        for v in &mut r {
            *v = self.u32();
        }
        let prefetch = [self.u32(), self.u32()];
        let n = self.u32();
        let ram = (0..n).map(|_| (self.u32(), self.u16())).collect();
        State { d: r[0..8].try_into().unwrap(), a: r[8..15].try_into().unwrap(), usp: r[15], ssp: r[16], sr: r[17], pc: r[18], prefetch, ram }
    }
    fn case(&mut self) -> Case {
        self.block(0xABC1_2367);
        self.block(0x89AB_CDEF);
        let len = self.u32() as usize;
        let name = String::from_utf8(self.b[self.p..self.p + len].to_vec()).expect("name is UTF-8");
        self.p += len;
        let initial = self.state();
        let fin = self.state();
        self.block(0x4567_89AB);
        let length = self.u32();
        let count = self.u32();
        let mut addr_error = false;
        for _ in 0..count {
            let kind = self.u8();
            self.u32();
            assert!(kind <= 5, "bad transaction kind {kind}");
            if kind != 0 {
                self.p += 20;
            }
            addr_error |= kind >= 4;
        }
        Case { name, initial, fin, length, addr_error }
    }
}

/// Decodes a whole `*.json.bin` file; panics unless it parses exactly to its end.
fn decode(bytes: &[u8]) -> Vec<Case> {
    let mut rd = Rd { b: bytes, p: 0 };
    assert_eq!(rd.u32(), 0x1A3F_5D71, "bad file magic");
    let n = rd.u32();
    let cases = (0..n).map(|_| rd.case()).collect();
    assert_eq!(rd.p, bytes.len(), "trailing bytes after the last case");
    cases
}

fn load(path: &Path) -> Vec<Case> {
    decode(&std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
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
    /// A suite word: high byte at `addr`, low byte at `addr | 1`.
    fn poke_word(&mut self, addr: u32, val: u16) {
        self.poke(addr, (val >> 8) as u8);
        self.poke(addr | 1, val as u8);
    }
    fn word(&self, addr: u32) -> u16 {
        u16::from_be_bytes([self.mem[(addr & MASK) as usize], self.mem[((addr | 1) & MASK) as usize]])
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
    let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/mame"));
    if dir.is_dir() {
        Some(dir)
    } else {
        println!("68000 MAME single-step tests missing: run scripts/fetch-m68k-tests.sh");
        None
    }
}

/// The `*.json.bin` files of `dir`, sorted, limited by `M68K_FILES`.
fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    let only: Option<Vec<String>> = std::env::var("M68K_FILES").ok().map(|s| s.split(',').map(|f| f.trim().to_string()).collect());
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .expect("read tests/data/mame")
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_name()?.to_str()?.strip_suffix(".json.bin")?.to_string();
            Some((name, path))
        })
        .filter(|(name, _)| only.as_ref().is_none_or(|o| o.contains(name)))
        .collect();
    out.sort();
    out
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
    for (i, (e, g)) in f.d.iter().zip(cpu.d).enumerate() {
        reg(format!("d{i}"), *e, g);
    }
    for (i, (e, g)) in f.a.iter().zip(cpu.a).enumerate() {
        reg(format!("a{i}"), *e, g);
    }
    reg("usp".into(), f.usp, cpu.usp);
    reg("ssp".into(), f.ssp, cpu.ssp);
    reg("sr".into(), f.sr, cpu.sr as u32);
    reg("pc".into(), final_pc(f.pc, cpu.stopped), cpu.pc);
    for &(addr, exp) in &f.ram {
        let got = ram.word(addr);
        if got != exp {
            out.push(format!("ram[{addr:06x}] exp {exp:04x} got {got:04x}"));
        }
    }
    out
}

struct FileResult {
    name: String,
    cases: usize,
    /// Cases whose transactions hold an address-error cycle.
    ae_cases: usize,
    state_fail: usize,
    cycle_fail: usize,
    /// Failing cases that end in an address error, and those that do not.
    fail_ae: usize,
    fail_other: usize,
    /// First failing case and its differences, or the panic.
    first: Option<String>,
    /// `failOther` cases by kind (the set of fields that differ): count and the first example.
    kinds: BTreeMap<String, (usize, String)>,
    panicked: bool,
}

/// The kind of a failure: which fields differ (`d`, `a`, `usp`, `ssp`, `sr`, `pc`, `ram`, `cycles`), in that order.
fn kind_of(bad: &[String]) -> String {
    let mut kinds: Vec<&str> = Vec::new();
    for b in bad {
        let k = ["usp", "ssp", "sr", "pc", "ram", "cycles", "d", "a"].into_iter().find(|k| b.starts_with(k)).unwrap_or("?");
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    kinds.join("+")
}

fn run_file(name: &str, path: &Path, limit: usize) -> FileResult {
    let mut r = FileResult {
        name: name.to_string(),
        cases: 0,
        ae_cases: 0,
        state_fail: 0,
        cycle_fail: 0,
        fail_ae: 0,
        fail_other: 0,
        first: None,
        kinds: BTreeMap::new(),
        panicked: false,
    };
    let cases = match catch_unwind(|| load(path)) {
        Ok(c) => c,
        Err(_) => {
            r.panicked = true;
            r.first = Some("file does not decode".to_string());
            return r;
        }
    };
    let cases = &cases[..cases.len().min(limit)];
    r.cases = cases.len();
    r.ae_cases = cases.iter().filter(|c| c.addr_error).count();
    let mut ram = Ram::new();
    let current = AtomicUsize::new(0);
    let run = catch_unwind(AssertUnwindSafe(|| {
        for (i, case) in cases.iter().enumerate() {
            current.store(i, Ordering::Relaxed);
            let init = &case.initial;
            let start = init.pc.wrapping_sub(4);
            let mut cpu = Cpu::new();
            cpu.d = init.d;
            cpu.a = init.a;
            cpu.usp = init.usp;
            cpu.ssp = init.ssp;
            cpu.sr = init.sr as u16;
            cpu.pc = start;
            for &(addr, val) in &init.ram {
                ram.poke_word(addr, val);
            }
            for (k, word) in init.prefetch.iter().enumerate() {
                ram.write16(start.wrapping_add(2 * k as u32), *word as u16);
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
            if !bad.is_empty() {
                let line = format!("{}{}: {}", case.name, if case.addr_error { " (address error)" } else { "" }, bad.join(", "));
                if case.addr_error {
                    r.fail_ae += 1;
                } else {
                    r.fail_other += 1;
                    let e = r.kinds.entry(kind_of(&bad)).or_insert_with(|| (0, line.clone()));
                    e.0 += 1;
                }
                if r.first.is_none() {
                    r.first = Some(line);
                }
            }
            ram.clear();
        }
    }));
    if let Err(e) = run {
        let msg = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default();
        let at = &cases[current.load(Ordering::Relaxed)].name;
        r.panicked = true;
        r.first = Some(format!("panic at {at}: {msg}"));
    }
    r
}

#[test]
fn mame() {
    let Some(dir) = data_dir() else { return };
    let files = files(&dir);
    let limit = std::env::var("M68K_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let results = par_map(&files, |name, path| run_file(name, path, limit));
    std::panic::set_hook(hook);

    // failAE + failOther = cases failing in state or cycles; AE = cases whose transactions hold `re` / `we`.
    println!("{:<14} {:>6} {:>6} {:>7} {:>7} {:>7} {:>9}  first failure", "file", "cases", "AE", "state", "cycles", "failAE", "failOther");
    let mut failing = Vec::new();
    let (mut passed, mut panicked) = (0, 0);
    let mut tot = [0usize; 6];
    for r in &results {
        let ok = !r.panicked && r.state_fail == 0 && r.cycle_fail == 0;
        let known = KNOWN.iter().find(|(f, _)| r.name.split('.').next() == Some(*f));
        let first = r.first.as_deref().unwrap_or("");
        let note = known.map_or(String::new(), |(_, why)| format!(" [known: {why}]"));
        let p = if r.panicked { " PANIC" } else { "" };
        println!(
            "{:<14} {:>6} {:>6} {:>7} {:>7} {:>7} {:>9}{p}  {first}{note}",
            r.name, r.cases, r.ae_cases, r.state_fail, r.cycle_fail, r.fail_ae, r.fail_other
        );
        for (t, v) in tot.iter_mut().zip([r.cases, r.ae_cases, r.state_fail, r.cycle_fail, r.fail_ae, r.fail_other]) {
            *t += v;
        }
        if ok {
            passed += 1;
        } else if r.panicked {
            panicked += 1;
        }
        if !ok && known.is_none() {
            failing.push(r.name.as_str());
        }
    }
    let t = tot;
    println!("{:<14} {:>6} {:>6} {:>7} {:>7} {:>7} {:>9}", "total", t[0], t[1], t[2], t[3], t[4], t[5]);
    println!("failOther by kind (fields that differ), with the first example of each:");
    for r in &results {
        for (kind, (n, example)) in &r.kinds {
            println!("KIND {:<14} {n:>5}  {kind:<24} {example}", r.name);
        }
    }
    println!("{} files: {passed} pass, {panicked} panicked, {} with failures (known ones not counted)", results.len(), failing.len());
    assert!(failing.is_empty(), "failing files: {}", failing.join(" "));
}

/// One case as the line the script of `decoder_matches_reference` prints from `decode.py` output.
fn summary(c: &Case) -> String {
    let state = |s: &State| {
        let regs: Vec<String> = s.d.iter().chain(&s.a).chain([&s.usp, &s.ssp, &s.sr, &s.pc]).chain(&s.prefetch).map(|v| format!("{v:x}")).collect();
        // decode.py lists RAM as bytes: [addr, high], [addr | 1, low].
        let ram: Vec<String> = s.ram.iter().map(|(a, w)| format!("{:x}={:x},{:x}={:x}", a, w >> 8, a | 1, w & 0xFF)).collect();
        format!("{} ram {}", regs.join(" "), ram.join(","))
    };
    format!("{} | {} | {} | {}", c.name, state(&c.initial), state(&c.fin), c.length)
}

/// The decoder against `python3 decode.py` (the suite's own decoder) on `REF_FILE`: the case count and the first
/// and last case (name, every register, prefetch, RAM, length), printed by this script run next to decode.py:
///   import decode, json; decode.decode_file('v1/F.json.bin', 'F.json'); t = json.load(open('F.json'))
///   def st(s): return ' '.join('%x' % s[r] for r in decode.REG_ORDER) + ' ' + ' '.join('%x' % p for p in s['prefetch']) \
///       + ' ram ' + ','.join('%x=%x' % (a, v) for a, v in s['ram'])
///   for c in (t[0], t[-1]): print(c['name'], '|', st(c['initial']), '|', st(c['final']), '|', c['length'])
#[test]
fn decoder_matches_reference() {
    let Some(dir) = data_dir() else { return };
    let path = dir.join(format!("{REF_FILE}.json.bin"));
    if !path.is_file() {
        println!("{} missing: run scripts/fetch-m68k-tests.sh", path.display());
        return;
    }
    let cases = load(&path);
    assert_eq!(cases.len(), REF_COUNT);
    assert_eq!(summary(&cases[0]), REF_FIRST);
    assert_eq!(summary(cases.last().unwrap()), REF_LAST);
}

const REF_FILE: &str = "MOVE.w";
const REF_COUNT: usize = 2500;
const REF_FIRST: &str = concat!(
    "000 MOVE.w A1, (A2)+ 34c9 | ",
    "96d42168 47564f84 407d4266 e29ce361 ab8ce853 dbbb5fb7 4c210254 88023780 f60e9be0 40d3323d 21a7f4a eae45708 831a2b42 776e0ba6 ",
    "4360cbc 2b1016 ae1e52 307 277a2e 34c9 2501 ram 277a2a=34,277a2b=c9,277a2c=25,277a2d=1,277a2e=f9,277a2f=f | ",
    "96d42168 47564f84 407d4266 e29ce361 ab8ce853 dbbb5fb7 4c210254 88023780 f60e9be0 40d3323d 21a7f4c eae45708 831a2b42 776e0ba6 ",
    "4360cbc 2b1016 ae1e52 300 277a30 2501 f90f ram 277a2a=34,277a2b=c9,277a2c=25,277a2d=1,1a7f4a=32,1a7f4b=3d,277a2e=f9,277a2f=f | 8",
);
const REF_LAST: &str = concat!(
    "2499 MOVE.w -(A4), D4 3824 | ",
    "61564fa2 6655802d 239f9c09 5db3d0d4 15a2451 3d4fec2 5c8283c3 cd22d9ea 7fa8ef29 6920fe95 c9b07fcf 983735ba 4eb899b8 eba57725 ",
    "c8415e8d 66bc7e fd8142 8305 57bc22 3824 13cc ram 57bc1e=38,57bc1f=24,57bc20=13,57bc21=cc,b899b6=28,b899b7=a8,57bc22=b1,57bc23=78 | ",
    "61564fa2 6655802d 239f9c09 5db3d0d4 15a28a8 3d4fec2 5c8283c3 cd22d9ea 7fa8ef29 6920fe95 c9b07fcf 983735ba 4eb899b6 eba57725 ",
    "c8415e8d 66bc7e fd8142 8300 57bc24 13cc b178 ram 57bc1e=38,57bc1f=24,57bc20=13,57bc21=cc,b899b6=28,b899b7=a8,57bc22=b1,57bc23=78 | 10",
);
