//! Headless runner for debugging and ROM hacking.
//!
//! headless <rom> [--frames N] [--region ntsc|pal] [--four-score] [--no-sprite-limit] [--input FILE]
//!          [--png OUT.png] [--png-every N DIR] [--peek ADDR[,ADDR..]] [--ram-dump FILE]
//!          [--ram-dump-every N DIR] [--watch ADDR[,ADDR..]] [--trace FROM:TO FILE]
//!          [--read-watch ADDR|LO-HI[,..]] [--poke ADDR=VAL@FRAME[,..]] [--hash-log FILE] [--bench]
//!
//! Input file lines: `<frame> <p1> <p2> [<p3> <p4>]` (hex bytes, bits as `ebobo_core::pad`);
//! each line applies from that frame on. `#` starts a comment.
//!
//! `--trace` lines: `frame PC A X Y P SP [b=NN]`; `b=` is the PRG bank mapped at $8000-$BFFF
//! (hex, `??` if unknown) and is printed only while PC is in that window.
//! `--read-watch` prints `frame PC op addr R|W|RMW [b=NN]` for every data access to a watched
//! address (decoded from the opcode before it runs; stack and vector accesses are not reported).
//! `--poke` writes RAM ($0000-$1FFF) at the start of FRAME (hex ADDR/VAL, decimal FRAME).
//! `--hash-log` writes `frame state_hash` (decimal frame, 16 hex digits) after every frame.

use ebobo_core::{CpuState, Nes, Region, HEIGHT, WIDTH};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::exit;
use std::time::Instant;

fn die(msg: &str) -> ! {
    eprintln!("headless: {msg}");
    exit(2)
}

fn hex16(s: &str) -> u16 {
    let s = s.trim_start_matches('$').trim_start_matches("0x");
    u16::from_str_radix(s, 16).unwrap_or_else(|_| die(&format!("bad address {s}")))
}

fn addr_list(s: &str) -> Vec<u16> {
    s.split(',').filter(|x| !x.is_empty()).map(hex16).collect()
}

fn hex8(s: &str) -> u8 {
    let s = s.trim_start_matches('$').trim_start_matches("0x");
    u8::from_str_radix(s, 16).unwrap_or_else(|_| die(&format!("bad byte {s}")))
}

fn range_list(s: &str) -> Vec<(u16, u16)> {
    s.split(',')
        .filter(|x| !x.is_empty())
        .map(|x| match x.split_once('-') {
            Some((lo, hi)) => (hex16(lo), hex16(hi)),
            None => (hex16(x), hex16(x)),
        })
        .collect()
}

fn poke_list(s: &str) -> Vec<(u32, u16, u8)> {
    s.split(',')
        .filter(|x| !x.is_empty())
        .map(|x| {
            let (av, f) = x.split_once('@').unwrap_or_else(|| die("--poke ADDR=VAL@FRAME"));
            let (a, v) = av.split_once('=').unwrap_or_else(|| die("--poke ADDR=VAL@FRAME"));
            let frame = f.parse().unwrap_or_else(|_| die(&format!("bad poke frame {f}")));
            (frame, hex16(a), hex8(v))
        })
        .collect()
}

// ---- 6502 data-access decoding ----

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    None,
    Zp,
    ZpX,
    ZpY,
    Abs,
    AbsX,
    AbsY,
    IndX,
    IndY,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    R,
    W,
    Rmw,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::R => "R",
            Kind::W => "W",
            Kind::Rmw => "RMW",
        }
    }
}

