//! ebobo-console: one machine type for every console ebobo emulates.
//!
//! Netplay and the frontend drive a [`Console`] and never name a core: the ROM bytes decide
//! which one runs. The determinism contract of the cores carries over: the same ROM, region,
//! multitap setting and inputs give bit-identical state everywhere.

use std::fmt;

use serde::{Deserialize, Serialize};

pub use ebobo_core as nes;
pub use ebobo_md as md;

/// Largest picture any console produces ([`Console::frame_rgba`]).
pub const MAX_W: usize = md::MAX_W;
pub const MAX_H: usize = md::MAX_H;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum System {
    Nes,
    MegaDrive,
}

/// Console region. A NES is `Ntsc` or `Pal`; a Mega Drive is `Ntsc` (Americas), `Pal`
/// (Europe) or `Japan`, which a NES treats as `Ntsc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Region {
    Ntsc,
    Pal,
    Japan,
}

impl Region {
    /// 50 frames per second.
    pub fn is_pal(self) -> bool {
        self == Region::Pal
    }

    /// One byte for the wire and for file headers.
    pub fn code(self) -> u8 {
        match self {
            Region::Ntsc => 0,
            Region::Pal => 1,
            Region::Japan => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Region> {
        match code {
            0 => Some(Region::Ntsc),
            1 => Some(Region::Pal),
            2 => Some(Region::Japan),
            _ => None,
        }
    }

    fn nes(self) -> nes::Region {
        match self {
            Region::Pal => nes::Region::Pal,
            Region::Ntsc | Region::Japan => nes::Region::Ntsc,
        }
    }

    fn md(self) -> md::Region {
        match self {
            Region::Ntsc => md::Region::Americas,
            Region::Pal => md::Region::Europe,
            Region::Japan => md::Region::Japan,
        }
    }
}

impl From<nes::Region> for Region {
    fn from(region: nes::Region) -> Region {
        match region {
            nes::Region::Ntsc => Region::Ntsc,
            nes::Region::Pal => Region::Pal,
        }
    }
}

impl From<md::Region> for Region {
    fn from(region: md::Region) -> Region {
        match region {
            md::Region::Americas => Region::Ntsc,
            md::Region::Europe => Region::Pal,
            md::Region::Japan => Region::Japan,
        }
    }
}

/// What a ROM file is, read from its header without building a machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RomId {
    pub system: System,
    /// The game's identity in netplay, save files and the library.
    pub crc32: u32,
    /// The region the header asks for.
    pub region: Region,
    /// A [`Console`] can run it (a NES board ebobo does not emulate gives `false`).
    pub supported: bool,
    /// NES mapper number, 0 for other systems.
    pub mapper: u16,
    /// The header asks for a four-player adapter.
    pub multitap: bool,
}

/// Identifies `bytes` as a ROM of one of the systems, `None` when it is none.
pub fn identify(bytes: &[u8]) -> Option<RomId> {
    if let Ok(info) = nes::cart::parse(bytes) {
        return Some(RomId {
            system: System::Nes,
            crc32: info.crc32,
            region: info.region.into(),
            supported: nes::SUPPORTED_MAPPERS.contains(&info.mapper),
            mapper: info.mapper,
            multitap: info.four_score,
        });
    }
    if md::PLAYABLE && md::cart::identify(bytes) {
        let info = md::cart::parse(bytes).ok()?;
        return Some(RomId {
            system: System::MegaDrive,
            crc32: info.crc32,
            region: info.region.into(),
            supported: true,
            mapper: 0,
            multitap: false,
        });
    }
    None
}

/// CRC-32 (IEEE 802.3) over the concatenation of `parts`.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    nes::cart::crc32(parts)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RomError {
    /// Not a ROM of any system ebobo emulates.
    Unknown,
    Nes(nes::RomError),
    Md(md::RomError),
}

