//! Headless runner of the Mega Drive core, the twin of `tools/md-oracle/oracle.py run`: same
//! flags, same file names and line formats, so outputs can be compared file by file.
//!
//! headless <rom> --frames N [--input FILE] [--png-every K DIR] [--png-at F,F,... DIR] [--wav FILE]
//!          [--hash-every K] [--region ntsc-u|pal|ntsc-j] [--multitap] [--state-hash-every K] [--bench]
//!
//! Frames count from 1; PNGs and hashes of frame F are taken after the F-th `run_frame`.
//! Input file lines: `<frame> <p1> <p2> [<p3> <p4>]` (hex masks, bits as `ebobo_md::pad`); each
//! line applies from that frame on. `#` starts a comment.
//! `--png-every` / `--png-at` write `DIR/frame_%06d.png` at the size the core gave (8-bit RGB).
//! Each size change is printed as `frame N size WxH`.
//! `--hash-every K` prints `frame N crc32 XXXXXXXX WxH`: CRC-32 of the picture as 8-bit R, G, B,
//! rows top to bottom (the bytes of the PNG, as the oracle hashes them).
//! `--state-hash-every K` prints `frame N state XXXXXXXXXXXXXXXX` (`Md::state_hash`).
//! `--wav` writes 48 kHz stereo 16-bit audio.
//! `--bench` only measures: the frames with fast-forward off, then again from power-on with it
//! on; frames per second of wall time for each. No files are written.

use ebobo_md::{Md, Region};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::process::exit;
use std::time::Instant;

const SAMPLE_RATE: u32 = 48_000;
const USAGE: &str = "usage: headless <rom> --frames N [--input FILE] [--png-every K DIR] [--png-at F,F,... DIR] [--wav FILE] \
                     [--hash-every K] [--region ntsc-u|pal|ntsc-j] [--multitap] [--state-hash-every K] [--bench]";

fn die(msg: &str) -> ! {
    eprintln!("headless: {msg}");
    exit(2)
}

fn parse_input(path: &str) -> Vec<(u32, [u16; 4])> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 3 && f.len() != 5 {
            die(&format!("{path}:{}: expected `<frame> <p1> <p2> [<p3> <p4>]`", n + 1));
        }
        let frame = f[0].parse().unwrap_or_else(|_| die(&format!("{path}:{}: bad frame", n + 1)));
        let mut pads = [0u16; 4];
        for (pad, v) in pads.iter_mut().zip(&f[1..]) {
            *pad = u16::from_str_radix(v.trim_start_matches("0x"), 16)
                .unwrap_or_else(|_| die(&format!("{path}:{}: bad pad mask {v}", n + 1)));
        }
        out.push((frame, pads));
    }
    out.sort_by_key(|e| e.0);
    out
}

// ---- minimal PNG encoder (stored deflate blocks) ----

/// CRC-32 (IEEE, reflected, polynomial 0xEDB88320) lookup table.
fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    table
}

