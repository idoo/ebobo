//! Memory maps of both CPUs, scheduler and the public machine. Owner: system slice.

use std::fmt;

use crate::Region;
use crate::cart::{self, RomError, RomInfo};

/// A Mega Drive with a cartridge in it.
pub struct Md {
    info: RomInfo,
}

/// Complete emulated state. Cloning copies about 200 KB (the ROM is shared; the picture and
/// the audio output are not part of it). Rollback saves one of these every frame.
#[derive(Clone)]
pub struct Snapshot {
    _todo: (),
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

impl Md {
    /// Reads `rom` (a plain or interleaved image) and powers the console on.
    /// `region` overrides the header's choice (`None` = `RomInfo::region`).
    pub fn new(rom: &[u8], _region: Option<Region>) -> Result<Md, RomError> {
        let _ = cart::parse(rom)?;
        Err(RomError::Invalid("the Mega Drive core is not finished"))
    }

    pub fn info(&self) -> &RomInfo {
        &self.info
    }

    /// The region the console runs as.
    pub fn region(&self) -> Region {
        todo!("owner: system slice")
    }

    /// Frames per second of the emulated console: 59.9228 (NTSC) or 49.7015 (PAL).
    pub fn frame_rate(&self) -> f64 {
        todo!("owner: system slice")
    }

    /// Reset button (RAM survives).
    pub fn reset(&mut self) {
        todo!("owner: system slice")
    }

    /// A four-player adapter is plugged in (which one is chosen for the game); without it
    /// only pads 1 and 2 reach the game. Part of the emulated state.
    pub fn set_multitap(&mut self, _on: bool) {
        todo!("owner: system slice")
    }

    pub fn multitap(&self) -> bool {
        todo!("owner: system slice")
    }

    /// Runs until the VDP finishes the last displayed line (vertical blanking starts).
    /// `pads[i]` is controller i+1 (see [`crate::pad`]).
    pub fn run_frame(&mut self, _pads: [u16; 4]) {
        todo!("owner: system slice")
    }

    /// Size of the last finished picture: 256 or 320 wide, 224 or 240 high (448 or 480 in
    /// interlace mode 2).
    pub fn frame_size(&self) -> (usize, usize) {
        todo!("owner: system slice")
    }

    /// Writes the last finished picture as RGBA8888 into `out` (resized to
    /// `width * height * 4` bytes) and returns its size.
    pub fn frame_rgba(&self, _out: &mut Vec<u8>) -> (usize, usize) {
        todo!("owner: system slice")
    }

    /// Output sample rate in Hz. May be nudged every frame to keep the audio queue level.
    pub fn set_sample_rate(&mut self, _hz: f64) {
        todo!("owner: system slice")
    }

    /// Appends the samples produced since the last drain: interleaved stereo (left, right),
    /// range about -1.0..1.0.
    pub fn drain_audio(&mut self, _out: &mut Vec<f32>) {
        todo!("owner: system slice")
    }

    /// Drops buffered samples (rollback re-simulation must not be heard twice).
    pub fn clear_audio(&mut self) {
        todo!("owner: system slice")
    }

    /// Fast-forward: emulation runs as usual, but no pixels are drawn and no samples are
    /// produced. Not part of the snapshot or the hash.
    pub fn set_fast_forward(&mut self, _on: bool) {
        todo!("owner: system slice")
    }

    pub fn snapshot(&self) -> Snapshot {
        todo!("owner: system slice")
    }

    pub fn restore(&mut self, _snapshot: &Snapshot) {
        todo!("owner: system slice")
    }

    /// Stable hash of the emulated state, identical across machines for identical state.
    pub fn state_hash(&self) -> u64 {
        todo!("owner: system slice")
    }

    /// Battery-backed RAM, if the cartridge has it.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        todo!("owner: system slice")
    }

    pub fn load_battery_ram(&mut self, _data: &[u8]) {
        todo!("owner: system slice")
    }

    /// Serializes the machine: magic, version, ROM CRC and region, then the state. The ROM is
    /// not included; load it into an `Md` running the same ROM.
    pub fn save_state(&self) -> Vec<u8> {
        self.encode_state(&self.snapshot())
    }

    /// Serializes `snapshot` (taken from this machine) like [`Md::save_state`].
    pub fn encode_state(&self, _snapshot: &Snapshot) -> Vec<u8> {
        todo!("owner: system slice")
    }

    /// Loads a [`Md::save_state`] blob. On any error the running machine is left unchanged.
    pub fn load_state(&mut self, _bytes: &[u8]) -> Result<(), StateError> {
        todo!("owner: system slice")
    }
}
