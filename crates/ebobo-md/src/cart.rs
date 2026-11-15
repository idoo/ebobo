//! ROM image, header, battery RAM, bank switching. Owner: cart slice.
//!
//! Contract with `system.rs`: [`Cart`] is the immutable part (ROM bytes and what the header
//! says), shared by every snapshot; [`CartState`] is the emulated state (battery RAM, bank
//! registers) and lives in the snapshot.
//!
//! Two file formats are read. A plain image (`.md`, `.gen`, `.bin`) is the ROM as the 68000
//! sees it, with "SEGA" at 0x100 or 0x101. A copier image (`.smd`) is a 512-byte header
//! followed by 16 KB blocks, each holding the odd bytes of 16 KB of ROM in its first half and
//! the even bytes in its second half.
//!
//! Serial EEPROM saves (header RAM type 0xE8) are not emulated: such a game is treated as
//! having no save memory.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Region;

/// Length of the vectors and the header: the shortest image that can be parsed.
const HEADER_LEN: usize = 0x200;
/// Length of a copier header.
const SMD_HEADER: usize = 512;
/// Length of one interleaved block of a copier image.
const SMD_BLOCK: usize = 0x4000;
/// One page of the bank mapper.
const PAGE: usize = 0x8_0000;
/// The cartridge area of the 68000 address space is 0x000000-0x3FFFFF.
const ADDR_MASK: u32 = 0x3F_FFFF;
/// The bank registers hold 6 bits: 64 pages of 512 KB.
const MAX_ROM: usize = 64 * PAGE;
/// ROMs longer than this have the bank mapper.
const MAPPER_ABOVE: usize = 0x40_0000;

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
    /// Overseas name from the header, trimmed (the domestic one when that is empty). Runs of
    /// spaces inside the name are folded into one; bytes outside printable ASCII read '?'.
    pub title: String,
    /// Serial, e.g. "GM 00001009-00".
    pub product: String,
    /// Regions the game says it runs on, in the order Americas, Europe, Japan.
    pub regions: Vec<Region>,
    /// The region the console starts as: the first of Americas, Europe, Japan that the game
    /// supports (Americas when the header names none).
    pub region: Region,
    /// The cartridge has battery-backed RAM.
    pub battery: bool,
}

/// True when `bytes` look like a Mega Drive ROM: a plain image (`.md`, `.gen`, `.bin`) or a
/// copier's interleaved one (`.smd`).
pub fn identify(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN && (has_sega(bytes) || is_smd(bytes))
}

/// Reads the header of a plain or interleaved image.
pub fn parse(bytes: &[u8]) -> Result<RomInfo, RomError> {
    Ok(header(&plain_image(bytes)?).0)
}

/// CRC-32 (IEEE 802.3, as zip and PNG use) over the concatenation of `parts`.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for &byte in *part {
            crc = CRC_TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

/// The byte-at-a-time table of the reflected polynomial 0xEDB88320.
const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut crc = n as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            bit += 1;
        }
        table[n] = crc;
        n += 1;
    }
    table
}

/// "SEGA" at 0x100 or 0x101 of a plain image at least [`HEADER_LEN`] long.
fn has_sega(plain: &[u8]) -> bool {
    &plain[0x100..0x104] == b"SEGA" || &plain[0x101..0x105] == b"SEGA"
}

/// Byte `i` (below [`SMD_BLOCK`]) of the ROM held in the first block of a copier image.
fn smd_byte(bytes: &[u8], i: usize) -> u8 {
    let half = if i & 1 == 0 { SMD_BLOCK / 2 } else { 0 };
    bytes[SMD_HEADER + half + (i >> 1)]
}

/// A copier image: at least one whole block whose decoded header has "SEGA".
fn is_smd(bytes: &[u8]) -> bool {
    if bytes.len() < SMD_HEADER + SMD_BLOCK {
        return false;
    }
    let mut head = [0u8; 5];
    for (i, byte) in head.iter_mut().enumerate() {
        *byte = smd_byte(bytes, 0x100 + i);
    }
    &head[..4] == b"SEGA" || &head[1..] == b"SEGA"
}

