#!/usr/bin/env python3
"""Headless libretro host that runs the Genesis Plus GX core as a black box and dumps its picture, sound and RAM.

The reference ebobo's own Mega Drive core is compared with. Standard library only. See README.md.
"""

import argparse
import ctypes
import io
import os
import shutil
import struct
import sys
import tempfile
import urllib.request
import wave
import zipfile
import zlib
from ctypes import POINTER, c_bool, c_char_p, c_double, c_float, c_int, c_int16, c_size_t, c_uint, c_void_p

HERE = os.path.dirname(os.path.abspath(__file__))
CORE_NAME = "genesis_plus_gx_libretro.dylib"
CORE_URL = "https://buildbot.libretro.com/nightly/apple/osx/arm64/latest/genesis_plus_gx_libretro.dylib.zip"
DEFAULT_CORE = os.path.join(HERE, "cores", CORE_NAME)
REGION_KEY = "genesis_plus_gx_region_detect"
ROM_EXTENSIONS = (".md", ".bin", ".gen", ".smd")

# libretro environment commands (libretro.h); the experimental flag is masked off before dispatch.
ENV_EXPERIMENTAL = 0x10000
ENV_GET_CAN_DUPE = 3
ENV_SET_PERFORMANCE_LEVEL = 8
ENV_GET_SYSTEM_DIRECTORY = 9
ENV_SET_PIXEL_FORMAT = 10
ENV_SET_INPUT_DESCRIPTORS = 11
ENV_GET_VARIABLE = 15
ENV_SET_VARIABLES = 16
ENV_GET_VARIABLE_UPDATE = 17
ENV_GET_SAVE_DIRECTORY = 31
ENV_SET_SYSTEM_AV_INFO = 32
ENV_SET_CONTROLLER_INFO = 35
ENV_SET_GEOMETRY = 37

PIXEL_0RGB1555, PIXEL_XRGB8888, PIXEL_RGB565 = 0, 1, 2
PIXEL_NAMES = {PIXEL_0RGB1555: "0RGB1555", PIXEL_XRGB8888: "XRGB8888", PIXEL_RGB565: "RGB565"}

RETRO_DEVICE_JOYPAD = 1
RETRO_DEVICE_MASK = 0xFF
RETRO_DEVICE_ID_JOYPAD_MASK = 256
RETRO_MEMORY_SYSTEM_RAM = 2

# ebobo Mega Drive pad bit -> RetroPad id, as Genesis Plus GX reads a 6-button pad.
PAD_MAP = (
    (0x001, 4),  # Up
    (0x002, 5),  # Down
    (0x004, 6),  # Left
    (0x008, 7),  # Right
    (0x010, 1),  # A -> RetroPad Y
    (0x020, 0),  # B -> RetroPad B
    (0x040, 8),  # C -> RetroPad A
    (0x080, 3),  # Start
    (0x100, 10),  # X -> RetroPad L
    (0x200, 9),  # Y -> RetroPad X
    (0x400, 11),  # Z -> RetroPad R
    (0x800, 2),  # Mode -> RetroPad Select
)


class Variable(ctypes.Structure):
    _fields_ = [("key", c_void_p), ("value", c_void_p)]


class ControllerDescription(ctypes.Structure):
    _fields_ = [("desc", c_void_p), ("id", c_uint)]


class ControllerInfo(ctypes.Structure):
    _fields_ = [("types", POINTER(ControllerDescription)), ("num_types", c_uint)]


class SystemInfo(ctypes.Structure):
    _fields_ = [
        ("library_name", c_char_p),
        ("library_version", c_char_p),
        ("valid_extensions", c_char_p),
        ("need_fullpath", c_bool),
        ("block_extract", c_bool),
    ]


class Geometry(ctypes.Structure):
    _fields_ = [("base_width", c_uint), ("base_height", c_uint), ("max_width", c_uint), ("max_height", c_uint), ("aspect_ratio", c_float)]


class Timing(ctypes.Structure):
    _fields_ = [("fps", c_double), ("sample_rate", c_double)]