impl fmt::Display for RomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RomError::Unknown => write!(f, "not a NES or Mega Drive ROM"),
            RomError::Nes(e) => e.fmt(f),
            RomError::Md(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RomError {}

/// Why [`Console::load_state`] refused a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateError {
    /// Not an ebobo save state of this system.
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

impl From<nes::StateError> for StateError {
    fn from(e: nes::StateError) -> StateError {
        match e {
            nes::StateError::BadMagic => StateError::BadMagic,
            nes::StateError::Version(v) => StateError::Version(v),
            nes::StateError::WrongRom => StateError::WrongRom,
            nes::StateError::Corrupt => StateError::Corrupt,
        }
    }
}

impl From<md::StateError> for StateError {
    fn from(e: md::StateError) -> StateError {
        match e {
            md::StateError::BadMagic => StateError::BadMagic,
            md::StateError::Version(v) => StateError::Version(v),
            md::StateError::WrongRom => StateError::WrongRom,
            md::StateError::Corrupt => StateError::Corrupt,
        }
    }
}

/// A console with a game in it.
pub enum Console {
    Nes(Box<nes::Nes>),
    Md(Box<md::Md>),
}

/// Complete emulated state of a [`Console`]. Rollback saves one of these every frame.
#[derive(Clone)]
pub enum Snapshot {
    Nes(nes::Snapshot),
    Md(md::Snapshot),
}

impl Console {
    /// Builds the console `rom` belongs to and powers it on. `region` overrides the
    /// header's choice (`None` = trust the header).
    pub fn new(rom: &[u8], region: Option<Region>) -> Result<Console, RomError> {
        match identify(rom).ok_or(RomError::Unknown)?.system {
            System::Nes => nes::Nes::new(rom, region.map(Region::nes))
                .map(|nes| Console::Nes(Box::new(nes)))
                .map_err(RomError::Nes),
            System::MegaDrive => md::Md::new(rom, region.map(Region::md))
                .map(|md| Console::Md(Box::new(md)))
                .map_err(RomError::Md),
        }
    }

    pub fn system(&self) -> System {
        match self {
            Console::Nes(_) => System::Nes,
            Console::Md(_) => System::MegaDrive,
        }
    }

    /// The NES inside, for what only it has (sprite limit, ESX banks, debugging).
    pub fn as_nes(&self) -> Option<&nes::Nes> {
        match self {
            Console::Nes(nes) => Some(nes),
            Console::Md(_) => None,
        }
    }

    pub fn as_nes_mut(&mut self) -> Option<&mut nes::Nes> {
        match self {
            Console::Nes(nes) => Some(nes),
            Console::Md(_) => None,
        }
    }

    /// The game's identity (see [`RomId::crc32`]).
    pub fn crc32(&self) -> u32 {
        match self {
            Console::Nes(nes) => nes.info().crc32,
            Console::Md(md) => md.info().crc32,
        }
    }

    /// The region the console runs as.
    pub fn region(&self) -> Region {
        match self {
            Console::Nes(nes) => nes.region().into(),
            Console::Md(md) => md.region().into(),
        }
    }

    /// Frames per second of the emulated console.
    pub fn frame_rate(&self) -> f64 {
        match self {
            Console::Nes(nes) => nes.frame_rate(),
            Console::Md(md) => md.frame_rate(),
        }
    }

    /// Reset button (RAM survives).
    pub fn reset(&mut self) {
        match self {
            Console::Nes(nes) => nes.reset(),
            Console::Md(md) => md.reset(),
        }
    }

    /// Four-player adapter (NES Four Score, Mega Drive Team Player / 4 Way Play): without
    /// it only controllers 1 and 2 reach the game. Part of the emulated state.
    pub fn set_multitap(&mut self, on: bool) {
        match self {
            Console::Nes(nes) => nes.set_four_score(on),
            Console::Md(md) => md.set_multitap(on),
        }
    }

    pub fn multitap(&self) -> bool {
        match self {
            Console::Nes(nes) => nes.four_score(),
            Console::Md(md) => md.multitap(),
        }
    }

    /// Runs one frame, until the picture is complete. `pads[i]` is controller i+1; the bits
    /// are the system's own ([`nes::pad`] in the low byte, [`md::pad`]).
    pub fn run_frame(&mut self, pads: [u16; 4]) {
        match self {
            Console::Nes(nes) => nes.run_frame(pads.map(|p| p as u8)),
            Console::Md(md) => md.run_frame(pads),
        }
    }

    /// Size of the last finished picture. A Mega Drive game may change it between frames.
    pub fn frame_size(&self) -> (usize, usize) {
        match self {
            Console::Nes(_) => (nes::WIDTH, nes::HEIGHT),
            Console::Md(md) => md.frame_size(),
        }
    }

    /// Writes the last finished picture as RGBA8888 into `out` (resized to
    /// `width * height * 4` bytes) and returns its size. Every picture is meant for a 4:3
    /// screen, whatever its pixel count.
    pub fn frame_rgba(&self, out: &mut Vec<u8>) -> (usize, usize) {
        match self {
            Console::Nes(nes) => {
                out.resize(nes::WIDTH * nes::HEIGHT * 4, 0);
                nes.frame_rgba(out);
                (nes::WIDTH, nes::HEIGHT)
            }
            Console::Md(md) => md.frame_rgba(out),
        }
    }

    /// Which layer each pixel of the last finished picture came from, one byte per pixel,
    /// row by row (`out` is resized to `width * height`), higher = nearer the viewer. NES:
    /// 0 = backdrop colour, 1 = sprite behind the background, 2 = background, 3 = sprite in
    /// front. Mega Drive: 0 = backdrop colour, 1 = plane B, 2 = plane A
    /// or the window, 3 = sprite. For display helpers (edge-aware scaling, shadows); not
    /// emulated state.
    pub fn frame_layers(&self, out: &mut Vec<u8>) -> (usize, usize) {
        match self {
            Console::Nes(nes) => {
                out.resize(nes::WIDTH * nes::HEIGHT, 0);
                nes.frame_layers(out);
                (nes::WIDTH, nes::HEIGHT)
            }
            Console::Md(md) => md.frame_layers(out),
        }
    }

    /// Display only: while on, every finished picture also keeps its planes apart, which
    /// [`Console::frame_tilted`] needs. Not emulated state; loading a state leaves it on.
    pub fn set_tilt(&mut self, on: bool) {
        match self {
            Console::Nes(nes) => nes.set_tilt(on),
            Console::Md(md) => md.set_tilt(on),
        }
    }

    /// Farthest the far plane moves in [`Console::frame_tilted`], in pixels across and down,
    /// for the last finished picture. The tilted picture is at least this much smaller on
    /// every side.
    pub fn tilt_reach(&self) -> (i32, i32) {
        match self {
            Console::Nes(_) => (nes::TILT_REACH.0 as i32, nes::TILT_REACH.1 as i32),
            Console::Md(md) => md.tilt_reach(),
        }
    }

    /// The last finished picture with its far plane moved `shift` pixels (right, down; each
    /// taken within ±[`Console::tilt_reach`]) and the planes between it and the sprites
    /// less; the sprites stay. Cropped so that no shift shows past an edge; returns the
    /// size. `rgba` and `layers` are filled as by [`Console::frame_rgba`] and
    /// [`Console::frame_layers`].
    pub fn frame_tilted(&self, shift: (i32, i32), rgba: &mut Vec<u8>, layers: &mut Vec<u8>) -> (usize, usize) {
        match self {
            Console::Nes(nes) => nes.frame_tilted(shift, rgba, layers),
            Console::Md(md) => md.frame_tilted(shift, rgba, layers),
        }
    }

    /// Output sample rate in Hz. May be nudged every frame to keep the audio queue level.
    pub fn set_sample_rate(&mut self, hz: f64) {
        match self {
            Console::Nes(nes) => nes.set_sample_rate(hz),
            Console::Md(md) => md.set_sample_rate(hz),
        }
    }

    /// Appends the samples produced since the last drain: interleaved stereo (left, right),
    /// range about -1.0..1.0. `scratch` is a reusable buffer.
    pub fn drain_audio(&mut self, out: &mut Vec<f32>, scratch: &mut Vec<f32>) {
        match self {
            Console::Nes(nes) => {
                scratch.clear();
                nes.drain_audio(scratch);
                out.extend(scratch.iter().flat_map(|s| [*s, *s]));
            }
            Console::Md(md) => md.drain_audio(out),
        }
    }

    /// Drops buffered samples (rollback re-simulation must not be heard twice).
    pub fn clear_audio(&mut self) {
        match self {
            Console::Nes(nes) => nes.clear_audio(),
            Console::Md(md) => md.clear_audio(),
        }
    }

    /// Fast-forward: emulation runs as usual, but no pixels are drawn and no samples are
    /// produced. Not part of the snapshot or the hash.
    pub fn set_fast_forward(&mut self, on: bool) {
        match self {
            Console::Nes(nes) => nes.set_fast_forward(on),
            Console::Md(md) => md.set_fast_forward(on),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        match self {
            Console::Nes(nes) => Snapshot::Nes(nes.snapshot()),
            Console::Md(md) => Snapshot::Md(md.snapshot()),
        }
    }

    /// Restores a snapshot taken from this console. One from another system is ignored.
    pub fn restore(&mut self, snapshot: &Snapshot) {
        match (self, snapshot) {
            (Console::Nes(nes), Snapshot::Nes(s)) => nes.restore(s),
            (Console::Md(md), Snapshot::Md(s)) => md.restore(s),
            _ => {}
        }
    }

    /// Stable hash of the emulated state, identical across machines for identical state.
    pub fn state_hash(&self) -> u64 {
        match self {
            Console::Nes(nes) => nes.state_hash(),
            Console::Md(md) => md.state_hash(),
        }
    }

    /// Battery-backed RAM, if the cartridge has it.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        match self {
            Console::Nes(nes) => nes.battery_ram(),
            Console::Md(md) => md.battery_ram(),
        }
    }

    pub fn load_battery_ram(&mut self, data: &[u8]) {
        match self {
            Console::Nes(nes) => nes.load_battery_ram(data),
            Console::Md(md) => md.load_battery_ram(data),
        }
    }

    /// Serialized machine state (a save state), without the ROM.
    pub fn save_state(&self) -> Vec<u8> {
        match self {
            Console::Nes(nes) => nes.save_state(),
            Console::Md(md) => md.save_state(),
        }
    }

    /// Serializes `snapshot` (taken from this console) like [`Console::save_state`]. One
    /// from another system gives an empty blob.
    pub fn encode_state(&self, snapshot: &Snapshot) -> Vec<u8> {
        match (self, snapshot) {
            (Console::Nes(nes), Snapshot::Nes(s)) => nes.encode_state(s),
            (Console::Md(md), Snapshot::Md(s)) => md.encode_state(s),
            _ => Vec::new(),
        }
    }

    /// Loads a [`Console::save_state`] blob. On any error the running machine is left
    /// unchanged.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), StateError> {
        match self {
            Console::Nes(nes) => nes.load_state(bytes).map_err(StateError::from),
            Console::Md(md) => md.load_state(bytes).map_err(StateError::from),
        }
    }
}
