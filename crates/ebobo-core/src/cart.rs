//! iNES / NES 2.0 file parsing. Owner: cart+mapper slice.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Region {
    Ntsc,
    Pal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Mirroring {
    Horizontal,
    Vertical,
    SingleLower,
    SingleUpper,
    FourScreen,
}

/// Immutable ROM bytes shared by every snapshot. `Hash` is deliberately a no-op: ROM never
/// changes, and hashing it each frame for desync detection would only burn time.
#[derive(Clone, Debug, Default)]
pub struct RomBytes(pub Arc<[u8]>);

impl Deref for RomBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Hash for RomBytes {
    fn hash<H: Hasher>(&self, _: &mut H) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RomError {
    /// Missing "NES\x1A" magic.
    NotNes,
    /// File shorter than its header claims.
    Truncated,
    UnsupportedMapper(u16),
    Invalid(&'static str),
}

impl fmt::Display for RomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RomError::NotNes => write!(f, "not an iNES file"),
            RomError::Truncated => write!(f, "ROM file is truncated"),
            RomError::UnsupportedMapper(m) => write!(f, "mapper {m} is not supported"),
            RomError::Invalid(why) => write!(f, "invalid ROM: {why}"),
        }
    }
}

impl std::error::Error for RomError {}

#[derive(Clone, Debug)]
pub struct RomInfo {
    pub mapper: u16,
    pub submapper: u8,
    pub prg_rom: RomBytes,
    /// Empty when the board uses CHR RAM.
    pub chr_rom: RomBytes,
    /// CHR RAM size in bytes (8 KiB when `chr_rom` is empty and the header says nothing).
    pub chr_ram_size: usize,
    /// PRG RAM / work RAM at $6000-$7FFF in bytes (8 KiB default for iNES 1.0).
    pub prg_ram_size: usize,
    pub battery: bool,
    /// Header mirroring; `FourScreen` when the four-screen bit is set.
    pub mirroring: Mirroring,
    /// NES 2.0 CPU/PPU timing, else iNES flags; `Ntsc` when unknown.
    pub region: Region,
    /// NES 2.0 default expansion device is a Four Score / Satellite.
    pub four_score: bool,
    /// CRC-32 (IEEE) of PRG ROM followed by CHR ROM and the ESX1 chunk (when present), without
    /// header and trainer. This is the game identity used by netplay and ROM databases.
    pub crc32: u32,
    /// 512-byte trainer mapped at $7000, if present.
    pub trainer: Option<RomBytes>,
    /// ESX1/ESX2 extra sprite banks (`docs/EXTENSIONS.md`), if the file carries the chunk.
    pub esx: Option<Esx>,
}

/// One ESX bank: tiles `first_tile .. first_tile + tile_count`, tile `t` drawn with
/// `palettes[tile_palette[t - first_tile]]`. ESX1 banks have one palette.
#[derive(Clone, Debug)]
pub struct EsxBank {
    /// 1..=4 palettes of NES colour indices for sprite pixel values 1, 2, 3.
    pub palettes: Vec<[u8; 3]>,
    pub first_tile: u8,
    pub tile_count: u8,
    /// `tile_count` palette indices (< `palettes.len()`), one per tile.
    pub tile_palette: RomBytes,
    /// `16 * tile_count` bytes of 2bpp tiles in standard NES layout.
    pub tiles: RomBytes,
}

impl EsxBank {
    /// Single-palette (ESX1) bank.
    pub fn single(palette: [u8; 3], first_tile: u8, tiles: RomBytes) -> EsxBank {
        let tile_count = (tiles.len() / 16) as u8;
        EsxBank {
            palettes: vec![palette],
            first_tile,
            tile_count,
            tile_palette: RomBytes(Arc::from(vec![0u8; tile_count as usize])),
            tiles,
        }
    }
}

/// Parsed ESX1/ESX2 chunk. Constant ROM data, shared by `Arc`, so it hashes to nothing.
#[derive(Clone, Debug)]
pub struct Esx {
    /// The whole chunk, from the magic through "ESXEND!!".
    pub raw: RomBytes,
    /// Bank `b` (1-based in OAM attribute bits 2-4) is `banks[b - 1]`.
    pub banks: Arc<[EsxBank]>,
}