/// The plain ROM held in a copier image (the caller checked [`is_smd`]).
fn decode_smd(bytes: &[u8]) -> Result<Vec<u8>, RomError> {
    let body = &bytes[SMD_HEADER..];
    // Byte 0 of the copier header counts the blocks (it wraps to 0 at 4 MB and many files leave it 0).
    if body.len() % SMD_BLOCK != 0 || bytes[0] as usize > body.len() / SMD_BLOCK {
        return Err(RomError::Truncated);
    }
    let mut plain = vec![0u8; body.len()];
    for (block, out) in body.chunks_exact(SMD_BLOCK).zip(plain.chunks_exact_mut(SMD_BLOCK)) {
        let (odd, even) = block.split_at(SMD_BLOCK / 2);
        for (pair, (&e, &o)) in out.chunks_exact_mut(2).zip(even.iter().zip(odd)) {
            pair[0] = e;
            pair[1] = o;
        }
    }
    Ok(plain)
}

/// The ROM as the 68000 sees it: `bytes` themselves for a plain image, the decoded blocks for
/// a copier one. At least [`HEADER_LEN`] long.
fn plain_image(bytes: &[u8]) -> Result<Cow<'_, [u8]>, RomError> {
    if bytes.len() < HEADER_LEN {
        return Err(RomError::Truncated);
    }
    let smd = !has_sega(bytes);
    if smd && !is_smd(bytes) {
        return Err(RomError::NotMegaDrive);
    }
    if bytes.len() - if smd { SMD_HEADER } else { 0 } > MAX_ROM {
        return Err(RomError::Invalid("larger than 32 MB"));
    }
    Ok(if smd { Cow::Owned(decode_smd(bytes)?) } else { Cow::Borrowed(bytes) })
}

/// Header text: printable ASCII kept, NUL taken as padding, any other byte shown as '?';
/// trimmed, with runs of spaces folded into one.
fn text(field: &[u8]) -> String {
    let raw: String = field
        .iter()
        .map(|&b| match b {
            0 => ' ',
            0x20..=0x7E => b as char,
            _ => '?',
        })
        .collect();
    raw.split(' ').filter(|word| !word.is_empty()).collect::<Vec<_>>().join(" ")
}

/// The regions named by the three bytes at 0x1F0, in the order Americas, Europe, Japan. Two
/// conventions exist: the letters J, U and E in any position, or one hex digit in the first
/// position as a bit mask (bit 0 Japan, bit 2 Americas, bit 3 Europe). A lone 'E' is the
/// letter, as every game that has it is a European release.
fn regions(codes: &[u8]) -> Vec<Region> {
    let has = |letter: u8| codes.iter().any(|c| c.to_ascii_uppercase() == letter);
    let (mut japan, mut americas, mut europe) = (has(b'J'), has(b'U'), has(b'E'));
    if !(japan || americas || europe) {
        if let Some(mask) = (codes[0] as char).to_digit(16) {
            japan = mask & 1 != 0;
            americas = mask & 4 != 0;
            europe = mask & 8 != 0;
        }
    }
    let mut list = Vec::with_capacity(3);
    for (present, region) in [(americas, Region::Americas), (europe, Region::Europe), (japan, Region::Japan)] {
        if present {
            list.push(region);
        }
    }
    list
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Which bytes of the address range hold RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RamWidth {
    /// 8-bit RAM on the even (upper) byte of each word.
    Even,
    /// 8-bit RAM on the odd (lower) byte of each word: the common kind.
    Odd,
    /// 16-bit RAM: every byte.
    Word,
}

/// Where the cartridge RAM sits, from the header's "RA" block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RamMap {
    /// First and last address holding a RAM byte (both of the RAM's parity for 8-bit RAM).
    start: u32,
    end: u32,
    width: RamWidth,
    /// The range overlaps ROM: bit 0 of register 0xA130F1 switches the RAM in and bit 1
    /// write-protects it. Otherwise the RAM is always visible and writable.
    gated: bool,
    battery: bool,
}

impl RamMap {
    /// Reads the "RA" block at 0x1B0 of a plain image. `None` for no block, a serial EEPROM,
    /// or a range that makes no sense (end before start, outside the cartridge area, more
    /// than 64 KB of addresses).
    fn from_header(plain: &[u8]) -> Option<RamMap> {
        let kind = plain[0x1B2];
        if &plain[0x1B0..0x1B2] != b"RA" || kind & 0x80 == 0 {
            return None;
        }
        let width = match (kind >> 3) & 3 {
            0 => RamWidth::Word,
            2 => RamWidth::Even,
            3 => RamWidth::Odd,
            _ => return None,
        };
        let (mut start, mut end) = (be32(plain, 0x1B4), be32(plain, 0x1B8));
        if end < start || end > ADDR_MASK {
            return None;
        }
        // Headers are sloppy about the low bit: bring both ends inward to the RAM's own bytes.
        match width {
            RamWidth::Even => {
                start = (start + 1) & !1;
                end &= !1;
            }
            RamWidth::Odd => {
                start |= 1;
                end = end.wrapping_sub(1) | 1;
            }
            RamWidth::Word => {
                start &= !1;
                end |= 1;
            }
        }
        if end < start || end - start > 0xFFFF {
            return None;
        }
        Some(RamMap { start, end, width, gated: plain.len() > start as usize, battery: kind & 0x40 != 0 })
    }