fn crc32(table: &[u32; 256], data: &[&[u8]]) -> u32 {
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

/// Writes `rgb` (`w * h * 3` bytes, rows top to bottom) as an 8-bit RGB PNG.
fn write_png(table: &[u32; 256], path: &str, rgb: &[u8], w: usize, h: usize) {
    let mut raw = Vec::with_capacity(h * (1 + w * 3));
    if w > 0 {
        for row in rgb.chunks(w * 3) {
            raw.push(0); // filter: none
            raw.extend_from_slice(row);
        }
    }
    let mut z = vec![0x78, 0x01];
    if raw.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    let blocks = raw.len().div_ceil(65535);
    for (i, blk) in raw.chunks(65535).enumerate() {
        z.push((i + 1 == blocks) as u8);
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
        png.extend_from_slice(&crc32(table, &[kind, data]).to_be_bytes());
    };
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    std::fs::write(path, png).unwrap_or_else(|e| die(&format!("{path}: {e}")));
}

// ---- WAV ----

/// 16-bit stereo PCM file; the two sizes in the header are filled in by `finish`.
struct Wav {
    out: BufWriter<File>,
    path: String,
    bytes: u32,
}

impl Wav {
    fn create(path: &str) -> Wav {
        let mut out = BufWriter::new(File::create(path).unwrap_or_else(|e| die(&format!("{path}: {e}"))));
        let mut header = Vec::with_capacity(44);
        header.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes()); // PCM
        header.extend_from_slice(&2u16.to_le_bytes()); // channels
        header.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        header.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes()); // bytes per second
        header.extend_from_slice(&4u16.to_le_bytes()); // bytes per sample frame
        header.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        header.extend_from_slice(b"data\0\0\0\0");
        out.write_all(&header).unwrap_or_else(|e| die(&format!("{path}: {e}")));
        Wav { out, path: path.to_string(), bytes: 0 }
    }

    /// Appends interleaved stereo samples in -1.0..1.0 (clipped outside it).
    fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            self.out.write_all(&v.to_le_bytes()).unwrap_or_else(|e| die(&format!("{}: {e}", self.path)));
        }
        self.bytes = self.bytes.saturating_add(samples.len() as u32 * 2);
    }

    fn finish(mut self) {
        let fail = |e: std::io::Error| -> ! { die(&format!("{}: {e}", self.path)) };
        self.out.seek(SeekFrom::Start(4)).unwrap_or_else(|e| fail(e));
        self.out.write_all(&(36 + self.bytes).to_le_bytes()).unwrap_or_else(|e| fail(e));
        self.out.seek(SeekFrom::Start(40)).unwrap_or_else(|e| fail(e));
        self.out.write_all(&self.bytes.to_le_bytes()).unwrap_or_else(|e| fail(e));
        self.out.flush().unwrap_or_else(|e| fail(e));
    }
}

fn region_name(region: Region) -> &'static str {
    match region {
        Region::Americas => "ntsc-u",
        Region::Europe => "pal",
        Region::Japan => "ntsc-j",
    }
}

/// Pads in force at `frame` (1-based): the last input line at or before it.
fn advance_input(inputs: &[(u32, [u16; 4])], next: &mut usize, frame: u32, pads: &mut [u16; 4]) {
    while *next < inputs.len() && inputs[*next].0 <= frame {
        *pads = inputs[*next].1;
        *next += 1;
    }
}

