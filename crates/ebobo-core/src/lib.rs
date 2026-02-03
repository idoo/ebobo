//! ebobo-core: deterministic NES emulator (2A03 CPU + APU, 2C02 PPU, cartridge boards).
//!
//! Determinism contract (rollback netplay depends on it): the same ROM bytes, region,
//! Four Score setting and sequence of `run_frame` inputs produce bit-identical state on every
//! machine. No clocks, no randomness, no floats in emulated state (floats live only in the
//! audio output path, which is outside `Snapshot`).
//!
//! Module ownership (one owner per file):
//! - `cpu.rs`: 6502 core, generic over [`cpu::CpuBus`].
//! - `ppu.rs`: dot-stepped 2C02.
//! - `apu.rs`: 2A03 audio + resampling `AudioOut`.
//! - `cart.rs` + `mapper/`: iNES/NES 2.0 parsing and boards (one file per board family).
//! - `bus.rs` + `palette.rs`: CPU memory map, controllers/Four Score, DMA, clock ratios.

mod apu;
mod bus;
pub mod cart;
mod cpu;
mod mapper;
mod palette;
mod ppu;

use bincode::Options;
use std::hash::{Hash, Hasher};

pub use cart::{Mirroring, Region, RomError, RomInfo};
pub use mapper::SUPPORTED_MAPPERS;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 240;

/// Controller bits in the order the game shifts them out of $4016/$4017 (bit 0 first).
pub mod pad {
    pub const A: u8 = 0x01;
    pub const B: u8 = 0x02;
    pub const SELECT: u8 = 0x04;
    pub const START: u8 = 0x08;
    pub const UP: u8 = 0x10;
    pub const DOWN: u8 = 0x20;
    pub const LEFT: u8 = 0x40;
    pub const RIGHT: u8 = 0x80;
}

pub struct Nes {
    cpu: cpu::Cpu,
    bus: bus::Bus,
    info: RomInfo,
}

/// Complete emulated state. Cloning is cheap: ROM bytes are shared, the frame buffer and audio
/// output are not part of it. Rollback saves one of these every frame.
#[derive(Clone)]
pub struct Snapshot {
    cpu: cpu::Cpu,
    bus: bus::BusState,
}

impl Nes {
    /// Parses `rom` (iNES / NES 2.0 file bytes) and powers the console on.
    /// `region` overrides the header's region (`None` = trust the header).
    pub fn new(rom: &[u8], region: Option<Region>) -> Result<Nes, RomError> {
        let mut info = cart::parse(rom)?;
        if let Some(region) = region {
            info.region = region;
        }
        let mut bus = bus::Bus::new(&info)?;
        bus.set_four_score(info.four_score);
        let mut cpu = cpu::Cpu::new();
        cpu.power_on(&mut bus);
        Ok(Nes { cpu, bus, info })
    }

    pub fn info(&self) -> &RomInfo {
        &self.info
    }

    /// Number of ESX1/ESX2 sprite banks in the ROM (`docs/EXTENSIONS.md`), 0 without a chunk.
    pub fn esx_banks(&self) -> usize {
        self.info.esx.as_ref().map_or(0, |e| e.banks.len())
    }

    /// Debugging: writes internal RAM ($0000-$1FFF) without bus side effects or cycles.
    /// For tools and tests only; netplay never calls it.
    pub fn poke_ram(&mut self, addr: u16, val: u8) {
        self.bus.poke_ram(addr, val);
    }

    pub fn region(&self) -> Region {
        self.info.region
    }

    /// Frames per second of the emulated console.
    pub fn frame_rate(&self) -> f64 {
        match self.info.region {
            Region::Ntsc => 60.0988,
            Region::Pal => 50.0070,
        }
    }

    /// Reset button (RAM survives).
    pub fn reset(&mut self) {
        self.bus.reset();
        self.cpu.reset(&mut self.bus);
    }

    /// Four Score / NES Satellite adapter: controllers 3 and 4 become readable.
    pub fn set_four_score(&mut self, on: bool) {
        self.bus.set_four_score(on);
    }