impl Hash for Esx {
    fn hash<H: Hasher>(&self, _: &mut H) {}
}

pub const ESX_MAGIC: &[u8; 4] = b"ESX1";
pub const ESX2_MAGIC: &[u8; 4] = b"ESX2";
pub const ESX_FOOTER: &[u8; 8] = b"ESXEND!!";

impl Esx {
    /// Byte `offset & 15` of tile `tile` from bank `bank` (1-based), if that bank replaces it,
    /// with the tile's global palette id `(bank - 1) * 4 + tile_palette + 1` (1..=28).
    #[inline]
    pub fn tile_byte(&self, bank: u8, tile: u8, offset: u16) -> Option<(u8, u8)> {
        let b = self.banks.get((bank as usize).wrapping_sub(1))?;
        let rel = tile.wrapping_sub(b.first_tile);
        if tile < b.first_tile || rel >= b.tile_count {
            return None;
        }
        let pal = ((bank - 1) << 2) + b.tile_palette[rel as usize] + 1;
        Some((b.tiles[rel as usize * 16 + (offset & 15) as usize], pal))
    }

    /// Serialises banks into a chunk (magic .. footer): ESX2 when any bank has more than one
    /// palette, else ESX1.
    pub fn build(banks: &[EsxBank]) -> Result<Vec<u8>, RomError> {
        if banks.is_empty() || banks.len() > 7 {
            return Err(RomError::Invalid("ESX bank count must be 1..=7"));
        }
        let v2 = banks.iter().any(|b| b.palettes.len() > 1);
        let mut out = Vec::new();
        out.extend_from_slice(if v2 { ESX2_MAGIC } else { ESX_MAGIC });
        out.push(banks.len() as u8);
        for b in banks {
            if b.tile_count == 0 || b.first_tile as usize + b.tile_count as usize > 256 {
                return Err(RomError::Invalid("ESX bank tile range"));
            }
            if b.tiles.len() != 16 * b.tile_count as usize {
                return Err(RomError::Invalid("ESX bank tile data length"));
            }
            if !(1..=4).contains(&b.palettes.len()) {
                return Err(RomError::Invalid("ESX2 palette count must be 1..=4"));
            }
            if b.tile_palette.len() != b.tile_count as usize
                || b.tile_palette.iter().any(|&p| p as usize >= b.palettes.len())
            {
                return Err(RomError::Invalid("ESX2 tile palette index out of range"));
            }
            if v2 {
                out.push(b.palettes.len() as u8);
                for p in &b.palettes {
                    out.extend_from_slice(p);
                }
                out.push(b.first_tile);
                out.push(b.tile_count);
                out.extend_from_slice(&b.tile_palette);
            } else {
                out.extend_from_slice(&b.palettes[0]);
                out.push(b.first_tile);
                out.push(b.tile_count);
            }
            out.extend_from_slice(&b.tiles);
        }
        let total = out.len() as u32;
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(ESX_FOOTER);
        Ok(out)
    }