/// Data-access addressing mode and kind of an opcode (official and unofficial), decoded from
/// the `aaabbbcc` layout. Immediate, implied, branches, jumps and stack ops give `Mode::None`.
fn decode(op: u8) -> (Mode, Kind) {
    let (aaa, bbb, cc) = (op >> 5, (op >> 2) & 7, op & 3);
    let x_to_y = aaa == 4 || aaa == 5; // STX/LDX/SAX/LAX index with Y
    match cc {
        1 | 3 => {
            let mode = match bbb {
                0 => Mode::IndX,
                1 => Mode::Zp,
                2 => Mode::None,
                3 => Mode::Abs,
                4 => Mode::IndY,
                5 if cc == 3 && x_to_y => Mode::ZpY, // SAX/LAX zp,Y
                5 => Mode::ZpX,
                6 => Mode::AbsY,
                _ if cc == 3 && x_to_y => Mode::AbsY, // SHA/LAX abs,Y
                _ => Mode::AbsX,
            };
            let kind = match (cc, aaa) {
                (_, 4) => Kind::W,
                (_, 5) => Kind::R,
                (1, _) => Kind::R,
                _ => Kind::Rmw,
            };
            (mode, kind)
        }
        2 => {
            let mode = match bbb {
                1 => Mode::Zp,
                3 => Mode::Abs,
                5 if x_to_y => Mode::ZpY,
                5 => Mode::ZpX,
                7 if x_to_y => Mode::AbsY,
                7 => Mode::AbsX,
                _ => Mode::None,
            };
            let kind = match aaa {
                4 => Kind::W,
                5 => Kind::R,
                _ => Kind::Rmw,
            };
            (mode, kind)
        }
        _ => {
            let mode = match (bbb, aaa) {
                (1, _) => Mode::Zp,              // BIT, STY, LDY, CPY, CPX, NOP zp
                (3, 0 | 1 | 4..=7) => Mode::Abs, // NOP/BIT/STY/LDY/CPY/CPX abs (2,3: JMP)
                (5, _) => Mode::ZpX,             // STY/LDY zp,X and NOP zp,X
                (7, _) => Mode::AbsX,            // LDY/SHY abs,X and NOP abs,X
                _ => Mode::None,
            };
            (mode, if aaa == 4 { Kind::W } else { Kind::R })
        }
    }
}

/// Effective data address of the instruction at the CPU's PC, if it accesses memory.
fn effective(nes: &Nes, cpu: &CpuState) -> Option<(u8, u16, Kind)> {
    let op = nes.peek(cpu.pc);
    let (mode, kind) = decode(op);
    let b1 = nes.peek(cpu.pc.wrapping_add(1));
    let w = || u16::from_le_bytes([b1, nes.peek(cpu.pc.wrapping_add(2))]);
    let zp16 = |p: u8| u16::from_le_bytes([nes.peek(p as u16), nes.peek(p.wrapping_add(1) as u16)]);
    let addr = match mode {
        Mode::None => return None,
        Mode::Zp => b1 as u16,
        Mode::ZpX => b1.wrapping_add(cpu.x) as u16,
        Mode::ZpY => b1.wrapping_add(cpu.y) as u16,
        Mode::Abs => w(),
        Mode::AbsX => w().wrapping_add(cpu.x as u16),
        Mode::AbsY => w().wrapping_add(cpu.y as u16),
        Mode::IndX => zp16(b1.wrapping_add(cpu.x)),
        Mode::IndY => zp16(b1).wrapping_add(cpu.y as u16),
    };
    Some((op, addr, kind))
}

// ---- MMC1 PRG bank tracking ----

/// Follows MMC1 register writes seen in the stepped instruction stream and, as a check and
/// fallback, matches the bytes at PC against the ROM's 16 KiB PRG banks.
struct Banks {
    prg: Vec<u8>,
    shift: u8,
    count: u8,
    control: u8,
    prg_reg: u8,
    known: bool,
}

impl Banks {
    fn new(rom: &[u8]) -> Banks {
        let prg = if rom.len() >= 16 && &rom[0..4] == b"NES\x1A" {
            let start = 16 + if rom[6] & 4 != 0 { 512 } else { 0 };
            let end = (start + rom[4] as usize * 0x4000).min(rom.len());
            rom[start.min(end)..end].to_vec()
        } else {
            Vec::new()
        };
        Banks { prg, shift: 0, count: 0, control: 0x0C, prg_reg: 0, known: false }
    }

