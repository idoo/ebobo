//! Appends an ESX1/ESX2 chunk (`docs/EXTENSIONS.md`) to an iNES file.
//!
//! usage: see `USAGE`. Per bank either the ESX1 form `--bank <first_tile> <c1,c2,c3> <tiles.chr>`
//! or `--bank <first_tile> <tiles.chr> --palette c1,c2,c3 [--palette ...] [--tile-palettes <spec>]`
//! (1..=4 palettes; `<spec>` is a file of tile_count bytes or an inline list `0,1,1,...`; default
//! all 0). The chunk is ESX2 when any bank has more than one palette, else ESX1.
//!
//! `tiles.chr` holds raw 16-byte 2bpp tiles; their count is the bank's tile_count. Numbers take
//! decimal, `0x..` or `$..`. An input that already carries a chunk is refused.

use ebobo_core::cart::{self, Esx, EsxBank, RomBytes};
use std::process::ExitCode;
use std::sync::Arc;

const USAGE: &str = "usage: esx_append <in.nes> <out.nes> \
--bank <first_tile> <tiles.chr> --palette c1,c2,c3 [--palette c1,c2,c3 ...] [--tile-palettes <file|i,i,...>] \
[--bank ...]   (ESX1 form also accepted: --bank <first_tile> <c1,c2,c3> <tiles.chr>)";

fn num(s: &str) -> Result<u8, String> {
    let s = s.trim();
    let r = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix('$')) {
        u8::from_str_radix(h, 16)
    } else {
        s.parse()
    };
    r.map_err(|_| format!("not a byte: {s:?}"))
}

fn palette(s: &str) -> Result<[u8; 3], String> {
    let p: Vec<u8> = s.split(',').map(num).collect::<Result<_, _>>()?;
    p.try_into().map_err(|_| format!("palette needs 3 colours: {s}"))
}

fn read_tiles(path: &str) -> Result<Vec<u8>, String> {
    let tiles = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if tiles.is_empty() || tiles.len() % 16 != 0 || tiles.len() / 16 > 255 {
        return Err(format!("{path}: need 1..=255 whole 16-byte tiles, got {} bytes", tiles.len()));
    }
    Ok(tiles)
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 6 {
        return Err(USAGE.into());
    }
    let (input, output) = (&args[0], &args[1]);
    let mut banks = Vec::new();
    let mut rest = &args[2..];
    while !rest.is_empty() {
        if rest[0] != "--bank" || rest.len() < 4 {
            return Err(USAGE.into());
        }
        let first_tile = num(&rest[1])?;
        if rest[3] != "--palette" {
            // ESX1 form.
            let pal = palette(&rest[2])?;
            let tiles = read_tiles(&rest[3])?;
            banks.push(EsxBank::single(pal, first_tile, RomBytes(Arc::from(tiles))));
            rest = &rest[4..];
            continue;
        }
        let tiles = read_tiles(&rest[2])?;
        let tile_count = tiles.len() / 16;
        rest = &rest[3..];
        let mut palettes = Vec::new();
        let mut tile_palette = vec![0u8; tile_count];
        while rest.len() >= 2 && rest[0] != "--bank" {
            match rest[0].as_str() {
                "--palette" => palettes.push(palette(&rest[1])?),
                "--tile-palettes" => {
                    let spec = &rest[1];
                    tile_palette = if std::path::Path::new(spec).is_file() {
                        std::fs::read(spec).map_err(|e| format!("{spec}: {e}"))?
                    } else {
                        spec.split(',').map(num).collect::<Result<_, _>>()?
                    };
                    if tile_palette.len() != tile_count {
                        return Err(format!(
                            "--tile-palettes {spec}: need {tile_count} entries, got {}",
                            tile_palette.len()
                        ));
                    }
                }
                _ => return Err(USAGE.into()),
            }
            rest = &rest[2..];
        }
        if rest.first().is_some_and(|a| a != "--bank") {
            return Err(USAGE.into());
        }
        if !(1..=4).contains(&palettes.len()) {
            return Err(format!("bank at tile {first_tile}: need 1..=4 --palette, got {}", palettes.len()));
        }
        if let Some(&p) = tile_palette.iter().find(|&&p| p as usize >= palettes.len()) {
            return Err(format!("bank at tile {first_tile}: tile palette {p} >= {} palettes", palettes.len()));
        }
        banks.push(EsxBank {
            palettes,
            first_tile,
            tile_count: tile_count as u8,
            tile_palette: RomBytes(Arc::from(tile_palette)),
            tiles: RomBytes(Arc::from(tiles)),
        });
    }

    let rom = std::fs::read(input).map_err(|e| format!("{input}: {e}"))?;
    let info = cart::parse(&rom).map_err(|e| format!("{input}: {e}"))?;
    if info.esx.is_some() {
        return Err(format!("{input} already has an ESX chunk"));
    }
    let chunk = Esx::build(&banks).map_err(|e| e.to_string())?;
    let mut out = rom;
    out.extend_from_slice(&chunk);
    let check = cart::parse(&out).map_err(|e| format!("result does not parse: {e}"))?;
    let n = check.esx.as_ref().map_or(0, |e| e.banks.len());
    if n != banks.len() {
        return Err("result lost the ESX chunk".into());
    }
    std::fs::write(output, &out).map_err(|e| format!("{output}: {e}"))?;
    println!(
        "{output}: {n} {} bank(s), {} chunk bytes, crc32 {:08X}",
        String::from_utf8_lossy(&chunk[..4]),
        chunk.len(),
        check.crc32
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("esx_append: {e}");
            ExitCode::FAILURE
        }
    }
}