class AvInfo(ctypes.Structure):
    _fields_ = [("geometry", Geometry), ("timing", Timing)]


class GameInfo(ctypes.Structure):
    _fields_ = [("path", c_char_p), ("data", c_void_p), ("size", c_size_t), ("meta", c_char_p)]


ENV_CB = ctypes.CFUNCTYPE(c_bool, c_uint, c_void_p)
VIDEO_CB = ctypes.CFUNCTYPE(None, c_void_p, c_uint, c_uint, c_size_t)
AUDIO_SAMPLE_CB = ctypes.CFUNCTYPE(None, c_int16, c_int16)
AUDIO_BATCH_CB = ctypes.CFUNCTYPE(c_size_t, c_void_p, c_size_t)
INPUT_POLL_CB = ctypes.CFUNCTYPE(None)
INPUT_STATE_CB = ctypes.CFUNCTYPE(c_int16, c_uint, c_uint, c_uint, c_uint)


class Fail(Exception):
    """A clear message for the user; main() prints it and exits with code 2."""


class Host:
    """The frontend side of libretro for one core: environment, video, audio and input callbacks."""

    def __init__(self, core_path, overrides, work_dir, keep_audio):
        if not os.path.isfile(core_path):
            raise Fail(f"core not found: {core_path}\nrun: python3 {os.path.relpath(__file__)} fetch-core")
        try:
            self.lib = ctypes.CDLL(core_path)
        except OSError as e:
            raise Fail(f"core does not load: {core_path}: {e}")
        self.overrides = overrides
        self.pixel_format = PIXEL_0RGB1555  # libretro's default until the core sets one
        self.variables = {}  # key -> (description, [values], default first)
        self.controllers = []  # per port: [(description, device id)]
        self.av_changed = None  # (fps, sample_rate) when the core changed it while running
        self.frame = None  # last picture: height * pitch bytes as the core gave them
        self.width = self.height = self.pitch = 0
        self.audio = []  # chunks of interleaved stereo s16 little-endian
        self.audio_frames = 0
        self.keep_audio = keep_audio
        self.pads = [0, 0, 0, 0]  # ebobo pad masks of players 1-4
        self._tables = {}
        self._strings = {}  # C strings handed to the core; they must outlive the calls
        self._dir = ctypes.create_string_buffer(os.fsencode(work_dir))
        lib = self.lib
        lib.retro_api_version.restype = c_uint
        lib.retro_get_system_info.argtypes = [POINTER(SystemInfo)]
        lib.retro_get_system_av_info.argtypes = [POINTER(AvInfo)]
        lib.retro_load_game.argtypes = [POINTER(GameInfo)]
        lib.retro_load_game.restype = c_bool
        lib.retro_set_controller_port_device.argtypes = [c_uint, c_uint]
        lib.retro_get_region.restype = c_uint
        lib.retro_get_memory_data.argtypes = [c_uint]
        lib.retro_get_memory_data.restype = c_void_p
        lib.retro_get_memory_size.argtypes = [c_uint]
        lib.retro_get_memory_size.restype = c_size_t
        for name in ("retro_init", "retro_deinit", "retro_run", "retro_unload_game"):
            getattr(lib, name).restype = None
        self._callbacks = (
            ENV_CB(self._environment),
            VIDEO_CB(self._video),
            AUDIO_SAMPLE_CB(self._audio_sample),
            AUDIO_BATCH_CB(self._audio_batch),
            INPUT_POLL_CB(lambda: None),
            INPUT_STATE_CB(self._input_state),
        )
        setters = ("environment", "video_refresh", "audio_sample", "audio_sample_batch", "input_poll", "input_state")
        for setter, callback in zip(setters, self._callbacks):
            getattr(lib, "retro_set_" + setter)(callback)

    def _environment(self, cmd, data):
        cmd &= ~ENV_EXPERIMENTAL
        if cmd == ENV_SET_PIXEL_FORMAT:
            fmt = ctypes.cast(data, POINTER(c_int))[0]
            if fmt not in PIXEL_NAMES:
                return False
            self.pixel_format = fmt
            return True
        if cmd in (ENV_GET_SYSTEM_DIRECTORY, ENV_GET_SAVE_DIRECTORY):
            ctypes.cast(data, POINTER(c_void_p))[0] = ctypes.addressof(self._dir)
            return True
        if cmd == ENV_GET_CAN_DUPE:
            ctypes.cast(data, POINTER(c_bool))[0] = True
            return True
        if cmd == ENV_SET_VARIABLES:
            entries = ctypes.cast(data, POINTER(Variable))
            i = 0
            while entries[i].key:
                key = ctypes.string_at(entries[i].key).decode()
                description, _, values = ctypes.string_at(entries[i].value).decode().partition("; ")
                self.variables[key] = (description, values.split("|"))
                i += 1
            return True
        if cmd == ENV_GET_VARIABLE:
            variable = ctypes.cast(data, POINTER(Variable)).contents
            key = ctypes.string_at(variable.key).decode()
            value = self.overrides.get(key)
            if value is None and key in self.variables:
                value = self.variables[key][1][0]
            if value is None:
                variable.value = None
                return False
            buffer = self._strings.setdefault((key, value), ctypes.create_string_buffer(value.encode()))
            variable.value = ctypes.addressof(buffer)
            return True
        if cmd == ENV_GET_VARIABLE_UPDATE:
            ctypes.cast(data, POINTER(c_bool))[0] = False
            return True
        if cmd == ENV_SET_CONTROLLER_INFO:
            ports = ctypes.cast(data, POINTER(ControllerInfo))
            self.controllers = []
            i = 0
            while ports[i].types:
                types = ports[i].types
                self.controllers.append([(ctypes.string_at(types[j].desc).decode(), types[j].id) for j in range(ports[i].num_types)])
                i += 1
            return True
        if cmd == ENV_SET_SYSTEM_AV_INFO:
            timing = ctypes.cast(data, POINTER(AvInfo)).contents.timing
            self.av_changed = (timing.fps, timing.sample_rate)
            return True
        return cmd in (ENV_SET_GEOMETRY, ENV_SET_INPUT_DESCRIPTORS, ENV_SET_PERFORMANCE_LEVEL)

    def _video(self, data, width, height, pitch):
        if not data:  # a duplicated frame: the last picture stands
            return
        self.width, self.height, self.pitch = width, height, pitch
        self.frame = ctypes.string_at(data, pitch * height)

    def _audio_sample(self, left, right):
        self.audio_frames += 1
        if self.keep_audio:
            self.audio.append(struct.pack("<hh", left, right))

    def _audio_batch(self, data, frames):
        self.audio_frames += frames
        if self.keep_audio and data and frames:
            self.audio.append(ctypes.string_at(data, frames * 4))
        return frames

    def _input_state(self, port, device, index, button):
        if device & RETRO_DEVICE_MASK != RETRO_DEVICE_JOYPAD or port >= len(self.pads) or index != 0:
            return 0
        pad = self.pads[port]
        if button == RETRO_DEVICE_ID_JOYPAD_MASK:
            return sum(1 << retro for bit, retro in PAD_MAP if pad & bit)
        return int(any(pad & bit for bit, retro in PAD_MAP if retro == button))

    def _table(self):
        """65536 RGB triples for the 16-bit format in use; 5- and 6-bit channels widen by bit replication."""
        table = self._tables.get(self.pixel_format)
        if table is None:
            five = [(v << 3) | (v >> 2) for v in range(32)]
            six = [(v << 2) | (v >> 4) for v in range(64)]
            if self.pixel_format == PIXEL_RGB565:
                table = [bytes((five[p >> 11], six[(p >> 5) & 63], five[p & 31])) for p in range(65536)]
            else:
                table = [bytes((five[(p >> 10) & 31], five[(p >> 5) & 31], five[p & 31])) for p in range(65536)]
            self._tables[self.pixel_format] = table
        return table

    def rgb(self):
        """The last picture as width * height * 3 bytes (R, G, B), rows top to bottom, without pitch padding."""
        width, height, pitch, frame = self.width, self.height, self.pitch, self.frame
        row_bytes = width * (4 if self.pixel_format == PIXEL_XRGB8888 else 2)
        visible = frame if pitch == row_bytes else b"".join(frame[y * pitch : y * pitch + row_bytes] for y in range(height))
        if self.pixel_format == PIXEL_XRGB8888:
            out = bytearray(width * height * 3)
            out[0::3] = visible[2::4]
            out[1::3] = visible[1::4]
            out[2::3] = visible[0::4]
            return bytes(out)
        return b"".join(map(self._table().__getitem__, struct.unpack(f"<{width * height}H", visible)))

    def six_button_device(self, port):
        """The device id of the plain 6-button pad on this port, or None when the core names none."""
        if port >= len(self.controllers):
            return None
        matches = [(len(desc), device) for desc, device in self.controllers[port] if "6 button" in desc.lower()]
        return min(matches)[1] if matches else None


