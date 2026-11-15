# md-oracle

A reference for ebobo's Mega Drive core. `oracle.py` is a headless libretro frontend in Python 3, standard library only. It runs the
Genesis Plus GX libretro core as a black box and dumps what that core shows, plays and holds in RAM: PNG frames, CRC32
picture hashes, a WAV file and work RAM dumps. Compare ebobo's output with these.

## Commands

```sh
python3 tools/md-oracle/oracle.py fetch-core            # downloads the core into tools/md-oracle/cores/ (git-ignored)
python3 tools/md-oracle/oracle.py run ROM --frames N \
    [--input FILE] [--png-every K DIR] [--png-at F,F,... DIR] [--wav FILE] [--hash-every K] \
    [--region ntsc-u|pal|ntsc-j] [--ram-at F FILE] [--ram-swap] [--opt KEY=VALUE] [--port-device PORT=ID] [--verbose]
```

- **ROM**: a `.md`, `.bin`, `.gen` or `.smd` image, or a `.zip` that holds one.
- **Frame numbering**: frames count from 1. Frame F is the F-th `retro_run`. PNGs, hashes and RAM for frame F are taken after
  that run.
- **`--input FILE`**: each line is `<frame> <p1 hex> <p2 hex> [<p3 hex> <p4 hex>]`; `#` starts a comment. The masks are held from
  that frame on, up to the next line. They use ebobo's pad bits: Up 0x001, Down 0x002, Left 0x004, Right 0x008, A 0x010,
  B 0x020, C 0x040, Start 0x080, X 0x100, Y 0x200, Z 0x400, Mode 0x800. These map to the RetroPad buttons Genesis Plus GX
  reads for a 6-button pad: B→B, A→Y, C→A, Y→X, X→L, Z→R, Start→Start, Mode→Select.
- **Pads**: both ports are set to the core's "MD Joypad 6 Button" device (id 513). Players 3 and 4 are only read when a
  multitap device is chosen, for example `--port-device 0=1281` (6 Button + 4-WayPlay). `--verbose` lists the device ids.
- **`--png-every`, `--png-at`**: write `DIR/frame_%06d.png` at the size the core gave, with no scaling. Each size change is
  printed as `frame N size WxH`.
- **`--hash-every K`**: prints `frame N crc32 XXXXXXXX WxH`. The hash covers the visible picture as 8-bit R, G, B, rows top to
  bottom, without pitch padding. These are the same bytes as the PNG.
- **Colour depth**: the core hands out RGB565. Channels are widened to 8 bits by bit replication. The Mega Drive itself has
  3 bits per channel, so compare colours on their top 3 bits, or convert ebobo's colours the same way before hashing.
- **`--wav FILE`**: stereo 16-bit audio at the core's sample rate. That is 44100 Hz, which is the core's output rate, not the
  chips' native rate.
- **`--region`**: sets the core option `genesis_plus_gx_region_detect`. Without it, the core detects the region from the ROM
  header. `--opt` sets any other option, and `--verbose` lists them all with their defaults. Every option not set stays at
  the core's default.
- **`--ram-at F FILE`**: the 64 KB work RAM (`RETRO_MEMORY_SYSTEM_RAM`) after frame F, in the byte order the core holds it.
  **Verified: it is byte-swapped in pairs.** A test ROM that runs `move.l #$01020304,$FF0000` produces a dump starting
  `02 01 04 03`. `--ram-swap` writes it in 68000 order instead, so the dump then starts `01 02 03 04`.
- **Start-up output**: the core version, fps, sample rate, region (`retro_get_region`) and pixel format.
- **Errors**: exit code 2 with a message when the core is missing or does not load, when the ROM file is missing or the
  core refuses it, or when an option or value is unknown.

## Example

```sh
python3 tools/md-oracle/oracle.py run 240pSuite-GenesisMD-1.21.zip --frames 300 --png-every 100 /tmp/oracle-proof --hash-every 100
# fps 59.9227 sample_rate 44100 region NTSC pixel_format RGB565
# frame 1 size 256x192
# frame 5 size 320x224
# frame 300 crc32 BC8D7A5A 320x224        (the suite's main menu)
```

## Licence

Genesis Plus GX is not part of this repository. It is under a non-commercial licence. `fetch-core` downloads the
libretro buildbot binary to a git-ignored folder. `oracle.py` loads it at run time through libretro's C interface and
compares only its output. No code from it is copied, linked into ebobo or shipped. ebobo's core is written without reading
it.

## Limits

- **Platform**: Apple Silicon macOS only, because the buildbot URL is the `osx/arm64` build. On another system, pass
  `--core PATH` to a matching build.
- **Changing core version**: `fetch-core` takes the latest nightly, so its output can change between downloads. Note the
  printed core version next to any reference data you keep.
- **Frames before the first picture**: the first frames come out at 256x192, before the ROM switches the VDP to its mode.
  Hashes and PNGs are taken of whatever the core last showed.
- **ROM checks**: the core loads almost any file as some Sega system, so a wrong file is not an error. Check the size line
  and the picture.
- **Not supported**: no save states, no SRAM dumps, no Mega CD, no light guns or mice.