    /// Detects an ESX1/ESX2 chunk at the end of `bytes`. `Ok(None)` when the footer is absent;
    /// `Err` when the footer is present but the chunk is malformed. On success also returns the
    /// chunk's start offset in `bytes`.
    pub fn detect(bytes: &[u8]) -> Result<Option<(usize, Esx)>, RomError> {
        if bytes.len() < 12 || &bytes[bytes.len() - 8..] != ESX_FOOTER {
            return Ok(None);
        }
        let bad = |why| Err(RomError::Invalid(why));
        let len_at = bytes.len() - 12;
        let total = u32::from_le_bytes(bytes[len_at..len_at + 4].try_into().unwrap()) as usize;
        if total < 5 || total > len_at {
            return bad("ESX total_len out of range");
        }
        let start = len_at - total;
        let body = &bytes[start..len_at];
        let v2 = match &body[..4] {
            m if m == ESX_MAGIC => false,
            m if m == ESX2_MAGIC => true,
            _ => return bad("ESX magic missing"),
        };
        let count = body[4] as usize;
        if !(1..=7).contains(&count) {
            return bad("ESX bank count must be 1..=7");
        }
        let mut off = 5;
        let mut banks = Vec::with_capacity(count);
        for _ in 0..count {
            let palette_count = if v2 {
                let Some(&n) = body.get(off) else { return bad("ESX bank header truncated") };
                off += 1;
                if !(1..=4).contains(&n) {
                    return bad("ESX2 palette count must be 1..=4");
                }
                n as usize
            } else {
                1
            };
            if body.len() < off + 3 * palette_count + 2 {
                return bad("ESX bank header truncated");
            }
            let palettes: Vec<[u8; 3]> = body[off..off + 3 * palette_count]
                .chunks_exact(3)
                .map(|c| [c[0], c[1], c[2]])
                .collect();
            off += 3 * palette_count;
            let first_tile = body[off];
            let tile_count = body[off + 1];
            off += 2;
            if tile_count == 0 || first_tile as usize + tile_count as usize > 256 {
                return bad("ESX bank tile range");
            }
            if palettes.iter().flatten().any(|&c| c > 0x3F) {
                return bad("ESX palette colour above $3F");
            }
            let tile_palette = if v2 {
                let n = tile_count as usize;
                if body.len() < off + n {
                    return bad("ESX2 tile palettes truncated");
                }
                let tp = &body[off..off + n];
                if tp.iter().any(|&p| p as usize >= palette_count) {
                    return bad("ESX2 tile palette index out of range");
                }
                off += n;
                RomBytes(Arc::from(tp))
            } else {
                RomBytes(Arc::from(vec![0u8; tile_count as usize]))
            };
            let n = 16 * tile_count as usize;
            if body.len() < off + n {
                return bad("ESX bank tiles truncated");
            }
            banks.push(EsxBank {
                palettes,
                first_tile,
                tile_count,
                tile_palette,
                tiles: RomBytes(Arc::from(&body[off..off + n])),
            });
            off += n;
        }
        if off != body.len() {
            return bad("ESX total_len does not match banks");
        }
        Ok(Some((start, Esx { raw: RomBytes(Arc::from(&bytes[start..])), banks: banks.into() })))
    }
}

/// CRC-32 (IEEE 802.3, reflected, poly 0xEDB88320) over the concatenation of `parts`.
pub fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &b in *part {
            crc = CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
    }
    !crc
}

static CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// NES 2.0 size field: a MSB nibble of $F switches to exponent-multiplier notation, where the
/// LSB byte is `EEEEEEMM`: size = 2^EEEEEE * (MM * 2 + 1) bytes.
fn nes2_size(lsb: u8, msb: u8, unit: usize) -> Result<usize, RomError> {
    if msb == 0x0F {
        let exp = (lsb >> 2) as u32;
        let mult = (lsb & 3) as u64 * 2 + 1;
        if exp > 40 {
            return Err(RomError::Invalid("exponent ROM size too large"));
        }
        let bytes = (1u64 << exp).saturating_mul(mult);
        usize::try_from(bytes).map_err(|_| RomError::Invalid("exponent ROM size too large"))
    } else {
        Ok((((msb as usize) << 8) | lsb as usize) * unit)
    }
}

/// NES 2.0 RAM nibble: 0 means none, otherwise 64 << n bytes.
fn nes2_ram(nibble: u8) -> usize {
    if nibble == 0 { 0 } else { 64usize << nibble }
}

