//! Memory maps of both CPUs, scheduler and the public machine. Owner: system slice.

use std::fmt;
use std::hash::{Hash, Hasher};

use bincode::Options;

use crate::bus::{Machine, State};
use crate::cart::{Cart, RomError, RomInfo};
use crate::io::Multitap;
use crate::{MAX_W, Region};

/// Master clocks per line, on both video standards.
const LINE_MCLK: u64 = 3420;
/// Lines per frame.
const LINES_NTSC: u64 = 262;
const LINES_PAL: u64 = 313;
/// Master clock in Hz.
const MCLK_NTSC_HZ: f64 = 53_693_175.0;
const MCLK_PAL_HZ: f64 = 53_203_424.0;

/// Save state layout: magic, format version (u16 LE), ROM CRC-32 (u32 LE), region byte, then
/// the state in bincode.
const STATE_MAGIC: &[u8; 4] = b"EBMD";
const STATE_VERSION: u16 = 1;
const STATE_HEADER_LEN: usize = 4 + 2 + 4 + 1;
/// No length inside a save state may claim more than this many bytes.
const STATE_LIMIT: u64 = 16 << 20;

/// A Mega Drive with a cartridge in it.
pub struct Md {
    m: Machine,
}

/// Complete emulated state. Cloning copies about 200 KB (the ROM is shared; the picture and
/// the audio output are not part of it). Rollback saves one of these every frame.
#[derive(Clone)]
pub struct Snapshot {
    state: State,
}

/// Why [`Md::load_state`] refused a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateError {
    /// Not an ebobo Mega Drive save state.
    BadMagic,
    /// Saved by an incompatible state format version.
    Version(u16),
    /// Saved from another ROM or region.
    WrongRom,
    /// Truncated or damaged.
    Corrupt,
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StateError::BadMagic => write!(f, "not a save state"),
            StateError::Version(v) => write!(f, "save state version {v} is not supported"),
            StateError::WrongRom => write!(f, "the save state is from another game"),
            StateError::Corrupt => write!(f, "the save state is damaged"),
        }
    }
}

impl std::error::Error for StateError {}

/// Hasher of the emulated state: one multiply and one rotate per 64-bit word, fixed constants,
/// so the result is the same on every machine (`std`'s default hasher promises no such thing).
/// Integers are hashed by value and `usize` as `u64`; byte runs are taken as little-endian
/// 8-byte words. Slices of wider integers reach `write` in the host's byte order, which is
/// little-endian on every platform ebobo runs on.
struct StateHasher {
    h: u64,
}

impl StateHasher {
    const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
    const MUL: u64 = 0xD6E8_FEB8_6659_FD93;

    fn new() -> StateHasher {
        StateHasher { h: Self::SEED }
    }

    #[inline]
    fn mix(&mut self, word: u64) {
        self.h = (self.h.rotate_left(23) ^ word).wrapping_mul(Self::MUL);
    }
}