    /// `true` (default) = hardware 8 sprites per line. `false` also draws the sprites past
    /// the 8, in OAM order, as a display-only overlay: emulation, `state_hash` and save
    /// states are unchanged, so netplay peers may differ. Survives restores and loads.
    pub fn set_sprite_limit(&mut self, on: bool) {
        self.bus.set_sprite_limit(on);
    }

    pub fn four_score(&self) -> bool {
        self.bus.four_score()
    }

    /// Runs until the PPU enters vblank. `pads[i]` is controller i+1 (see [`pad`]).
    pub fn run_frame(&mut self, pads: [u8; 4]) {
        self.bus.set_pads(pads);
        loop {
            self.cpu.step(&mut self.bus);
            if self.bus.take_frame_done() {
                break;
            }
        }
    }

    /// Last completed picture: `WIDTH * HEIGHT` entries, bits 0-5 palette index,
    /// bits 6-8 PPUMASK colour emphasis.
    pub fn frame(&self) -> &[u16] {
        self.bus.frame()
    }

    /// Converts [`Nes::frame`] to RGBA8888 (`WIDTH * HEIGHT * 4` bytes).
    pub fn frame_rgba(&self, out: &mut [u8]) {
        palette::write_rgba(self.bus.frame(), self.info.region, out);
    }

    /// Output sample rate of [`Nes::drain_audio`]. Fractional so the frontend can nudge it
    /// to keep the audio queue level.
    pub fn set_sample_rate(&mut self, hz: f64) {
        self.bus.audio().set_sample_rate(hz);
    }

    /// Appends mono samples produced since the last drain (range about -1.0..1.0).
    pub fn drain_audio(&mut self, out: &mut Vec<f32>) {
        self.bus.audio().drain(out);
    }

    /// Drops buffered samples (rollback re-simulation must not be heard twice).
    pub fn clear_audio(&mut self) {
        self.bus.audio().clear();
    }

    /// Host-side fast-forward: while on, frames are emulated exactly (state, hash, IRQ/NMI
    /// timing unchanged) but no pixels are composed into [`Nes::frame`] and no audio is
    /// produced. Not part of the snapshot or the hash.
    pub fn set_fast_forward(&mut self, on: bool) {
        self.bus.set_fast_forward(on);
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            cpu: self.cpu.clone(),
            bus: self.bus.state().clone(),
        }
    }

    pub fn restore(&mut self, snapshot: &Snapshot) {
        self.cpu = snapshot.cpu.clone();
        self.bus.restore(&snapshot.bus);
    }

    /// Stable hash of the emulated state, identical across machines for identical state.
    pub fn state_hash(&self) -> u64 {
        let mut hasher = StateHasher::default();
        self.cpu.hash(&mut hasher);
        self.bus.state().hash(&mut hasher);
        hasher.finish()
    }

    /// Battery-backed PRG RAM, if the board has it.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.bus.battery_ram()
    }

    pub fn load_battery_ram(&mut self, data: &[u8]) {
        self.bus.load_battery_ram(data);
    }

    /// Side-effect-free CPU address space read (tests, tools).
    pub fn peek(&self, addr: u16) -> u8 {
        self.bus.peek(addr)
    }

    /// Debugging: CPU registers before the next instruction.
    pub fn cpu_state(&self) -> CpuState {
        let c = &self.cpu;
        CpuState { pc: c.pc, a: c.a, x: c.x, y: c.y, p: c.p, sp: c.sp }
    }

    /// Debugging: runs one CPU instruction (or interrupt entry) with `pads` latched. Returns
    /// true when the PPU entered vblank during it, i.e. a `run_frame` would have ended here.
    pub fn step_instruction(&mut self, pads: [u8; 4]) -> bool {
        self.bus.set_pads(pads);
        self.cpu.step(&mut self.bus);
        self.bus.take_frame_done()
    }

    /// Serialized machine state (a save state): `EBST` magic, format version, ROM crc32,
    /// region, then the bincode of the CPU and [`Snapshot`] bus state. ROM bytes are not
    /// included; load it into a `Nes` running the same ROM.
    pub fn save_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 * 1024);
        out.extend_from_slice(STATE_MAGIC);
        out.extend_from_slice(&STATE_VERSION.to_le_bytes());
        out.extend_from_slice(&self.info.crc32.to_le_bytes());
        out.push(region_byte(self.info.region));
        state_options()
            .serialize_into(&mut out, &(&self.cpu, self.bus.state()))
            .expect("in-memory state serialization cannot fail");
        out
    }

    /// Loads a [`Nes::save_state`] blob. On any error the running machine is left unchanged.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), StateError> {
        if bytes.len() < 4 || &bytes[..4] != STATE_MAGIC {
            return Err(StateError::BadMagic);
        }
        if bytes.len() < STATE_HEADER {
            return Err(StateError::Corrupt);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != STATE_VERSION {
            return Err(StateError::Version(version));
        }
        let crc = u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
        if crc != self.info.crc32 || bytes[10] != region_byte(self.info.region) {
            return Err(StateError::WrongRom);
        }
        let (cpu, bus): (cpu::Cpu, bus::BusState) = state_options()
            .deserialize(&bytes[STATE_HEADER..])
            .map_err(|_| StateError::Corrupt)?;
        self.bus.load_state(bus).map_err(|_| StateError::Corrupt)?;
        self.cpu = cpu;
        Ok(())
    }
}