/// Runs `frames` frames from power-on and returns frames per second of wall time.
fn bench_pass(rom: &[u8], region: Option<Region>, multitap: bool, inputs: &[(u32, [u16; 4])], frames: u32, fast: bool) -> f64 {
    let mut md = Md::new(rom, region).unwrap_or_else(|e| die(&format!("{e}")));
    md.set_multitap(multitap);
    md.set_sample_rate(SAMPLE_RATE as f64);
    md.set_fast_forward(fast);
    let mut pads = [0u16; 4];
    let mut next_input = 0;
    let mut audio = Vec::new();
    let start = Instant::now();
    for frame in 1..=frames {
        advance_input(inputs, &mut next_input, frame, &mut pads);
        md.run_frame(pads);
        audio.clear();
        md.drain_audio(&mut audio);
    }
    frames as f64 / start.elapsed().as_secs_f64().max(1e-9)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        eprintln!("{USAGE}");
        exit(2);
    }
    let rom_path = &args[0];
    let mut frames: Option<u32> = None;
    let mut region = None;
    let mut multitap = false;
    let mut inputs = Vec::new();
    let mut png_every: Option<(u32, String)> = None;
    let mut png_at: Option<(Vec<u32>, String)> = None;
    let mut wav_path: Option<String> = None;
    let mut hash_every: Option<u32> = None;
    let mut state_hash_every: Option<u32> = None;
    let mut bench = false;

    let mut i = 1;
    let next = |i: &mut usize| -> String {
        *i += 1;
        args.get(*i).cloned().unwrap_or_else(|| die(&format!("{} needs a value", args[*i - 1])))
    };
    let positive = |name: &str, v: String| -> u32 {
        match v.parse() {
            Ok(n) if n > 0 => n,
            _ => die(&format!("{name} needs a number above 0, got {v}")),
        }
    };
    while i < args.len() {
        match args[i].as_str() {
            "--frames" => frames = Some(next(&mut i).parse().unwrap_or_else(|_| die("bad --frames"))),
            "--region" => {
                region = Some(match next(&mut i).as_str() {
                    "ntsc-u" => Region::Americas,
                    "pal" => Region::Europe,
                    "ntsc-j" => Region::Japan,
                    r => die(&format!("bad region {r} (ntsc-u, pal or ntsc-j)")),
                })
            }
            "--multitap" => multitap = true,
            "--input" => inputs = parse_input(&next(&mut i)),
            "--png-every" => {
                let k = positive("--png-every", next(&mut i));
                png_every = Some((k, next(&mut i)));
            }
            "--png-at" => {
                let list = next(&mut i);
                let at = list
                    .split(',')
                    .filter(|x| !x.is_empty())
                    .map(|x| x.parse().unwrap_or_else(|_| die(&format!("bad --png-at frame {x}"))))
                    .collect();
                png_at = Some((at, next(&mut i)));
            }
            "--wav" => wav_path = Some(next(&mut i)),
            "--hash-every" => hash_every = Some(positive("--hash-every", next(&mut i))),
            "--state-hash-every" => state_hash_every = Some(positive("--state-hash-every", next(&mut i))),
            "--bench" => bench = true,
            a => die(&format!("unknown argument {a}\n{USAGE}")),
        }
        i += 1;
    }
    let frames = frames.unwrap_or_else(|| die(&format!("--frames N is required\n{USAGE}")));

    let rom = std::fs::read(rom_path).unwrap_or_else(|e| die(&format!("{rom_path}: {e}")));
    let mut md = Md::new(&rom, region).unwrap_or_else(|e| die(&format!("{rom_path}: {e}")));
    md.set_multitap(multitap);
    md.set_sample_rate(SAMPLE_RATE as f64);
    println!(
        "title \"{}\" product \"{}\" crc32 {:08X} region {} fps {:.4} sample_rate {SAMPLE_RATE}",
        md.info().title,
        md.info().product,
        md.info().crc32,
        region_name(md.region()),
        md.frame_rate()
    );

    if bench {
        let fps = md.frame_rate();
        for (fast, label) in [(false, "off"), (true, "on")] {
            let got = bench_pass(&rom, region, multitap, &inputs, frames, fast);
            println!("bench fast-forward {label}: {got:.1} fps ({:.2}x real time)", got / fps);
        }
        return;
    }

    for dir in png_every.iter().map(|p| &p.1).chain(png_at.iter().map(|p| &p.1)) {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| die(&format!("{dir}: {e}")));
    }
    let mut wav = wav_path.as_deref().map(Wav::create);
    let table = crc_table();
    let mut rgba = Vec::new();
    let mut rgb = Vec::new();
    let mut audio = Vec::new();
    let mut pads = [0u16; 4];
    let mut next_input = 0;
    let mut size = (0usize, 0usize);

    for frame in 1..=frames {
        advance_input(&inputs, &mut next_input, frame, &mut pads);
        md.run_frame(pads);

        audio.clear();
        md.drain_audio(&mut audio);
        if let Some(wav) = wav.as_mut() {
            wav.push(&audio);
        }

        let now = md.frame_size();
        if now != size {
            size = now;
            println!("frame {frame} size {}x{}", size.0, size.1);
        }
        let every_dir = png_every.as_ref().filter(|(k, _)| frame % k == 0).map(|(_, dir)| dir);
        let at_dir = png_at.as_ref().filter(|(at, _)| at.contains(&frame)).map(|(_, dir)| dir);
        let hash = hash_every.is_some_and(|k| frame % k == 0);
        if every_dir.is_some() || at_dir.is_some() || hash {
            let (w, h) = md.frame_rgba(&mut rgba);
            rgb.clear();
            rgb.extend(rgba.chunks_exact(4).flat_map(|px| [px[0], px[1], px[2]]));
            if let Some(dir) = every_dir {
                write_png(&table, &format!("{dir}/frame_{frame:06}.png"), &rgb, w, h);
            }
            if let Some(dir) = at_dir.filter(|dir| Some(*dir) != every_dir) {
                write_png(&table, &format!("{dir}/frame_{frame:06}.png"), &rgb, w, h);
            }
            if hash {
                println!("frame {frame} crc32 {:08X} {w}x{h}", crc32(&table, &[&rgb]));
            }
        }
        if state_hash_every.is_some_and(|k| frame % k == 0) {
            println!("frame {frame} state {:016X}", md.state_hash());
        }
    }
    if let Some(wav) = wav {
        wav.finish();
    }
}
