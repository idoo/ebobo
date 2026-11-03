# ebobo extensions

## ESX1: extra sprite tile banks with their own palettes

Purpose: give NES ROM hacks new sprite art (e.g. extra playable characters) without a new
mapper or tile budget. Real hardware and other emulators ignore the extension and show the
original tiles, so a hack degrades gracefully.

### File format

A chunk appended after the ROM's last byte (after PRG, CHR and any misc ROM; header sizes
are not changed):

```
"ESX1"                      4 bytes magic
count: u8                   number of banks, 1..=7
repeat count times:
    palette: [u8; 3]        NES colour indices for sprite pixel values 1, 2, 3
    first_tile: u8          first tile index this bank replaces
    tile_count: u8          number of tiles (1..=255; first_tile + tile_count <= 256)
    tiles: [u8; 16 * tile_count]   2bpp tiles, standard NES layout
total_len: u32 LE           length of everything from "ESX1" up to (not incl.) total_len
"ESXEND!!"                  8 bytes footer
```

`cart::parse` detects the footer at the end of the file. `RomInfo::crc32` then covers PRG, CHR
and the whole chunk, so two hacks with different art never match in netplay.

### PPU behaviour (only when the ROM has an ESX1 chunk)

- OAM byte 2 keeps bits 2-4 (hardware clears them). `b = (attr >> 2) & 7`.
- Sprite pattern fetch for a sprite with `1 <= b <= count` whose tile (8x8: tile; 8x16: the
  half's tile number) is in `first_tile .. first_tile + tile_count` of bank `b`: the PPU still
  performs the normal `ppu_read` (mapper side effects such as MMC3 A12 stay identical) but uses
  the bank's tile bytes for the pattern.
- Pixels of such sprites take their colour from the bank palette (pixel value 1-3 → `palette[v-1]`)
  instead of palette RAM; transparency (value 0), priority, sprite-0 hit and overflow behave as
  usual.
- All other sprites and the background are unchanged.

ROMs without the chunk behave exactly as before (bits 2-4 read back as 0).

## ESX2: same, with a palette per tile

ESX1 gives a bank one 3-colour palette, which is too few for a recognisable character
(e.g. blond hair vs tan face vs lavender suit). ESX2 lets each tile pick one of up to 4
palettes of its bank. Same footer (`total_len` + `"ESXEND!!"`), different magic and bank layout:

```
"ESX2"                      4 bytes magic
count: u8                   number of banks, 1..=7
repeat count times:
    palette_count: u8                   1..=4
    palettes: [[u8; 3]; palette_count]  NES colour indices for pixel values 1, 2, 3
    first_tile: u8
    tile_count: u8                      1..=255; first_tile + tile_count <= 256
    tile_palette: [u8; tile_count]      palette index per tile (< palette_count)
    tiles: [u8; 16 * tile_count]
total_len: u32 LE
"ESXEND!!"
```

PPU behaviour is ESX1's, except that a replaced sprite pixel takes its colour from
`palettes[tile_palette[tile - first_tile]]` (8x16: the palette of the half's tile). `cart::parse`
accepts both magics; `esx_append` writes ESX2 when any bank has more than one palette.