    /// Feeds the instruction about to run (MMC1 ignores the second write of RMW opcodes, so
    /// only the first one, the unmodified value, is applied).
    fn observe(&mut self, acc: Option<(u8, u16, Kind)>, cpu: &CpuState, nes: &Nes) {
        let Some((op, addr, kind)) = acc else { return };
        if addr < 0x8000 || kind == Kind::R {
            return;
        }
        let val = match (kind, op) {
            (Kind::Rmw, _) => nes.peek(addr),
            (_, 0x81 | 0x85 | 0x8D | 0x91 | 0x95 | 0x99 | 0x9D) => cpu.a,
            (_, 0x86 | 0x8E | 0x96) => cpu.x,
            (_, 0x84 | 0x8C | 0x94) => cpu.y,
            (_, 0x83 | 0x87 | 0x8F | 0x97) => cpu.a & cpu.x,
            _ => return, // unstable SHA/SHX/SHY/TAS stores
        };
        if val & 0x80 != 0 {
            self.shift = 0;
            self.count = 0;
            self.control |= 0x0C;
            return;
        }
        self.shift |= (val & 1) << self.count;
        self.count += 1;
        if self.count == 5 {
            match addr & 0xE000 {
                0x8000 => self.control = self.shift,
                0xE000 => {
                    self.prg_reg = self.shift & 0x0F;
                    self.known = true;
                }
                _ => {}
            }
            self.shift = 0;
            self.count = 0;
        }
    }

    fn matches(&self, bank: usize, pc: u16, nes: &Nes) -> bool {
        let off = bank * 0x4000 + (pc as usize & 0x3FFF);
        (0..4u16).all(|i| {
            let o = off + i as usize;
            o >= bank * 0x4000 + 0x4000 || self.prg.get(o) == Some(&nes.peek(pc.wrapping_add(i)))
        })
    }

    /// PRG bank at $8000-$BFFF for `pc` in that window.
    fn bank(&self, pc: u16, nes: &Nes) -> Option<usize> {
        let n = self.prg.len() / 0x4000;
        if n == 0 {
            return None;
        }
        if self.known {
            let b = match (self.control >> 2) & 3 {
                0 | 1 => (self.prg_reg & 0x0E) as usize,
                2 => 0,
                _ => self.prg_reg as usize,
            } % n;
            if self.matches(b, pc, nes) {
                return Some(b);
            }
        }
        let mut found = (0..n).filter(|&b| self.matches(b, pc, nes));
        match (found.next(), found.next()) {
            (Some(b), None) => Some(b),
            _ => None,
        }
    }

    fn suffix(&self, pc: u16, nes: &Nes) -> String {
        if !(0x8000..0xC000).contains(&pc) {
            return String::new();
        }
        match self.bank(pc, nes) {
            Some(b) => format!(" b={b:02X}"),
            None => " b=??".to_string(),
        }
    }
}

fn parse_input(path: &str) -> Vec<(u32, [u8; 4])> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 || f.len() > 5 {
            die(&format!("{path}:{}: expected `<frame> <p1> <p2> [<p3> <p4>]`", n + 1));
        }
        let frame = f[0].parse().unwrap_or_else(|_| die(&format!("{path}:{}: bad frame", n + 1)));
        let mut pads = [0u8; 4];
        for (i, v) in f[1..].iter().enumerate() {
            pads[i] = u8::from_str_radix(v.trim_start_matches("0x"), 16)
                .unwrap_or_else(|_| die(&format!("{path}:{}: bad pad byte {v}", n + 1)));
        }
        out.push((frame, pads));
    }
    out.sort_by_key(|e| e.0);
    out
}

// ---- minimal PNG encoder (stored deflate blocks) ----