def write_png(path, width, height, rgb):
    """Writes an 8-bit RGB PNG: one IDAT, no filtering."""

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    stride = width * 3
    raw = b"".join(b"\0" + rgb[y * stride : (y + 1) * stride] for y in range(height))
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)))
        f.write(chunk(b"IDAT", zlib.compress(raw, 6)))
        f.write(chunk(b"IEND", b""))


def read_inputs(path):
    """Parses `<frame> <p1 hex> <p2 hex> [<p3 hex> <p4 hex>]` lines into [(frame, [p1, p2, p3, p4])] sorted by frame."""
    entries = []
    with open(path) as f:
        for number, line in enumerate(f, 1):
            fields = line.split("#", 1)[0].split()
            if not fields:
                continue
            try:
                if len(fields) not in (3, 5):
                    raise ValueError
                frame = int(fields[0])
                masks = [int(field, 16) for field in fields[1:]]
            except ValueError:
                raise Fail(f"{path}:{number}: expected `<frame> <p1 hex> <p2 hex> [<p3 hex> <p4 hex>]`, got: {line.strip()}")
            entries.append((frame, masks + [0] * (4 - len(masks))))
    entries.sort(key=lambda entry: entry[0])
    return entries


def rom_file(path, work_dir):
    """The ROM path to give the core; a .zip is unpacked (its first Mega Drive image) into the temp dir."""
    if not os.path.isfile(path):
        raise Fail(f"ROM not found: {path}")
    if not path.lower().endswith(".zip"):
        return os.path.abspath(path)
    with zipfile.ZipFile(path) as archive:
        for name in archive.namelist():
            if name.lower().endswith(ROM_EXTENSIONS):
                target = os.path.join(work_dir, os.path.basename(name))
                with open(target, "wb") as f:
                    f.write(archive.read(name))
                return target
    raise Fail(f"no {'/'.join(ROM_EXTENSIONS)} file inside {path}")