    /// Bytes of RAM.
    fn len(&self) -> usize {
        let span = (self.end - self.start) as usize;
        match self.width {
            RamWidth::Word => span + 1,
            RamWidth::Even | RamWidth::Odd => span / 2 + 1,
        }
    }

    /// Index of the RAM byte at `addr` (inside `start..=end`); `None` on the byte lane the
    /// RAM is not wired to.
    fn index(&self, addr: u32) -> Option<usize> {
        let offset = addr - self.start;
        match self.width {
            RamWidth::Word => Some(offset as usize),
            RamWidth::Even | RamWidth::Odd => (offset & 1 == 0).then_some((offset >> 1) as usize),
        }
    }
}

/// Header fields of a plain image at least [`HEADER_LEN`] long, and its RAM.
fn header(plain: &[u8]) -> (RomInfo, Option<RamMap>) {
    let overseas = text(&plain[0x150..0x180]);
    let title = if overseas.is_empty() { text(&plain[0x120..0x150]) } else { overseas };
    let regions = regions(&plain[0x1F0..0x1F3]);
    let ram = RamMap::from_header(plain);
    let info = RomInfo {
        crc32: crc32(&[plain]),
        title,
        product: text(&plain[0x180..0x18E]),
        region: regions.first().copied().unwrap_or(Region::Americas),
        regions,
        battery: ram.is_some_and(|ram| ram.battery),
    };
    (info, ram)
}

/// The cartridge as built: ROM and header. Cloning is cheap (the ROM is shared).
#[derive(Clone)]
pub struct Cart {
    /// The plain image, padded with 0xFF to an even length.
    rom: Arc<[u8]>,
    info: RomInfo,
    ram: Option<RamMap>,
    /// The ROM is longer than 4 MB: the bank registers are live.
    mapper: bool,
}

/// The cartridge's emulated state: battery RAM and bank registers.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub struct CartState {
    /// Cartridge RAM, battery-backed or not; empty when the cartridge has none.
    ram: Vec<u8>,
    /// `ram` is kept by a battery.
    battery: bool,
    /// Last write to 0xA130F1: bit 0 = RAM visible, bit 1 = RAM write-protected.
    ram_reg: u8,
    /// The 512 KB ROM page seen at each 512 KB of the cartridge area. Entry 0 is fixed.
    banks: [u8; 8],
}

impl Cart {
    /// `region` overrides the header's choice (`None` = `RomInfo::region`).
    pub fn new(bytes: &[u8], region: Option<Region>) -> Result<Cart, RomError> {
        let plain = plain_image(bytes)?;
        let (mut info, ram) = header(&plain);
        if let Some(region) = region {
            info.region = region;
        }
        let mut rom = plain.into_owned();
        if rom.len() % 2 != 0 {
            rom.push(0xFF);
        }
        let mapper = rom.len() > MAPPER_ABOVE;
        Ok(Cart { rom: rom.into(), info, ram, mapper })
    }

    pub fn info(&self) -> &RomInfo {
        &self.info
    }

    /// State at power-on: battery RAM filled with 0xFF, default banks.
    pub fn power_on(&self) -> CartState {
        CartState {
            ram: vec![0xFF; self.ram.map_or(0, |ram| ram.len())],
            battery: self.info.battery,
            ram_reg: 0,
            banks: [0, 1, 2, 3, 4, 5, 6, 7],
        }
    }

    /// The RAM mapping when `addr` is inside it and the RAM is switched in.
    fn ram_at(&self, state: &CartState, addr: u32) -> Option<RamMap> {
        self.ram.filter(|ram| ram.start <= addr && addr <= ram.end && (!ram.gated || state.ram_reg & 1 != 0))
    }

