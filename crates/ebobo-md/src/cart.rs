//! ROM image, header, battery RAM, bank switching. Owner: cart slice.
//!
//! Contract with `system.rs`: [`Cart`] is the immutable part (ROM bytes and what the header
//! says), shared by every snapshot; [`CartState`] is the emulated state (battery RAM, bank
//! registers) and lives in the snapshot.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Region;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RomError {
    /// No "SEGA" where a Mega Drive header has it.
    NotMegaDrive,
    /// Shorter than a header, or than a copier header claims.
    Truncated,
    Invalid(&'static str),
}

impl fmt::Display for RomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RomError::NotMegaDrive => write!(f, "not a Mega Drive ROM"),
            RomError::Truncated => write!(f, "the ROM file is cut short"),
            RomError::Invalid(what) => write!(f, "bad ROM: {what}"),
        }
    }
}

impl std::error::Error for RomError {}

/// What the header says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RomInfo {
    /// CRC-32 (IEEE) of the plain ROM image: a `.smd` copy of a game has the CRC of its
    /// `.bin`.
    pub crc32: u32,
    /// Overseas name from the header, trimmed (the domestic one when that is empty).
    pub title: String,
    /// Serial, e.g. "GM 00001009-00".
    pub product: String,
    /// Regions the game says it runs on.
    pub regions: Vec<Region>,
    /// The region the console starts as: the first of Americas, Europe, Japan that the game
    /// supports (Americas when the header names none).
    pub region: Region,
    /// The cartridge has battery-backed RAM.
    pub battery: bool,
}

/// True when `bytes` look like a Mega Drive ROM: a plain image (`.md`, `.gen`, `.bin`) or a
/// copier's interleaved one (`.smd`).
pub fn identify(_bytes: &[u8]) -> bool {
    todo!("owner: periph slice")
}

/// Reads the header of a plain or interleaved image.
pub fn parse(_bytes: &[u8]) -> Result<RomInfo, RomError> {
    todo!("owner: periph slice")
}

/// CRC-32 (IEEE 802.3, as zip and PNG use) over the concatenation of `parts`.
pub fn crc32(_parts: &[&[u8]]) -> u32 {
    todo!("owner: periph slice")
}

/// The cartridge as built: ROM and header. Cloning is cheap (the ROM is shared).
#[derive(Clone)]
pub struct Cart {
    _todo: (),
}

/// The cartridge's emulated state: battery RAM and bank registers.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct CartState {
    _todo: (),
}

impl Cart {
    /// `region` overrides the header's choice (`None` = `RomInfo::region`).
    pub fn new(_bytes: &[u8], _region: Option<Region>) -> Result<Cart, RomError> {
        todo!("owner: periph slice")
    }

    pub fn info(&self) -> &RomInfo {
        todo!("owner: periph slice")
    }

    /// State at power-on: battery RAM filled with 0xFF, default banks.
    pub fn power_on(&self) -> CartState {
        todo!("owner: periph slice")
    }

    /// 68000 read in the cartridge area, `addr` = 0x000000-0x3FFFFF (ROM, or battery RAM
    /// where it is mapped and enabled). Past the end of the ROM reads give 0xFF.
    pub fn read8(&self, _state: &CartState, _addr: u32) -> u8 {
        todo!("owner: periph slice")
    }

    /// `addr` is even.
    pub fn read16(&self, _state: &CartState, _addr: u32) -> u16 {
        todo!("owner: periph slice")
    }

    pub fn write8(&self, _state: &mut CartState, _addr: u32, _val: u8) {
        todo!("owner: periph slice")
    }

    /// `addr` is even.
    pub fn write16(&self, _state: &mut CartState, _addr: u32, _val: u16) {
        todo!("owner: periph slice")
    }

    /// Byte write to a cartridge control register, `addr` = 0xA13000-0xA130FF (battery RAM
    /// enable at 0xA130F1, bank registers at 0xA130F3-0xA130FF).
    pub fn write_reg(&self, _state: &mut CartState, _addr: u32, _val: u8) {
        todo!("owner: periph slice")
    }
}

impl CartState {
    /// Battery-backed RAM, if the cartridge has it.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        todo!("owner: periph slice")
    }

    /// Loads a saved battery RAM image (a shorter or longer one is fitted).
    pub fn load_battery_ram(&mut self, _data: &[u8]) {
        todo!("owner: periph slice")
    }
}