def fetch_core(args):
    target = DEFAULT_CORE
    if os.path.isfile(target) and not args.force:
        print(f"core already there: {target} (use --force to download again)")
        return
    print(f"downloading {CORE_URL}")
    request = urllib.request.Request(CORE_URL, headers={"User-Agent": "ebobo-md-oracle"})
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            packed = response.read()
        with zipfile.ZipFile(io.BytesIO(packed)) as archive:
            data = archive.read(CORE_NAME)
    except (OSError, KeyError, zipfile.BadZipFile) as e:
        raise Fail(f"core download failed: {e}")
    os.makedirs(os.path.dirname(target), exist_ok=True)
    with open(target, "wb") as f:
        f.write(data)
    print(f"wrote {target} ({len(data)} bytes)")


def run(args):
    overrides = {}
    for option in args.opt:
        key, separator, value = option.partition("=")
        if not separator:
            raise Fail(f"--opt wants key=value, got: {option}")
        overrides[key] = value
    if args.region:
        overrides[REGION_KEY] = args.region
    port_devices = {}
    for item in args.port_device:
        try:
            port, device = item.split("=")
            port_devices[int(port)] = int(device, 0)
        except ValueError:
            raise Fail(f"--port-device wants PORT=ID, got: {item}")
    inputs = read_inputs(args.input) if args.input else []
    png_at = set()
    if args.png_at:
        try:
            png_at = {int(frame) for frame in args.png_at[0].split(",")}
        except ValueError:
            raise Fail(f"--png-at wants F,F,... DIR, got: {args.png_at[0]}")
    try:
        png_every = int(args.png_every[0]) if args.png_every else 0
    except ValueError:
        raise Fail(f"--png-every wants K DIR, got: {args.png_every[0]}")
    for directory in {pair[1] for pair in (args.png_every, args.png_at) if pair}:
        os.makedirs(directory, exist_ok=True)

    work_dir = tempfile.mkdtemp(prefix="md-oracle-")
    try:
        rom = rom_file(args.rom, work_dir)
        host = Host(args.core, overrides, work_dir, bool(args.wav))
        lib = host.lib
        info = SystemInfo()
        lib.retro_get_system_info(ctypes.byref(info))
        print(f"core {info.library_name.decode()} {info.library_version.decode()} (libretro api {lib.retro_api_version()})")
        lib.retro_init()
        with open(rom, "rb") as f:
            image = f.read()
        game = GameInfo(os.fsencode(rom), ctypes.cast(ctypes.c_char_p(image), c_void_p), len(image), None)
        if not lib.retro_load_game(ctypes.byref(game)):
            lib.retro_deinit()
            raise Fail(f"the core refused the ROM: {args.rom}")
        for key, value in overrides.items():
            if key not in host.variables:
                raise Fail(f"the core has no option `{key}`; its options: {', '.join(sorted(host.variables)) or 'none announced'}")
            if value not in host.variables[key][1]:
                raise Fail(f"option `{key}` takes {'|'.join(host.variables[key][1])}, not `{value}`")
        av = AvInfo()
        lib.retro_get_system_av_info(ctypes.byref(av))
        fps, sample_rate = av.timing.fps, av.timing.sample_rate
        region = "PAL" if lib.retro_get_region() == 1 else "NTSC"
        print(f"fps {fps:.4f} sample_rate {sample_rate:.0f} region {region} pixel_format {PIXEL_NAMES[host.pixel_format]}")
        for port in (0, 1):
            device = port_devices.pop(port, None)
            if device is None:
                device = host.six_button_device(port)
            if device is not None:
                lib.retro_set_controller_port_device(port, device)
                print(f"port {port + 1} device {device}")
            else:
                print(f"port {port + 1} device left at the core's default (no 6-button pad in its controller info)")
        for port, device in sorted(port_devices.items()):
            lib.retro_set_controller_port_device(port, device)
            print(f"port {port + 1} device {device}")
        if args.verbose:
            for key in sorted(host.variables):
                description, values = host.variables[key]
                print(f"option {key} = {overrides.get(key, values[0])}  [{'|'.join(values)}]  {description}")
            for port, types in enumerate(host.controllers):
                print(f"port {port + 1} devices: " + ", ".join(f"{device} {desc}" for desc, device in types))

        size = None
        next_input = 0
        pngs = 0
        for frame in range(1, args.frames + 1):
            while next_input < len(inputs) and inputs[next_input][0] <= frame:
                host.pads = inputs[next_input][1]
                next_input += 1
            lib.retro_run()
            if host.av_changed:
                fps, sample_rate = host.av_changed
                host.av_changed = None
                print(f"frame {frame} av info changed: fps {fps:.4f} sample_rate {sample_rate:.0f}")
            if host.frame is not None and (host.width, host.height) != size:
                size = (host.width, host.height)
                print(f"frame {frame} size {host.width}x{host.height}")
            want_hash = args.hash_every and frame % args.hash_every == 0
            targets = []
            if png_every and frame % png_every == 0:
                targets.append(args.png_every[1])
            if frame in png_at:
                targets.append(args.png_at[1])
            if want_hash or targets:
                if host.frame is None:
                    print(f"frame {frame} no picture yet")
                else:
                    rgb = host.rgb()
                    if want_hash:
                        print(f"frame {frame} crc32 {zlib.crc32(rgb):08X} {host.width}x{host.height}")
                    for directory in dict.fromkeys(targets):
                        write_png(os.path.join(directory, f"frame_{frame:06d}.png"), host.width, host.height, rgb)
                        pngs += 1
            if args.ram_at and frame == int(args.ram_at[0]):
                pointer = lib.retro_get_memory_data(RETRO_MEMORY_SYSTEM_RAM)
                length = lib.retro_get_memory_size(RETRO_MEMORY_SYSTEM_RAM)
                if not pointer or not length:
                    raise Fail("the core exposes no system RAM")
                ram = ctypes.string_at(pointer, length)
                if args.ram_swap:
                    swapped = bytearray(ram)
                    swapped[0::2], swapped[1::2] = ram[1::2], ram[0::2]
                    ram = bytes(swapped)
                with open(args.ram_at[1], "wb") as f:
                    f.write(ram)
                print(f"frame {frame} ram {length} bytes -> {args.ram_at[1]}")
        if args.wav:
            with wave.open(args.wav, "wb") as out:
                out.setnchannels(2)
                out.setsampwidth(2)
                out.setframerate(round(sample_rate))
                out.writeframes(b"".join(host.audio))
        print(f"ran {args.frames} frames, {host.audio_frames} audio frames, {pngs} png")
        lib.retro_unload_game()
        lib.retro_deinit()
    finally:
        shutil.rmtree(work_dir, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(description="Run Genesis Plus GX (libretro) headless and dump picture, sound and RAM.")
    commands = parser.add_subparsers(dest="command", required=True)
    fetch = commands.add_parser("fetch-core", help="download the core from the libretro buildbot into cores/")
    fetch.add_argument("--force", action="store_true", help="download even when the core is already there")
    fetch.set_defaults(handler=fetch_core)
    runner = commands.add_parser("run", help="run a ROM for N frames")
    runner.add_argument("rom", metavar="ROM", help="Mega Drive image (.md/.bin/.gen/.smd) or a .zip holding one")
    runner.add_argument("--frames", type=int, required=True, metavar="N", help="number of frames to run; frames count from 1")
    runner.add_argument("--input", metavar="FILE", help="lines `<frame> <p1 hex> <p2 hex> [<p3 hex> <p4 hex>]`, held from that frame on")
    runner.add_argument("--png-every", nargs=2, metavar=("K", "DIR"), help="write DIR/frame_%%06d.png every K frames")
    runner.add_argument("--png-at", nargs=2, metavar=("F,F,...", "DIR"), help="write DIR/frame_%%06d.png for the listed frames")
    runner.add_argument("--wav", metavar="FILE", help="write the sound at the core's rate, stereo 16-bit")
    runner.add_argument("--hash-every", type=int, default=0, metavar="K", help="print `frame N crc32 XXXXXXXX WxH` every K frames")
    runner.add_argument("--region", choices=("ntsc-u", "pal", "ntsc-j"), help="force the console region (default: the core detects it)")
    runner.add_argument("--ram-at", nargs=2, metavar=("F", "FILE"), help="write the 64 KB work RAM after frame F, as the core holds it")
    runner.add_argument("--ram-swap", action="store_true", help="swap the bytes of each pair in the --ram-at dump")
    runner.add_argument("--opt", action="append", default=[], metavar="KEY=VALUE", help="set a core option (see --verbose)")
    runner.add_argument("--port-device", action="append", default=[], metavar="PORT=ID", help="libretro device id for a port (0-based)")
    runner.add_argument("--core", default=DEFAULT_CORE, metavar="PATH", help="the core's dylib (default: cores/ next to this script)")
    runner.add_argument("--verbose", action="store_true", help="list the core's options and controller devices")
    runner.set_defaults(handler=run)
    args = parser.parse_args()
    try:
        args.handler(args)
    except Fail as e:
        print(f"oracle: {e}", file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