    /// 68000 read in the cartridge area, `addr` = 0x000000-0x3FFFFF (ROM, or battery RAM
    /// where it is mapped and enabled). Past the end of the ROM reads give 0xFF, and so does
    /// the unwired byte lane of 8-bit RAM.
    pub fn read8(&self, state: &CartState, addr: u32) -> u8 {
        let addr = addr & ADDR_MASK;
        if let Some(ram) = self.ram_at(state, addr) {
            return ram.index(addr).and_then(|i| state.ram.get(i)).copied().unwrap_or(0xFF);
        }
        let offset = if self.mapper {
            ((state.banks[(addr >> 19) as usize] as usize) << 19) | (addr as usize & (PAGE - 1))
        } else {
            addr as usize
        };
        self.rom.get(offset).copied().unwrap_or(0xFF)
    }

    /// `addr` is even. The two bytes, big-endian.
    pub fn read16(&self, state: &CartState, addr: u32) -> u16 {
        u16::from_be_bytes([self.read8(state, addr), self.read8(state, addr | 1)])
    }

    /// Writes reach only RAM that is switched in and not write-protected; ROM ignores them.
    pub fn write8(&self, state: &mut CartState, addr: u32, val: u8) {
        let addr = addr & ADDR_MASK;
        let Some(ram) = self.ram_at(state, addr) else { return };
        if ram.gated && state.ram_reg & 2 != 0 {
            return;
        }
        if let Some(byte) = ram.index(addr).and_then(|i| state.ram.get_mut(i)) {
            *byte = val;
        }
    }

    /// `addr` is even.
    pub fn write16(&self, state: &mut CartState, addr: u32, val: u16) {
        let [high, low] = val.to_be_bytes();
        self.write8(state, addr, high);
        self.write8(state, addr | 1, low);
    }

    /// Byte write to a cartridge control register, `addr` = 0xA13000-0xA130FF (battery RAM
    /// enable at 0xA130F1, bank registers at 0xA130F3-0xA130FF). The bank registers exist
    /// only on ROMs longer than 4 MB: register 0xA130F3 + 2n picks the 512 KB page seen at
    /// 0x080000 * (n + 1).
    pub fn write_reg(&self, state: &mut CartState, addr: u32, val: u8) {
        match addr & 0xFF {
            0xF1 if self.ram.is_some() => state.ram_reg = val & 3,
            low @ 0xF3..=0xFF if low & 1 != 0 && self.mapper => state.banks[((low - 0xF1) >> 1) as usize] = val & 0x3F,
            _ => {}
        }
    }
}