impl Hasher for StateHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut words = bytes.chunks_exact(8);
        for word in &mut words {
            self.mix(u64::from_le_bytes([word[0], word[1], word[2], word[3], word[4], word[5], word[6], word[7]]));
        }
        let tail = words.remainder();
        if !tail.is_empty() {
            // Up to 7 bytes in the low 56 bits, their count in the top byte.
            let mut word = [0u8; 8];
            word[..tail.len()].copy_from_slice(tail);
            word[7] = tail.len() as u8;
            self.mix(u64::from_le_bytes(word));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.mix(u64::from(n));
    }

    fn write_u16(&mut self, n: u16) {
        self.mix(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.mix(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.mix(n);
    }

    fn write_u128(&mut self, n: u128) {
        self.mix(n as u64);
        self.mix((n >> 64) as u64);
    }

    fn write_usize(&mut self, n: usize) {
        self.mix(n as u64);
    }

    fn finish(&self) -> u64 {
        let mut h = self.h;
        h ^= h >> 32;
        h = h.wrapping_mul(Self::MUL);
        h ^= h >> 29;
        h = h.wrapping_mul(Self::SEED);
        h ^ (h >> 32)
    }
}

fn state_options() -> impl Options {
    bincode::DefaultOptions::new().with_fixint_encoding().with_little_endian().with_limit(STATE_LIMIT)
}

fn region_byte(region: Region) -> u8 {
    match region {
        Region::Japan => 0,
        Region::Americas => 1,
        Region::Europe => 2,
    }
}

impl Md {
    /// Reads `rom` (a plain or interleaved image) and powers the console on.
    /// `region` overrides the header's choice (`None` = `RomInfo::region`).
    pub fn new(rom: &[u8], region: Option<Region>) -> Result<Md, RomError> {
        let cart = Cart::new(rom, region)?;
        Ok(Md { m: Machine::new(cart) })
    }

    pub fn info(&self) -> &RomInfo {
        self.m.cart.info()
    }

    /// The region the console runs as.
    pub fn region(&self) -> Region {
        self.m.cart.info().region
    }

    /// Frames per second of the emulated console: 59.9228 (NTSC) or 49.7015 (PAL).
    pub fn frame_rate(&self) -> f64 {
        match self.region() {
            Region::Japan | Region::Americas => MCLK_NTSC_HZ / (LINE_MCLK * LINES_NTSC) as f64,
            Region::Europe => MCLK_PAL_HZ / (LINE_MCLK * LINES_PAL) as f64,
        }
    }

    /// Reset button (RAM survives).
    pub fn reset(&mut self) {
        self.m.reset();
    }

    /// A four-player adapter is plugged in (which one is chosen for the game); without it
    /// only pads 1 and 2 reach the game. Part of the emulated state.
    pub fn set_multitap(&mut self, on: bool) {
        let tap = match on {
            false => Multitap::None,
            true if self.is_electronic_arts() => Multitap::EaFourWay,
            true => Multitap::TeamPlayer,
        };
        if tap != self.m.s.io.multitap() {
            self.m.s.io.set_multitap(tap);
        }
    }

    pub fn multitap(&self) -> bool {
        self.m.s.io.multitap() != Multitap::None
    }

    /// The header's copyright field (0x110-0x11F) holds Electronic Arts' maker code `T-50`:
    /// their games talk to the EA 4 Way Play, everyone else's to the Sega Team Player.
    fn is_electronic_arts(&self) -> bool {
        let mut field = [0u8; 16];
        for (i, byte) in field.iter_mut().enumerate() {
            *byte = self.m.cart.read8(&self.m.s.cart, 0x110 + i as u32);
        }
        // "T-50" not followed by another digit (that would be a different maker).
        (0..=field.len() - 4).any(|i| &field[i..i + 4] == b"T-50" && !field.get(i + 4).is_some_and(u8::is_ascii_digit))
    }

    /// Runs until the VDP finishes the last displayed line (vertical blanking starts).
    /// `pads[i]` is controller i+1 (see [`crate::pad`]).
    pub fn run_frame(&mut self, pads: [u16; 4]) {
        self.m.s.io.set_pads(pads);
        // A frame always ends within one frame's time; two frames is the bound for a machine
        // that for any reason never signals it.
        let lines = if self.region() == Region::Europe { LINES_PAL } else { LINES_NTSC };
        let limit = 2 * LINE_MCLK * lines;
        let start = self.m.s.now;
        loop {
            self.m.step_m68k();
            self.m.run_z80();
            self.m.sync_vdp();
            if self.m.s.vdp.take_frame_done() || self.m.s.now.wrapping_sub(start) >= limit {
                break;
            }
        }
        self.m.sync_sound();
    }

    /// Size of the last finished picture: 256 or 320 wide, 224 or 240 high (448 or 480 in
    /// interlace mode 2).
    pub fn frame_size(&self) -> (usize, usize) {
        (self.m.screen.w, self.m.screen.h)
    }

    /// Writes the last finished picture as RGBA8888 into `out` (resized to
    /// `width * height * 4` bytes) and returns its size.
    pub fn frame_rgba(&self, out: &mut Vec<u8>) -> (usize, usize) {
        let (w, h) = self.frame_size();
        out.resize(w * h * 4, 0);
        if w == 0 {
            return (w, h);
        }
        for (row, line) in out.chunks_exact_mut(w * 4).zip(self.m.screen.px.chunks(MAX_W)) {
            for (rgba, &px) in row.chunks_exact_mut(4).zip(line) {
                rgba.copy_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, px as u8, 255]);
            }
        }
        (w, h)
    }

    /// Which layer each pixel of the last finished picture came from, one byte per pixel,
    /// row by row (`out` is resized to `width * height`): 0 = backdrop colour, 1 = plane B,
    /// 2 = plane A or the window, 3 = sprite. For display helpers; not emulated state.
    pub fn frame_layers(&self, out: &mut Vec<u8>) -> (usize, usize) {
        let (w, h) = self.frame_size();
        out.resize(w * h, 0);
        if w == 0 {
            return (w, h);
        }
        for (row, line) in out.chunks_exact_mut(w).zip(self.m.screen.px.chunks(MAX_W)) {
            for (layer, &px) in row.iter_mut().zip(line) {
                *layer = (px >> 24) as u8;
            }
        }
        (w, h)
    }

    /// Output sample rate in Hz. May be nudged every frame to keep the audio queue level.
    pub fn set_sample_rate(&mut self, hz: f64) {
        self.m.audio.set_sample_rate(hz);
    }

    /// Appends the samples produced since the last drain: interleaved stereo (left, right),
    /// range about -1.0..1.0.
    pub fn drain_audio(&mut self, out: &mut Vec<f32>) {
        self.m.audio.drain(out);
    }

    /// Drops buffered samples (rollback re-simulation must not be heard twice).
    pub fn clear_audio(&mut self) {
        self.m.audio.clear();
    }

    /// Fast-forward: emulation runs as usual, but no pixels are drawn and no samples are
    /// produced. Not part of the snapshot or the hash.
    pub fn set_fast_forward(&mut self, on: bool) {
        self.m.output = !on;
        self.m.audio.set_muted(on);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { state: self.m.s.clone() }
    }

    pub fn restore(&mut self, snapshot: &Snapshot) {
        self.m.s.clone_from(&snapshot.state);
    }

    /// Stable hash of the emulated state, identical across machines for identical state.
    pub fn state_hash(&self) -> u64 {
        let mut hasher = StateHasher::new();
        self.m.s.hash(&mut hasher);
        hasher.finish()
    }

    /// Battery-backed RAM, if the cartridge has it.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.m.s.cart.battery_ram()
    }

    pub fn load_battery_ram(&mut self, data: &[u8]) {
        self.m.s.cart.load_battery_ram(data);
    }

    /// Debugging and tests: the 64 KB work RAM, in the 68000's byte order (index 0 is the
    /// byte at 0xFF0000).
    pub fn ram(&self) -> &[u8] {
        &self.m.s.ram
    }

    /// Debugging and tests: the Z80's 8 KB RAM.
    pub fn z80_ram(&self) -> &[u8] {
        &self.m.s.zram
    }

    /// Serializes the machine: magic, version, ROM CRC and region, then the state. The ROM is
    /// not included; load it into an `Md` running the same ROM.
    pub fn save_state(&self) -> Vec<u8> {
        self.encode(&self.m.s)
    }

    /// Serializes `snapshot` (taken from this machine) like [`Md::save_state`].
    pub fn encode_state(&self, snapshot: &Snapshot) -> Vec<u8> {
        self.encode(&snapshot.state)
    }

    fn encode(&self, state: &State) -> Vec<u8> {
        let mut out = Vec::with_capacity(STATE_HEADER_LEN + 0x40000);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_VERSION.to_le_bytes());
        out.extend_from_slice(&self.info().crc32.to_le_bytes());
        out.push(region_byte(self.region()));
        // Plain data into a vector: the only possible failure is the size limit, far above
        // what a state holds.
        state_options().serialize_into(&mut out, state).expect("the state serializes");
        out
    }

    /// Loads a [`Md::save_state`] blob. On any error the running machine is left unchanged.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), StateError> {
        if bytes.len() < 4 || &bytes[..4] != STATE_MAGIC {
            return Err(StateError::BadMagic);
        }
        if bytes.len() < STATE_HEADER_LEN {
            return Err(StateError::Corrupt);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != STATE_VERSION {
            return Err(StateError::Version(version));
        }
        let crc = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
        if crc != self.info().crc32 || bytes[10] != region_byte(self.region()) {
            return Err(StateError::WrongRom);
        }
        let state: State = state_options().deserialize(&bytes[STATE_HEADER_LEN..]).map_err(|_| StateError::Corrupt)?;
        // The memory maps index these without checks.
        if state.ram.len() != self.m.s.ram.len() || state.zram.len() != self.m.s.zram.len() {
            return Err(StateError::Corrupt);
        }
        self.m.s = state;
        self.m.audio.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1 KB cartridge: every vector points at 0x200, where a `BRA.s` jumps to itself.
    /// `copyright` is the header field at 0x110.
    fn rom_with(copyright: &[u8; 16]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x400];
        rom[0..4].copy_from_slice(&0x00FF_FE00u32.to_be_bytes());
        for vector in 1..64 {
            rom[vector * 4..vector * 4 + 4].copy_from_slice(&0x200u32.to_be_bytes());
        }
        rom[0x100..0x200].fill(b' ');
        rom[0x100..0x110].copy_from_slice(b"SEGA MEGA DRIVE ");
        rom[0x110..0x120].copy_from_slice(copyright);
        rom[0x180..0x18E].copy_from_slice(b"GM 00000000-00");
        rom[0x1A0..0x1A4].copy_from_slice(&0u32.to_be_bytes());
        rom[0x1A4..0x1A8].copy_from_slice(&0x3FFu32.to_be_bytes());
        rom[0x1A8..0x1AC].copy_from_slice(&0x00FF_0000u32.to_be_bytes());
        rom[0x1AC..0x1B0].copy_from_slice(&0x00FF_FFFFu32.to_be_bytes());
        rom[0x1F0..0x1F3].copy_from_slice(b"JUE");
        rom[0x200..0x202].copy_from_slice(&[0x60, 0xFE]);
        rom
    }

    fn rom() -> Vec<u8> {
        rom_with(b"(C)SEGA 1991.JAN")
    }

    fn md() -> Md {
        Md::new(&rom(), Some(Region::Americas)).unwrap()
    }

    fn run(md: &mut Md, frames: usize) {
        for i in 0..frames {
            md.run_frame([i as u16 & 0xFF, 0, 0, 0]);
        }
    }

    #[test]
    fn hasher_is_fixed_width_and_word_based() {
        let hash = |f: &dyn Fn(&mut StateHasher)| {
            let mut h = StateHasher::new();
            f(&mut h);
            h.finish()
        };
        assert_eq!(hash(&|h| h.write_usize(5)), hash(&|h| h.write_u64(5)));
        assert_eq!(hash(&|h| h.write_isize(-1)), hash(&|h| h.write_u64(u64::MAX)));
        // Eight bytes are one little-endian word.
        assert_eq!(hash(&|h| h.write(&[1, 2, 3, 4, 5, 6, 7, 8])), hash(&|h| h.write_u64(0x0807_0605_0403_0201)));
        // A short tail is not the same as its zero-padded word, and every byte counts.
        assert_ne!(hash(&|h| h.write(&[1, 2, 3])), hash(&|h| h.write(&[1, 2, 3, 0])));
        assert_ne!(hash(&|h| h.write(&[0; 16])), hash(&|h| h.write(&[0; 17])));
        let mut bytes = [0u8; 100];
        let zero = hash(&|h| h.write(&bytes));
        bytes[99] = 1;
        assert_ne!(zero, hash(&|h| h.write(&bytes)));
        assert_ne!(hash(&|_| ()), hash(&|h| h.write_u8(0)));
    }

    #[test]
    fn frame_rates() {
        let ntsc = md();
        assert_eq!(ntsc.region(), Region::Americas);
        assert!((ntsc.frame_rate() - 59.9228).abs() < 0.0001, "{}", ntsc.frame_rate());
        let japan = Md::new(&rom(), Some(Region::Japan)).unwrap();
        assert_eq!(japan.frame_rate(), ntsc.frame_rate());
        let pal = Md::new(&rom(), Some(Region::Europe)).unwrap();
        assert_eq!(pal.region(), Region::Europe);
        assert!((pal.frame_rate() - 49.7015).abs() < 0.0001, "{}", pal.frame_rate());
        assert_eq!(ntsc.info().crc32, crate::cart::crc32(&[&rom()]));
    }

    #[test]
    fn run_frame_returns_with_a_picture() {
        let mut md = md();
        let before = md.state_hash();
        run(&mut md, 2);
        assert_ne!(md.state_hash(), before);
        let (w, h) = md.frame_size();
        assert!(w == 320 || w == 256, "width {w}");
        assert_eq!(h, 224);
    }

    #[test]
    fn a_frame_takes_one_frame_of_time() {
        let mut md = md();
        run(&mut md, 2);
        let start = md.m.s.now;
        run(&mut md, 4);
        let per_frame = (md.m.s.now - start) / 4;
        let frame = LINE_MCLK * LINES_NTSC;
        assert!(per_frame.abs_diff(frame) < LINE_MCLK, "{per_frame} master clocks per frame, expected about {frame}");
    }

    #[test]
    fn frame_rgba_resizes_the_buffer() {
        let mut md = md();
        run(&mut md, 1);
        let mut out = vec![7u8; 3];
        let (w, h) = md.frame_rgba(&mut out);
        assert_eq!((w, h), md.frame_size());
        assert_eq!(out.len(), w * h * 4);
        assert!(out.chunks_exact(4).all(|px| px[3] == 255));
        let mut big = vec![7u8; 320 * 480 * 4 + 5];
        md.frame_rgba(&mut big);
        assert_eq!(big, out);
    }

    #[test]
    fn frame_rgba_reads_rows_max_w_apart() {
        let mut md = md();
        md.m.screen.w = 256;
        md.m.screen.h = 224;
        md.m.screen.px[0] = 0x0011_2233;
        md.m.screen.px[MAX_W + 255] = 0x00AA_BBCC;
        let mut out = Vec::new();
        assert_eq!(md.frame_rgba(&mut out), (256, 224));
        assert_eq!(&out[..4], &[0x11, 0x22, 0x33, 255]);
        let at = (256 + 255) * 4;
        assert_eq!(&out[at..at + 4], &[0xAA, 0xBB, 0xCC, 255]);
    }

    #[test]
    fn snapshot_and_restore_replay_identically() {
        let mut md = md();
        run(&mut md, 2);
        let snapshot = md.snapshot();
        let at_snapshot = md.state_hash();
        run(&mut md, 3);
        let a = md.state_hash();
        assert_ne!(a, at_snapshot);
        md.restore(&snapshot);
        assert_eq!(md.state_hash(), at_snapshot);
        run(&mut md, 3);
        assert_eq!(md.state_hash(), a);
    }

    #[test]
    fn fast_forward_does_not_change_the_state() {
        let (mut shown, mut skipped) = (md(), md());
        skipped.set_fast_forward(true);
        run(&mut shown, 3);
        run(&mut skipped, 3);
        assert_eq!(shown.state_hash(), skipped.state_hash());
        let mut samples = Vec::new();
        skipped.drain_audio(&mut samples);
        assert!(samples.is_empty());
        skipped.set_fast_forward(false);
        run(&mut shown, 2);
        run(&mut skipped, 2);
        assert_eq!(shown.state_hash(), skipped.state_hash());
    }

    #[test]
    fn audio_drains_in_stereo_and_clears() {
        let mut md = md();
        md.set_sample_rate(48_000.0);
        run(&mut md, 3);
        let mut samples = Vec::new();
        md.drain_audio(&mut samples);
        assert!(!samples.is_empty());
        assert_eq!(samples.len() % 2, 0);
        assert!(samples.iter().all(|s| s.is_finite() && s.abs() <= 1.5));
        run(&mut md, 1);
        md.clear_audio();
        samples.clear();
        md.drain_audio(&mut samples);
        assert!(samples.is_empty());
    }

    #[test]
    fn save_state_loads_into_a_fresh_machine() {
        let mut md1 = md();
        run(&mut md1, 3);
        let blob = md1.save_state();
        assert_eq!(&blob[..4], b"EBMD");
        assert_eq!(blob, md1.encode_state(&md1.snapshot()));
        let mut md2 = md();
        assert_ne!(md2.state_hash(), md1.state_hash());
        md2.load_state(&blob).unwrap();
        assert_eq!(md2.state_hash(), md1.state_hash());
        run(&mut md1, 2);
        run(&mut md2, 2);
        assert_eq!(md2.state_hash(), md1.state_hash());
    }

    #[test]
    fn damaged_save_states_leave_the_machine_alone() {
        let mut md1 = md();
        run(&mut md1, 2);
        let blob = md1.save_state();
        let hash = md1.state_hash();
        let mut check = |bytes: &[u8], error: StateError| {
            assert_eq!(md1.load_state(bytes), Err(error));
            assert_eq!(md1.state_hash(), hash, "{error:?} changed the machine");
        };

        let mut bad = blob.clone();
        bad[0] = b'X';
        check(&bad, StateError::BadMagic);
        check(b"EB", StateError::BadMagic);
        check(&[], StateError::BadMagic);

        let mut bad = blob.clone();
        bad[4] = 2;
        check(&bad, StateError::Version(2));

        let mut other = Md::new(&rom_with(b"(C)SEGA 1992.FEB"), Some(Region::Americas)).unwrap();
        run(&mut other, 1);
        check(&other.save_state(), StateError::WrongRom);
        let mut pal = Md::new(&rom(), Some(Region::Europe)).unwrap();
        run(&mut pal, 1);
        check(&pal.save_state(), StateError::WrongRom);

        check(&blob[..blob.len() - 10], StateError::Corrupt);
        check(&blob[..blob.len() / 2], StateError::Corrupt);
        check(&blob[..STATE_HEADER_LEN], StateError::Corrupt);
        check(&blob[..8], StateError::Corrupt);
        let mut long = blob.clone();
        long.push(0);
        check(&long, StateError::Corrupt);

        md1.load_state(&blob).unwrap();
        assert_eq!(md1.state_hash(), hash);
    }

    #[test]
    fn multitap_is_part_of_the_state() {
        let mut md = md();
        assert!(!md.multitap());
        let off = md.state_hash();
        md.set_multitap(true);
        assert!(md.multitap());
        assert_eq!(md.m.s.io.multitap(), Multitap::TeamPlayer);
        let on = md.state_hash();
        assert_ne!(on, off);
        let snapshot = md.snapshot();
        let blob = md.save_state();
        md.set_multitap(false);
        assert!(!md.multitap());
        assert_eq!(md.state_hash(), off);
        md.restore(&snapshot);
        assert!(md.multitap());
        assert_eq!(md.state_hash(), on);
        md.set_multitap(false);
        md.load_state(&blob).unwrap();
        assert!(md.multitap());
    }

    #[test]
    fn electronic_arts_games_get_the_four_way_play() {
        let mut ea = Md::new(&rom_with(b"(C)T-50 1993.JUL"), None).unwrap();
        ea.set_multitap(true);
        assert_eq!(ea.m.s.io.multitap(), Multitap::EaFourWay);
        ea.set_multitap(false);
        assert_eq!(ea.m.s.io.multitap(), Multitap::None);
        // Another maker whose code starts the same way.
        let mut other = Md::new(&rom_with(b"(C)T-502 1993.JU"), None).unwrap();
        other.set_multitap(true);
        assert_eq!(other.m.s.io.multitap(), Multitap::TeamPlayer);
    }

    #[test]
    fn reset_and_battery_ram() {
        let mut md = md();
        run(&mut md, 2);
        md.reset();
        run(&mut md, 2);
        assert_eq!(md.frame_size().1, 224);
        assert_eq!(md.battery_ram(), None);
        let hash = md.state_hash();
        md.load_battery_ram(&[1, 2, 3]);
        assert_eq!(md.state_hash(), hash);
    }
}