const STATE_MAGIC: &[u8; 4] = b"EBST";
/// Bump on any change to the serialized layout of `Cpu` / `BusState`.
const STATE_VERSION: u16 = 3;
/// Magic, version, crc32, region.
const STATE_HEADER: usize = 4 + 2 + 4 + 1;
/// Upper bound for the decoded state; rejects garbage length prefixes before they allocate.
const STATE_LIMIT: u64 = 16 << 20;

fn state_options() -> impl bincode::Options {
    use bincode::Options;
    bincode::DefaultOptions::new().with_limit(STATE_LIMIT)
}

fn region_byte(region: Region) -> u8 {
    match region {
        Region::Ntsc => 0,
        Region::Pal => 1,
    }
}

/// Why [`Nes::load_state`] refused a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateError {
    /// Not an ebobo save state.
    BadMagic,
    /// Saved by an incompatible state format version.
    Version(u16),
    /// Saved from another ROM or region.
    WrongRom,
    /// Truncated or damaged.
    Corrupt,
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::BadMagic => f.write_str("not an ebobo save state"),
            StateError::Version(v) => write!(f, "unsupported save state version {v}"),
            StateError::WrongRom => f.write_str("save state is from another ROM or region"),
            StateError::Corrupt => f.write_str("save state is truncated or damaged"),
        }
    }
}

impl std::error::Error for StateError {}

/// Serde for `[u8; N]` with N > 32 (serde's built-in array impls stop at 32): a byte string.
mod big_array {
    use serde::de::{Deserializer, Error, SeqAccess, Visitor};
    use serde::Serializer;

    pub fn serialize<S: Serializer, const N: usize>(a: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(a)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[u8; N], D::Error> {
        struct V<const N: usize>;
        impl<'de, const N: usize> Visitor<'de> for V<N> {
            type Value = [u8; N];
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "{N} bytes")
            }
            fn visit_bytes<E: Error>(self, b: &[u8]) -> Result<[u8; N], E> {
                b.try_into().map_err(|_| E::invalid_length(b.len(), &self))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<[u8; N], A::Error> {
                let mut a = [0u8; N];
                for (i, v) in a.iter_mut().enumerate() {
                    *v = seq.next_element()?.ok_or_else(|| A::Error::invalid_length(i, &self))?;
                }
                Ok(a)
            }
        }
        d.deserialize_bytes(V::<N>)
    }
}

/// CPU registers for debuggers and tracing tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuState {
    pub pc: u16,
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub p: u8,
    pub sp: u8,
}

/// FxHash-style word hasher: fast, platform independent for our state types.
#[derive(Default)]
struct StateHasher(u64);

impl StateHasher {
    #[inline]
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for StateHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            self.mix(u64::from_le_bytes(chunk.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.mix(u64::from_le_bytes(word) ^ ((rest.len() as u64) << 56));
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