impl CartState {
    /// Battery-backed RAM, if the cartridge has it.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.battery.then_some(self.ram.as_slice())
    }

    /// Loads a saved battery RAM image (a shorter or longer one is fitted): the RAM keeps its
    /// size, a short image leaves the rest 0xFF, the excess of a long one is dropped. Does
    /// nothing on a cartridge without a battery.
    pub fn load_battery_ram(&mut self, data: &[u8]) {
        if !self.battery {
            return;
        }
        let kept = data.len().min(self.ram.len());
        self.ram[..kept].copy_from_slice(&data[..kept]);
        self.ram[kept..].fill(0xFF);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use super::*;

    fn put(rom: &mut [u8], at: usize, bytes: &[u8]) {
        rom[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// A made-up ROM of `len` bytes: a position-dependent pattern and a tidy header.
    fn image(len: usize) -> Vec<u8> {
        let mut rom: Vec<u8> = (0..len).map(|i| (i ^ (i >> 8) ^ (i >> 16)) as u8).collect();
        rom[0x100..0x200].fill(b' ');
        put(&mut rom, 0x100, b"SEGA MEGA DRIVE ");
        put(&mut rom, 0x120, b"DOMESTIC NAME");
        put(&mut rom, 0x150, b"OVERSEAS      NAME");
        put(&mut rom, 0x180, b"GM 00001009-00");
        put(&mut rom, 0x1F0, b"JUE");
        rom
    }

    /// `image` with an "RA" block.
    fn image_with_ram(len: usize, kind: u8, start: u32, end: u32) -> Vec<u8> {
        let mut rom = image(len);
        put(&mut rom, 0x1B0, &[b'R', b'A', kind, 0x20]);
        put(&mut rom, 0x1B4, &start.to_be_bytes());
        put(&mut rom, 0x1B8, &end.to_be_bytes());
        rom
    }

    /// The copier's file for a plain image whose length is a multiple of 16 KB.
    fn interleave(plain: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; SMD_HEADER];
        out[0] = (plain.len() / SMD_BLOCK) as u8;
        out[1] = 3;
        out[8] = 0xAA;
        out[9] = 0xBB;
        for block in plain.chunks(SMD_BLOCK) {
            out.extend(block.iter().skip(1).step_by(2));
            out.extend(block.iter().step_by(2));
        }
        out
    }

    fn hash_of(state: &CartState) -> u64 {
        let mut hasher = DefaultHasher::new();
        state.hash(&mut hasher);
        hasher.finish()
    }

    fn cart(rom: &[u8]) -> (Cart, CartState) {
        let cart = Cart::new(rom, None).ok().expect("a valid image");
        let state = cart.power_on();
        (cart, state)
    }

    #[test]
    fn crc32_is_ieee() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"1234", b"", b"56789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[]), 0);
    }

    #[test]
    fn both_formats_identify_with_one_crc() {
        let plain = image(0x8000);
        let smd = interleave(&plain);
        assert_eq!(smd.len(), SMD_HEADER + 0x8000);
        assert!(identify(&plain));
        assert!(identify(&smd));
        let (a, b) = (parse(&plain).unwrap(), parse(&smd).unwrap());
        assert_eq!(a, b);
        assert_eq!(a.crc32, crc32(&[&plain]));

        let (from_plain, state) = cart(&plain);
        let (from_smd, _) = cart(&smd);
        for addr in (0..0x8000).step_by(0x1F3).chain([0x100, 0x101, 0x3FFF, 0x4000, 0x7FFF]) {
            assert_eq!(from_plain.read8(&state, addr), plain[addr as usize], "plain {addr:#x}");
            assert_eq!(from_smd.read8(&state, addr), plain[addr as usize], "smd {addr:#x}");
        }
    }

    #[test]
    fn sega_one_byte_late_is_accepted() {
        let mut plain = image(0x4000);
        put(&mut plain, 0x100, b" SEGA GENESIS   ");
        assert!(identify(&plain));
        assert!(identify(&interleave(&plain)));
        assert_eq!(parse(&interleave(&plain)).unwrap().crc32, crc32(&[&plain]));
    }

    #[test]
    fn header_fields() {
        let mut rom = image(0x400);
        let info = parse(&rom).unwrap();
        assert_eq!(info.title, "OVERSEAS NAME");
        assert_eq!(info.product, "GM 00001009-00");
        assert!(!info.battery);

        rom[0x150..0x180].fill(b' ');
        assert_eq!(parse(&rom).unwrap().title, "DOMESTIC NAME");

        rom[0x150..0x180].fill(0);
        put(&mut rom, 0x150, &[b'A', 0x83, b'B', 0x7F, 0x1F, b'~']);
        assert_eq!(parse(&rom).unwrap().title, "A?B??~");
    }

    #[test]
    fn region_codes_in_both_conventions() {
        use Region::{Americas, Europe, Japan};
        let cases: [(&[u8; 3], &[Region], Region); 16] = [
            (b"JUE", &[Americas, Europe, Japan], Americas),
            (b"EUJ", &[Americas, Europe, Japan], Americas),
            (b"U  ", &[Americas], Americas),
            (b"E  ", &[Europe], Europe),
            (b"J  ", &[Japan], Japan),
            (b" U ", &[Americas], Americas),
            (b"  E", &[Europe], Europe),
            (b"JE ", &[Europe, Japan], Europe),
            (b"4  ", &[Americas], Americas),
            (b"8  ", &[Europe], Europe),
            (b"1  ", &[Japan], Japan),
            (b"5  ", &[Americas, Japan], Americas),
            (b"A\0\0", &[Europe], Europe),
            (b"F  ", &[Americas, Europe, Japan], Americas),
            (b"   ", &[], Americas),
            (b"\0\0\0", &[], Americas),
        ];
        for (code, regions, region) in cases {
            let mut rom = image(0x200);
            put(&mut rom, 0x1F0, code);
            let info = parse(&rom).unwrap();
            assert_eq!(info.regions, regions, "{code:?}");
            assert_eq!(info.region, region, "{code:?}");
        }
    }

    #[test]
    fn region_override() {
        let rom = image(0x200);
        assert!(Cart::new(&rom, None).ok().unwrap().info().region == Region::Americas);
        let japan = Cart::new(&rom, Some(Region::Japan)).ok().unwrap();
        assert_eq!(japan.info().region, Region::Japan);
        assert_eq!(japan.info().regions.len(), 3);
    }

    #[test]
    fn odd_ram_outside_the_rom_is_always_visible() {
        let rom = image_with_ram(0x1_0000, 0xF8, 0x20_0001, 0x20_3FFF);
        let (cart, mut state) = cart(&rom);
        assert!(cart.info().battery);
        assert_eq!(state.battery_ram().unwrap().len(), 0x2000);
        assert!(state.battery_ram().unwrap().iter().all(|&b| b == 0xFF));

        cart.write8(&mut state, 0x20_0001, 0x12);
        cart.write8(&mut state, 0x20_0000, 0x99);
        cart.write16(&mut state, 0x20_3FFE, 0xABCD);
        assert_eq!(cart.read8(&state, 0x20_0001), 0x12);
        assert_eq!(cart.read8(&state, 0x20_0000), 0xFF);
        assert_eq!(cart.read16(&state, 0x20_0000), 0xFF12);
        assert_eq!(cart.read16(&state, 0x20_3FFE), 0xFFCD);
        assert_eq!(state.battery_ram().unwrap()[0], 0x12);
        assert_eq!(state.battery_ram().unwrap()[0x1FFF], 0xCD);

        // Past the RAM nothing is there, and the register does not hide ungated RAM.
        cart.write8(&mut state, 0x20_4001, 0x55);
        assert_eq!(cart.read8(&state, 0x20_4001), 0xFF);
        cart.write_reg(&mut state, 0xA1_30F1, 2);
        cart.write8(&mut state, 0x20_0003, 0x34);
        assert_eq!(cart.read8(&state, 0x20_0003), 0x34);
    }

    #[test]
    fn sloppy_even_start_for_odd_ram() {
        let rom = image_with_ram(0x1_0000, 0xF8, 0x20_0000, 0x20_FFFF);
        let (cart, mut state) = cart(&rom);
        assert_eq!(state.battery_ram().unwrap().len(), 0x8000);
        cart.write8(&mut state, 0x20_FFFF, 0x77);
        assert_eq!(state.battery_ram().unwrap()[0x7FFF], 0x77);
    }

    #[test]
    fn ram_over_rom_needs_the_enable_register() {
        let rom = image_with_ram(0x21_0000, 0xF8, 0x20_0001, 0x20_FFFF);
        let (cart, mut state) = cart(&rom);
        let (addr, rom_byte) = (0x20_0101, rom[0x20_0101]);
        assert_ne!(rom_byte, 0xFF);

        // Switched out: ROM shows, writes go nowhere.
        cart.write8(&mut state, addr, 0x5A);
        assert_eq!(cart.read8(&state, addr), rom_byte);
        assert!(state.battery_ram().unwrap().iter().all(|&b| b == 0xFF));

        cart.write_reg(&mut state, 0xA1_30F1, 1);
        assert_eq!(cart.read8(&state, addr), 0xFF);
        assert_eq!(cart.read8(&state, addr - 1), 0xFF);
        cart.write8(&mut state, addr, 0x5A);
        assert_eq!(cart.read8(&state, addr), 0x5A);
        // Outside the RAM range the ROM is still there.
        assert_eq!(cart.read8(&state, 0x1F_FFFF), rom[0x1F_FFFF]);

        cart.write_reg(&mut state, 0xA1_30F1, 3);
        cart.write8(&mut state, addr, 0x00);
        assert_eq!(cart.read8(&state, addr), 0x5A);

        cart.write_reg(&mut state, 0xA1_30F1, 0);
        assert_eq!(cart.read8(&state, addr), rom_byte);
        assert_eq!(state.battery_ram().unwrap()[0x80], 0x5A);
    }

    #[test]
    fn ram_kinds_and_bad_ranges() {
        // 8-bit odd RAM without a battery works but is not a save.
        let (plain_ram, mut state) = cart(&image_with_ram(0x1_0000, 0xB8, 0x20_0001, 0x20_3FFF));
        assert!(!plain_ram.info().battery);
        assert!(state.battery_ram().is_none());
        plain_ram.write8(&mut state, 0x20_0001, 0x42);
        assert_eq!(plain_ram.read8(&state, 0x20_0001), 0x42);
        state.load_battery_ram(&[0; 16]);
        assert_eq!(plain_ram.read8(&state, 0x20_0001), 0x42);

        // 16-bit RAM takes both bytes.
        let (word, mut state) = cart(&image_with_ram(0x1_0000, 0xE0, 0x20_0000, 0x20_FFFF));
        assert_eq!(state.battery_ram().unwrap().len(), 0x1_0000);
        word.write16(&mut state, 0x20_0010, 0xBEEF);
        assert_eq!(word.read16(&state, 0x20_0010), 0xBEEF);
        assert_eq!(&state.battery_ram().unwrap()[0x10..0x12], &[0xBE, 0xEF]);

        // 8-bit even RAM.
        let (even, mut state) = cart(&image_with_ram(0x1_0000, 0xF0, 0x20_0000, 0x20_3FFE));
        assert_eq!(state.battery_ram().unwrap().len(), 0x2000);
        even.write16(&mut state, 0x20_0000, 0x1234);
        assert_eq!(even.read16(&state, 0x20_0000), 0x12FF);

        for (kind, start, end) in [
            (0xF8, 0x20_3FFF, 0x20_0001),  // end before start
            (0xF8, 0x20_0001, 0x22_0001),  // more than 64 KB
            (0xE0, 0x20_0000, 0x21_0000),  // one word more than 64 KB
            (0xF8, 0x40_0001, 0x40_3FFF),  // outside the cartridge area
            (0xF8, 0x2020_2020, 0x2020_2020), // spaces
            (0xE8, 0x20_0001, 0x20_0001),  // serial EEPROM
            (0x78, 0x20_0001, 0x20_3FFF),  // not a RAM type byte
        ] {
            let (none, mut state) = cart(&image_with_ram(0x1_0000, kind, start, end));
            assert!(!none.info().battery, "{kind:#x} {start:#x} {end:#x}");
            assert!(state.battery_ram().is_none());
            none.write8(&mut state, 0x20_0001, 0x11);
            assert_eq!(none.read8(&state, 0x20_0001), 0xFF);
        }
    }

    #[test]
    fn bank_registers_move_pages() {
        let mut rom = image(0x50_0000);
        for page in 0..10 {
            rom[page * PAGE] = 0xC0 + page as u8;
            rom[page * PAGE + 1] = page as u8;
        }
        let (cart, mut state) = cart(&rom);
        // Power-on is the identity; past the 4 MB window nothing is reachable yet.
        for page in 0..8u32 {
            assert_eq!(cart.read16(&state, page * PAGE as u32), 0xC000 + page as u16 * 0x101);
        }
        assert_eq!(cart.read8(&state, 0x12_3457), rom[0x12_3457]);

        cart.write_reg(&mut state, 0xA1_30F3, 9);
        assert_eq!(cart.read16(&state, 0x08_0000), 0xC909);
        assert_eq!(cart.read8(&state, 0x08_1234), rom[9 * PAGE + 0x1234]);
        cart.write_reg(&mut state, 0xA1_30FF, 2);
        assert_eq!(cart.read8(&state, 0x38_0000), 0xC2);
        assert_eq!(cart.read8(&state, 0x10_0000), 0xC2);
        // A page past the end of the ROM, and the even address between two registers.
        cart.write_reg(&mut state, 0xA1_30F5, 10);
        assert_eq!(cart.read16(&state, 0x10_0000), 0xFFFF);
        cart.write_reg(&mut state, 0xA1_30F4, 5);
        assert_eq!(cart.read16(&state, 0x08_0000), 0xC909);
        // Page 0 stays, ROM writes do nothing.
        cart.write_reg(&mut state, 0xA1_30F1, 7);
        cart.write16(&mut state, 0x08_0000, 0x1234);
        assert_eq!(cart.read8(&state, 0x00_0000), rom[0]);
        assert_eq!(cart.read16(&state, 0x08_0000), 0xC909);

        // A 4 MB ROM has no mapper.
        let rom = image(0x40_0000);
        let (cart, mut state) = super::tests::cart(&rom);
        let before = hash_of(&state);
        cart.write_reg(&mut state, 0xA1_30F3, 3);
        assert_eq!(hash_of(&state), before);
        assert_eq!(cart.read8(&state, 0x08_0000), rom[0x08_0000]);
    }

    #[test]
    fn reads_past_the_rom_and_odd_lengths() {
        let rom = image(0x301);
        let (cart, mut state) = cart(&rom);
        assert_eq!(cart.info().crc32, crc32(&[&rom]));
        assert_eq!(cart.read8(&state, 0x300), rom[0x300]);
        assert_eq!(cart.read8(&state, 0x301), 0xFF);
        assert_eq!(cart.read16(&state, 0x300), u16::from_be_bytes([rom[0x300], 0xFF]));
        assert_eq!(cart.read16(&state, 0x3F_FFFE), 0xFFFF);
        assert_eq!(cart.read16(&state, 0x102), u16::from_be_bytes([rom[0x102], rom[0x103]]));
        cart.write16(&mut state, 0x102, 0);
        cart.write8(&mut state, 0x3F_FFFF, 0);
        assert_eq!(cart.read8(&state, 0x102), rom[0x102]);
    }

    #[test]
    fn load_battery_ram_fits_short_and_long_images() {
        let (cart, mut state) = cart(&image_with_ram(0x1_0000, 0xF8, 0x20_0001, 0x20_00FF));
        assert_eq!(state.battery_ram().unwrap().len(), 0x80);
        cart.write8(&mut state, 0x20_00FF, 0x01);

        state.load_battery_ram(&[1, 2, 3]);
        let ram = state.battery_ram().unwrap();
        assert_eq!(ram.len(), 0x80);
        assert_eq!(&ram[..4], &[1, 2, 3, 0xFF]);
        assert_eq!(ram[0x7F], 0xFF);
        assert_eq!(cart.read8(&state, 0x20_0005), 3);

        let long: Vec<u8> = (0..0x200).map(|i| (i / 2) as u8).collect();
        state.load_battery_ram(&long);
        assert_eq!(state.battery_ram().unwrap(), &long[..0x80]);

        state.load_battery_ram(&[]);
        assert!(state.battery_ram().unwrap().iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn garbage_and_short_inputs_are_errors() {
        let plain = image(0x8000);
        let smd = interleave(&plain);

        assert_eq!(parse(&[]), Err(RomError::Truncated));
        assert_eq!(parse(&plain[..0x150]), Err(RomError::Truncated));
        assert_eq!(parse(&plain[..0x1FF]), Err(RomError::Truncated));
        assert!(!identify(&plain[..0x1FF]));
        assert_eq!(parse(&[0; 0x200]), Err(RomError::NotMegaDrive));
        assert_eq!(parse(&smd[..SMD_HEADER]), Err(RomError::NotMegaDrive));
        // Cut inside the first block, inside the second, and at a whole block the header counts.
        assert_eq!(parse(&smd[..SMD_HEADER + SMD_BLOCK - 1]), Err(RomError::NotMegaDrive));
        assert!(!identify(&smd[..SMD_HEADER + SMD_BLOCK - 1]));
        assert_eq!(parse(&smd[..smd.len() - 1]), Err(RomError::Truncated));
        assert_eq!(parse(&smd[..SMD_HEADER + SMD_BLOCK]), Err(RomError::Truncated));
        assert_eq!(Cart::new(&smd[..smd.len() - 1], None).err(), Some(RomError::Truncated));
        assert_eq!(Cart::new(&[1, 2, 3], None).err(), Some(RomError::Truncated));
        // A block count of 0 says nothing.
        let mut uncounted = smd.clone();
        uncounted[0] = 0;
        assert_eq!(parse(&uncounted[..SMD_HEADER + SMD_BLOCK]).unwrap().title, "OVERSEAS NAME");

        let mut seed = 0x1234_5678u32;
        for len in [0, 1, 0x100, 0x104, 0x1FF, 0x200, 0x201, 0x2FF, SMD_HEADER + SMD_BLOCK - 1, SMD_HEADER + SMD_BLOCK, 70_001] {
            let noise: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            assert!(!identify(&noise), "{len}");
            assert!(parse(&noise).is_err(), "{len}");
            assert!(Cart::new(&noise, Some(Region::Europe)).is_err(), "{len}");
        }

        // A valid header over noise: every header field is read without a panic.
        let mut seed = 99u32;
        for _ in 0..200 {
            let mut rom = image(0x200);
            for byte in &mut rom[0x104..] {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *byte = (seed >> 24) as u8;
            }
            rom[0x1B0] = b'R';
            rom[0x1B1] = b'A';
            let (cart, mut state) = cart(&rom);
            cart.write16(&mut state, 0x20_0000, 0x1234);
            cart.read16(&state, 0x20_0000);
            cart.read16(&state, 0x3F_FFFE);
            assert_eq!(parse(&rom).unwrap().regions, cart.info().regions);
        }
    }

    #[test]
    fn state_is_plain_data() {
        let (cart, mut state) = cart(&image_with_ram(0x1_0000, 0xF8, 0x20_0001, 0x20_3FFF));
        let fresh = state.clone();
        assert_eq!(hash_of(&fresh), hash_of(&cart.power_on()));
        cart.write8(&mut state, 0x20_0001, 0x12);
        assert_ne!(hash_of(&state), hash_of(&fresh));

        let bytes = bincode::serialize(&state).unwrap();
        let back: CartState = bincode::deserialize(&bytes).unwrap();
        assert_eq!(hash_of(&back), hash_of(&state));
        assert_eq!(cart.read8(&back, 0x20_0001), 0x12);
    }
}