fn crc32(data: &[&[u8]]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    let mut crc = !0u32;
    for part in data {
        for &b in *part {
            crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

fn write_png(path: &str, rgba: &[u8]) {
    let mut raw = Vec::with_capacity(HEIGHT * (1 + WIDTH * 3));
    for row in rgba.chunks(WIDTH * 4) {
        raw.push(0); // filter: none
        for px in row.chunks(4) {
            raw.extend_from_slice(&px[..3]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
    for (i, blk) in blocks.iter().enumerate() {
        z.push((i + 1 == blocks.len()) as u8);
        let len = blk.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(blk);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(WIDTH as u32).to_be_bytes());
    ihdr.extend_from_slice(&(HEIGHT as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    std::fs::write(path, png).unwrap_or_else(|e| die(&format!("{path}: {e}")));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        eprintln!(
            "usage: headless <rom> [--frames N] [--region ntsc|pal] [--four-score] [--no-sprite-limit] [--input FILE] \
             [--png OUT.png] [--png-every N DIR] [--peek ADDR[,ADDR..]] [--ram-dump FILE] \
             [--ram-dump-every N DIR] [--watch ADDR[,ADDR..]] [--trace FROM:TO FILE] \
             [--read-watch ADDR|LO-HI[,..]] [--poke ADDR=VAL@FRAME[,..]] [--hash-log FILE] [--bench]"
        );
        exit(2);
    }
    let rom_path = &args[0];
    let mut frames: u32 = 600;
    let mut region = None;
    let mut four_score = false;
    let mut no_sprite_limit = false;
    let mut inputs = Vec::new();
    let mut png = None;
    let mut png_every: Option<(u32, String)> = None;
    let mut peeks = Vec::new();
    let mut ram_dump = None;
    let mut ram_every: Option<(u32, String)> = None;
    let mut watch = Vec::new();
    let mut trace: Option<(u32, u32, String)> = None;
    let mut bench = false;
    let mut read_watch: Vec<(u16, u16)> = Vec::new();
    let mut pokes: Vec<(u32, u16, u8)> = Vec::new();
    let mut hash_log: Option<String> = None;

    let mut i = 1;
    let next = |i: &mut usize| -> String {
        *i += 1;
        args.get(*i).cloned().unwrap_or_else(|| die(&format!("{} needs a value", args[*i - 1])))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--frames" => frames = next(&mut i).parse().unwrap_or_else(|_| die("bad --frames")),
            "--region" => {
                region = Some(match next(&mut i).as_str() {
                    "ntsc" => Region::Ntsc,
                    "pal" => Region::Pal,
                    r => die(&format!("bad region {r}")),
                })
            }
            "--four-score" => four_score = true,
            "--no-sprite-limit" => no_sprite_limit = true,
            "--input" => inputs = parse_input(&next(&mut i)),
            "--png" => png = Some(next(&mut i)),
            "--png-every" => {
                let n = next(&mut i).parse().unwrap_or_else(|_| die("bad --png-every N"));
                if n == 0 {
                    die("--png-every N must be > 0");
                }
                png_every = Some((n, next(&mut i)));
            }
            "--peek" => peeks = addr_list(&next(&mut i)),
            "--ram-dump" => ram_dump = Some(next(&mut i)),
            "--ram-dump-every" => {
                let n = next(&mut i).parse().unwrap_or_else(|_| die("bad --ram-dump-every N"));
                if n == 0 {
                    die("--ram-dump-every N must be > 0");
                }
                ram_every = Some((n, next(&mut i)));
            }
            "--watch" => watch = addr_list(&next(&mut i)),
            "--trace" => {
                let range = next(&mut i);
                let (a, b) = range.split_once(':').unwrap_or_else(|| die("--trace FROM:TO FILE"));
                let from = a.parse().unwrap_or_else(|_| die("bad trace FROM"));
                let to = b.parse().unwrap_or_else(|_| die("bad trace TO"));
                trace = Some((from, to, next(&mut i)));
            }
            "--read-watch" => read_watch = range_list(&next(&mut i)),
            "--poke" => pokes.extend(poke_list(&next(&mut i))),
            "--hash-log" => hash_log = Some(next(&mut i)),
            "--bench" => bench = true,
            a => die(&format!("unknown argument {a}")),
        }
        i += 1;
    }

    let rom = std::fs::read(rom_path).unwrap_or_else(|e| die(&format!("{rom_path}: {e}")));
    let mut nes = Nes::new(&rom, region).unwrap_or_else(|e| die(&format!("{rom_path}: {e:?}")));
    if four_score {
        nes.set_four_score(true);
    }
    if no_sprite_limit {
        nes.set_sprite_limit(false);
    }
    if let Some((_, dir)) = &png_every {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| die(&format!("{dir}: {e}")));
    }
    let mut trace_out = trace.as_ref().map(|(from, to, path)| {
        let f = File::create(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        (*from, *to, BufWriter::new(f))
    });
    let mut hash_out = hash_log.as_ref().map(|path| {
        BufWriter::new(File::create(path).unwrap_or_else(|e| die(&format!("{path}: {e}"))))
    });

    let step_mode = !watch.is_empty() || trace_out.is_some() || !read_watch.is_empty();
    let mut banks = Banks::new(&rom);
    pokes.sort_by_key(|p| p.0);
    let mut next_poke = 0;
    let mut watched: Vec<u8> = watch.iter().map(|&a| nes.peek(a)).collect();
    let mut rgba = vec![0u8; WIDTH * HEIGHT * 4];
    let mut pads = [0u8; 4];
    let mut next_input = 0;
    let start = Instant::now();

    for frame in 0..frames {
        while next_input < inputs.len() && inputs[next_input].0 <= frame {
            pads = inputs[next_input].1;
            next_input += 1;
        }
        while next_poke < pokes.len() && pokes[next_poke].0 <= frame {
            let (_, a, v) = pokes[next_poke];
            if pokes[next_poke].0 == frame {
                nes.poke_ram(a, v);
            }
            next_poke += 1;
        }
        if step_mode {
            loop {
                let cpu = nes.cpu_state();
                let acc = effective(&nes, &cpu);
                if let Some((op, addr, kind)) = acc {
                    if read_watch.iter().any(|&(lo, hi)| (lo..=hi).contains(&addr)) {
                        println!(
                            "{frame} {:04X} {op:02X} {addr:04X} {}{}",
                            cpu.pc,
                            kind.name(),
                            banks.suffix(cpu.pc, &nes)
                        );
                    }
                }
                if let Some((from, to, out)) = trace_out.as_mut() {
                    if frame >= *from && frame <= *to {
                        writeln!(
                            out,
                            "{frame} {:04X} {:02X} {:02X} {:02X} {:02X} {:02X}{}",
                            cpu.pc,
                            cpu.a,
                            cpu.x,
                            cpu.y,
                            cpu.p,
                            cpu.sp,
                            banks.suffix(cpu.pc, &nes)
                        )
                        .unwrap();
                    }
                }
                banks.observe(acc, &cpu, &nes);
                let done = nes.step_instruction(pads);
                for (k, &a) in watch.iter().enumerate() {
                    let v = nes.peek(a);
                    if v != watched[k] {
                        println!(
                            "frame={frame} pc={:04X} addr={a:04X} {:02X}->{v:02X}",
                            cpu.pc, watched[k]
                        );
                        watched[k] = v;
                    }
                }
                if done {
                    break;
                }
            }
        } else {
            nes.run_frame(pads);
        }
        if let Some(out) = hash_out.as_mut() {
            writeln!(out, "{frame} {:016x}", nes.state_hash())
                .unwrap_or_else(|e| die(&format!("hash log: {e}")));
        }
        if let Some((n, dir)) = &png_every {
            if (frame + 1) % n == 0 {
                nes.frame_rgba(&mut rgba);
                write_png(&format!("{dir}/frame_{}.png", frame + 1), &rgba);
            }
        }
        if let Some((n, dir)) = &ram_every {
            if (frame + 1) % n == 0 {
                let ram: Vec<u8> = (0x0000u16..=0x07FF).map(|a| nes.peek(a)).collect();
                let path = format!("{dir}/ram_{}.bin", frame + 1);
                std::fs::write(&path, ram).unwrap_or_else(|e| die(&format!("{path}: {e}")));
            }
        }
    }
    let elapsed = start.elapsed();

    if let Some((_, _, mut out)) = trace_out {
        out.flush().unwrap();
    }
    if let Some(mut out) = hash_out {
        out.flush().unwrap_or_else(|e| die(&format!("hash log: {e}")));
    }
    if let Some(path) = &png {
        nes.frame_rgba(&mut rgba);
        write_png(path, &rgba);
    }
    if !peeks.is_empty() {
        let parts: Vec<String> =
            peeks.iter().map(|&a| format!("{a:04X}={:02X}", nes.peek(a))).collect();
        println!("peek {}", parts.join(" "));
    }
    if let Some(path) = &ram_dump {
        let ram: Vec<u8> = (0x0000u16..=0x07FF).map(|a| nes.peek(a)).collect();
        std::fs::write(path, ram).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        let prg: Vec<u8> = (0x6000u16..=0x7FFF).map(|a| nes.peek(a)).collect();
        if prg.iter().any(|&b| b != 0) {
            let prg_path = format!("{path}.prg.bin");
            std::fs::write(&prg_path, prg).unwrap_or_else(|e| die(&format!("{prg_path}: {e}")));
        }
    }
    println!("frames={frames} state_hash={:016x}", nes.state_hash());
    if bench {
        let ms = elapsed.as_secs_f64() * 1000.0 / frames.max(1) as f64;
        println!("bench: {ms:.3} ms/frame ({:.1} fps)", 1000.0 / ms);
    }
}