pub fn parse(bytes: &[u8]) -> Result<RomInfo, RomError> {
    let (bytes, esx) = match Esx::detect(bytes)? {
        Some((start, esx)) => (&bytes[..start], Some(esx)),
        None => (bytes, None),
    };
    if bytes.len() < 16 {
        return Err(RomError::Truncated);
    }
    if &bytes[0..4] != b"NES\x1A" {
        return Err(RomError::NotNes);
    }
    let flags6 = bytes[6];
    let flags7 = bytes[7];
    let is_nes2 = (flags7 >> 2) & 0b11 == 0b10;

    // iNES 1.0 files written by old tools carry junk ("DiskDude!") in bytes 7..16; in that case
    // the upper mapper nibble in byte 7 is garbage and must be ignored.
    let header_junk = !is_nes2 && bytes[12..16].iter().any(|&b| b != 0);

    let mapper: u16 = if is_nes2 {
        ((bytes[8] as u16 & 0x0F) << 8) | ((flags7 as u16 & 0xF0)) | (flags6 as u16 >> 4)
    } else if header_junk {
        flags6 as u16 >> 4
    } else {
        (flags7 as u16 & 0xF0) | (flags6 as u16 >> 4)
    };
    let submapper = if is_nes2 { bytes[8] >> 4 } else { 0 };

    let (prg_size, chr_size) = if is_nes2 {
        (
            nes2_size(bytes[4], bytes[9] & 0x0F, 16 * 1024)?,
            nes2_size(bytes[5], bytes[9] >> 4, 8 * 1024)?,
        )
    } else {
        (bytes[4] as usize * 16 * 1024, bytes[5] as usize * 8 * 1024)
    };
    if prg_size == 0 {
        return Err(RomError::Invalid("PRG ROM size is zero"));
    }

    let battery = flags6 & 0b10 != 0;
    let has_trainer = flags6 & 0b100 != 0;
    let mirroring = if flags6 & 0b1000 != 0 {
        Mirroring::FourScreen
    } else if flags6 & 1 != 0 {
        Mirroring::Vertical
    } else {
        Mirroring::Horizontal
    };

    let prg_ram_size = if is_nes2 {
        let total = nes2_ram(bytes[10] & 0x0F) + nes2_ram(bytes[10] >> 4);
        if total == 0 && battery { 8 * 1024 } else { total }
    } else if bytes[8] == 0 || header_junk {
        8 * 1024
    } else {
        bytes[8] as usize * 8 * 1024
    };

    let chr_ram_size = if is_nes2 {
        nes2_ram(bytes[11] & 0x0F) + nes2_ram(bytes[11] >> 4)
    } else if chr_size == 0 {
        8 * 1024
    } else {
        0
    };

    let region = if is_nes2 {
        // Byte 12 bits 0-1: 0 = NTSC, 1 = PAL, 2 = multi-region, 3 = Dendy.
        match bytes[12] & 0b11 {
            1 | 3 => Region::Pal,
            _ => Region::Ntsc,
        }
    } else if !header_junk && bytes[9] & 1 != 0 {
        Region::Pal
    } else {
        Region::Ntsc
    };

    // NES 2.0 byte 15, Default Expansion Device (https://www.nesdev.org/wiki/NES_2.0
    // #Default_Expansion_Device): $02 = "NES Four Score/Satellite with two additional standard
    // controllers". Other multitap entries ($03 Famicom Four Players Adapter) are Famicom-only.
    let four_score = is_nes2 && (bytes[15] & 0x3F) == 0x02;

    let mut off = 16usize;
    let trainer = if has_trainer {
        if bytes.len() < off + 512 {
            return Err(RomError::Truncated);
        }
        let t = RomBytes(Arc::from(&bytes[off..off + 512]));
        off += 512;
        Some(t)
    } else {
        None
    };

    if bytes.len() < off + prg_size {
        return Err(RomError::Truncated);
    }
    let prg = &bytes[off..off + prg_size];
    off += prg_size;
    if bytes.len() < off + chr_size {
        return Err(RomError::Truncated);
    }
    let chr = &bytes[off..off + chr_size];

    let crc = match &esx {
        Some(e) => crc32(&[prg, chr, &e.raw]),
        None => crc32(&[prg, chr]),
    };

    Ok(RomInfo {
        mapper,
        submapper,
        prg_rom: RomBytes(Arc::from(prg)),
        chr_rom: RomBytes(Arc::from(chr)),
        chr_ram_size: if chr_size == 0 && chr_ram_size == 0 { 8 * 1024 } else { chr_ram_size },
        prg_ram_size,
        battery,
        mirroring,
        region,
        four_score,
        crc32: crc,
        trainer,
        esx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_check_vector() {
        assert_eq!(crc32(&[b"123456789"]), 0xCBF4_3926);
        assert_eq!(crc32(&[b"12345", b"6789"]), 0xCBF4_3926);
    }

    #[test]
    fn nestest_header() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/nestest/nestest.nes");
        let Ok(bytes) = std::fs::read(path) else {
            println!("test ROMs missing: run scripts/fetch-test-roms.sh");
            return;
        };
        let info = parse(&bytes).expect("parse nestest");
        assert_eq!(info.mapper, 0);
        assert_eq!(info.submapper, 0);
        assert_eq!(info.prg_rom.len(), 16 * 1024);
        assert_eq!(info.chr_rom.len(), 8 * 1024);
        assert_eq!(info.region, Region::Ntsc);
        assert!(info.trainer.is_none());
    }

    #[test]
    fn ines1_pal_mmc1_header() {
        // iNES 1.0, mapper 1, 128 KiB PRG + 128 KiB CHR, byte 9 bit 0 = PAL.
        let mut bytes = header(8, 16, 0x10, 0x00);
        bytes[9] = 0x01;
        bytes.resize(16 + 256 * 1024, 0);
        let info = parse(&bytes).expect("parse PAL MMC1");
        assert_eq!(info.mapper, 1);
        assert_eq!(info.prg_rom.len(), 128 * 1024);
        assert_eq!(info.chr_rom.len(), 128 * 1024);
        assert_eq!(info.region, Region::Pal);
    }

    fn header(prg_16k: u8, chr_8k: u8, flags6: u8, flags7: u8) -> Vec<u8> {
        let mut v = vec![0u8; 16];
        v[0..4].copy_from_slice(b"NES\x1A");
        v[4] = prg_16k;
        v[5] = chr_8k;
        v[6] = flags6;
        v[7] = flags7;
        v
    }

    #[test]
    fn ines1_diskdude_upper_nibble_ignored() {
        let mut v = header(1, 0, 0x00, 0x44);
        v[12..16].copy_from_slice(b"ude!");
        v.extend(std::iter::repeat_n(0u8, 16 * 1024));
        let info = parse(&v).unwrap();
        assert_eq!(info.mapper, 0);
        assert_eq!(info.chr_ram_size, 8 * 1024);
    }

    #[test]
    fn nes2_exponent_and_fields() {
        let mut v = header(0b0000_1101, 0, 0b0000_0011, 0b0000_1000);
        v[9] = 0x0F; // PRG MSB nibble = exponent notation
        v[10] = 0x07; // PRG RAM = 64 << 7 = 8 KiB
        v[11] = 0x07; // CHR RAM = 8 KiB
        v[12] = 1; // PAL
        v[15] = 0x02; // Four Score
        let size = (1usize << 3) * (1 * 2 + 1); // EEEEEE=3, MM=1 -> 24 bytes
        v.extend(std::iter::repeat_n(0u8, size));
        let info = parse(&v).unwrap();
        assert_eq!(info.prg_rom.len(), size);
        assert_eq!(info.prg_ram_size, 8 * 1024);
        assert_eq!(info.chr_ram_size, 8 * 1024);
        assert_eq!(info.region, Region::Pal);
        assert!(info.four_score);
        assert!(info.battery);
        assert_eq!(info.mirroring, Mirroring::Vertical);
    }

    #[test]
    fn trainer_and_errors() {
        assert_eq!(parse(b"junkjunkjunkjunk").unwrap_err(), RomError::NotNes);
        assert_eq!(parse(b"NES\x1A").unwrap_err(), RomError::Truncated);
        let mut v = header(1, 0, 0b0000_0100, 0);
        v.extend(std::iter::repeat_n(0xAAu8, 512));
        v.extend(std::iter::repeat_n(0u8, 16 * 1024));
        let info = parse(&v).unwrap();
        assert_eq!(info.trainer.as_deref().map(|t| t.len()), Some(512));
        assert_eq!(info.trainer.unwrap()[0], 0xAA);
        let short = header(2, 0, 0, 0);
        assert_eq!(parse(&short).unwrap_err(), RomError::Truncated);
    }
}
