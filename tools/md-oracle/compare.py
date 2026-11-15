#!/usr/bin/env python3
"""Compare ebobo's Mega Drive core with the reference (oracle.py), picture by picture.

compare.py ROM [ROM ...] [--frames 600] [--at 60,120,300,600] [--input FILE] [--region R] [--out DIR] [--ours PATH]

For each ROM both runners get the same arguments and write `DIR/<stem>/ref/frame_%06d.png` and
`DIR/<stem>/ours/frame_%06d.png`. Each frame is compared per channel on the console's 15 brightness steps (see
REF_STEP / OURS_STEP), and `DIR/<stem>/diff_%06d.png` shows reference, ours and a mask side by side.
Exit code 0 when every ROM is `same` or `close`, 1 otherwise. Standard library only.
"""

import argparse
import os
import shutil
import struct
import subprocess
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
TIMEOUT_S = 60
TAIL_LINES = 8
GAP = 4  # pixels between the three panels of a diff picture
PNG_SIG = b"\x89PNG\r\n\x1a\n"
# Each side's channel value -> brightness step 0..14 of the console's 15 output steps: normal colour k is step 2k,
# shadowed k is step k, highlighted k is step 7 + k. The reference (Genesis Plus GX, RGB565 widened) gives step
# v >> 4 (its highlight 7 is 15, kept as 14). Ours uses the levels of `crates/ebobo-md/src/vdp_render.rs`
# (SHADOW then HIGHLIGHT, which also hold every NORMAL level); a value is taken to the nearest of them.
REF_STEP = bytes(min(v >> 4, 14) for v in range(256))
OURS_LEVELS = [0, 29, 52, 70, 87, 101, 116, 130, 144, 158, 172, 187, 206, 228, 255]
OURS_STEP = bytes(min(range(15), key=lambda s: abs(OURS_LEVELS[s] - v)) for v in range(256))
DIM = bytes(v >> 2 for v in range(256))


# ---- PNG ----

def read_png(path):
    """Returns (width, height, bytes of 8-bit R,G,B rows top to bottom). 8-bit RGB or RGBA, not interlaced."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != PNG_SIG:
        raise ValueError(f"{path}: not a PNG")
    pos, idat, head = 8, [], None
    while pos + 8 <= len(data):
        length, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if kind == b"IHDR":
            head = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            idat.append(body)
        elif kind == b"IEND":
            break
    if head is None:
        raise ValueError(f"{path}: no IHDR")
    w, h, depth, ctype, _, _, interlace = head
    if depth != 8 or ctype not in (2, 6) or interlace != 0:
        raise ValueError(f"{path}: only 8-bit RGB/RGBA non-interlaced PNGs (depth {depth}, type {ctype}, interlace {interlace})")
    bpp = 3 if ctype == 2 else 4
    stride = w * bpp
    raw = zlib.decompress(b"".join(idat))
    if len(raw) != (stride + 1) * h:
        raise ValueError(f"{path}: {len(raw)} bytes of pixels, expected {(stride + 1) * h}")
    out = bytearray()
    prev = bytearray(stride)
    for y in range(h):
        start = y * (stride + 1)
        ftype = raw[start]
        row = bytearray(raw[start + 1:start + 1 + stride])
        if ftype == 1:  # Sub
            for i in range(bpp, stride):
                row[i] = (row[i] + row[i - bpp]) & 255
        elif ftype == 2:  # Up
            for i in range(stride):
                row[i] = (row[i] + prev[i]) & 255
        elif ftype == 3:  # Average
            for i in range(stride):
                left = row[i - bpp] if i >= bpp else 0
                row[i] = (row[i] + ((left + prev[i]) >> 1)) & 255
        elif ftype == 4:  # Paeth
            for i in range(stride):
                a = row[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pred = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                row[i] = (row[i] + pred) & 255
        elif ftype != 0:
            raise ValueError(f"{path}: filter type {ftype} in row {y}")
        if bpp == 4:
            rgb = bytearray(w * 3)
            rgb[0::3], rgb[1::3], rgb[2::3] = row[0::4], row[1::4], row[2::4]
            out += rgb
        else:
            out += row
        prev = row
    return w, h, bytes(out)


def write_png(path, w, h, rgb):
    """Writes 8-bit RGB rows (top to bottom) as a PNG."""
    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    stride = w * 3
    raw = b"".join(b"\x00" + rgb[y * stride:(y + 1) * stride] for y in range(h))
    with open(path, "wb") as f:
        f.write(PNG_SIG + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))


# ---- comparing ----

def compare(ref, ours):
    """Both are (w, h, rgb) of one size. Returns the numbers of the frame and the mask picture (rgb)."""
    w, h, a = ref
    b = ours[2]
    stride = w * 3
    la, lb = a.translate(REF_STEP), b.translate(OURS_STEP)
    mask = bytearray(a.translate(DIM))
    differing = near = 0
    cells = set()
    x0, y0, x1, y1 = w, h, -1, -1
    for y in range(h):
        ra, rb = la[y * stride:(y + 1) * stride], lb[y * stride:(y + 1) * stride]
        if ra == rb:
            continue
        for x in range(w):
            i = x * 3
            if ra[i:i + 3] == rb[i:i + 3]:
                continue
            step = max(abs(ra[i] - rb[i]), abs(ra[i + 1] - rb[i + 1]), abs(ra[i + 2] - rb[i + 2]))
            differing += 1
            near += step == 1
            cells.add((x >> 3, y >> 3))
            x0, x1 = min(x0, x), max(x1, x)
            y0, y1 = min(y0, y), max(y1, y)
            o = y * stride + i
            mask[o:o + 3] = b"\x90\x00\x00" if step == 1 else b"\xff\x00\x00"
    return {
        "pixels": w * h,
        "differing": differing,
        "near": near,
        "far": differing - near,
        "share": differing / (w * h) if w * h else 0.0,
        "bbox": (x0, y0, x1, y1) if differing else None,
        "cells": len(cells),
        "total_cells": ((w + 7) >> 3) * ((h + 7) >> 3),
    }, bytes(mask)


def side_by_side(panels):
    """panels: [(w, h, rgb)]. One picture with the panels left to right on dark grey, GAP pixels apart."""
    height = max(p[1] for p in panels)
    width = sum(p[0] for p in panels) + GAP * (len(panels) - 1)
    rows = []
    for y in range(height):
        parts = []
        for n, (w, h, rgb) in enumerate(panels):
            if n:
                parts.append(b"\x30\x30\x30" * GAP)
            parts.append(rgb[y * w * 3:(y + 1) * w * 3] if y < h else b"\x30\x30\x30" * w)
        rows.append(b"".join(parts))
    return width, height, b"".join(rows)


# ---- running ----

def run(cmd, log_path):
    """Runs one runner. Returns (ok, last lines of its output); the whole output goes to log_path."""
    try:
        done = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=TIMEOUT_S)
        text, note = done.stdout.decode("utf-8", "replace"), None if done.returncode == 0 else f"exit code {done.returncode}"
    except subprocess.TimeoutExpired as e:
        text, note = (e.stdout or b"").decode("utf-8", "replace"), f"timed out after {TIMEOUT_S} s"
    except OSError as e:
        text, note = "", f"cannot start: {e}"
    with open(log_path, "w") as f:
        f.write(text)
    if note is None:
        return True, ""
    tail = text.strip().splitlines()[-TAIL_LINES:]
    return False, "\n".join([note] + tail)


def compare_rom(rom, args, frames_at):
    """Runs both sides for one ROM, prints the per-frame lines, returns (verdict, worst share or None, note)."""
    stem = os.path.splitext(os.path.basename(rom))[0]
    base = os.path.join(args.out, stem)
    shutil.rmtree(base, ignore_errors=True)
    dirs = {side: os.path.join(base, side) for side in ("ref", "ours")}
    for d in dirs.values():
        os.makedirs(d)
    tail = ["--frames", str(args.frames)]
    if args.input:
        tail += ["--input", args.input]
    if args.region:
        tail += ["--region", args.region]
    at = ",".join(str(f) for f in frames_at)
    cmds = {
        "ref": [sys.executable, os.path.join(HERE, "oracle.py"), "run", rom] + tail + ["--png-at", at, dirs["ref"]],
        "ours": [args.ours, rom] + tail + ["--png-at", at, dirs["ours"]],
    }
    print(f"== {stem}")
    crashed = []
    for side in ("ref", "ours"):
        ok, text = run(cmds[side], os.path.join(base, side + ".log"))
        if not ok:
            crashed.append(side)
            print(f"  {side} CRASH: " + text.replace("\n", "\n    | "))
    if crashed:
        return "crash", None, " and ".join(crashed) + " failed"

    worst, size_mismatch, missing, far_total = 0.0, False, [], 0
    for frame in frames_at:
        name = f"frame_{frame:06d}.png"
        pics = {}
        for side in ("ref", "ours"):
            path = os.path.join(dirs[side], name)
            try:
                pics[side] = read_png(path)
            except (OSError, ValueError, zlib.error) as e:
                missing.append(f"{side} frame {frame}")
                print(f"  frame {frame:6d}  {side}: {e}")
        if len(pics) < 2:
            continue
        ref, ours = pics["ref"], pics["ours"]
        diff_path = os.path.join(base, f"diff_{frame:06d}.png")
        if ref[:2] != ours[:2]:
            size_mismatch = True
            print(f"  frame {frame:6d}  SIZE ref {ref[0]}x{ref[1]} ours {ours[0]}x{ours[1]}")
            write_png(diff_path, *side_by_side([ref, ours]))
            continue
        r, mask = compare(ref, ours)
        write_png(diff_path, *side_by_side([ref, ours, (ref[0], ref[1], mask)]))
        worst = max(worst, r["share"])
        far_total += r["far"]
        if r["differing"]:
            x0, y0, x1, y1 = r["bbox"]
            box = f"box x {x0}..{x1} y {y0}..{y1}"
        else:
            box = "box none"
        print(f"  frame {frame:6d}  {ref[0]}x{ref[1]}  differ {r['differing']}/{r['pixels']} ({r['share'] * 100:.2f}%)  "
              f"one step {r['near']}  more {r['far']}  {box}  cells {r['cells']}/{r['total_cells']}")
    if missing:
        return "crash", None, "no picture: " + ", ".join(missing)
    if size_mismatch:
        return "size", worst, ""
    if worst == 0.0:
        return "same", worst, ""
    if worst < 0.01:
        return "close (<1% pixels)", worst, ""
    return "different", worst, ""


def main():
    ap = argparse.ArgumentParser(description="Compare ebobo's Mega Drive core with the reference, frame by frame.")
    ap.add_argument("roms", nargs="+", metavar="ROM")
    ap.add_argument("--frames", type=int, default=600)
    ap.add_argument("--at", default="60,120,300,600", help="frames to compare, comma separated (those above --frames are dropped)")
    ap.add_argument("--input", help="input file, passed to both runners")
    ap.add_argument("--region", help="ntsc-u, pal or ntsc-j, passed to both runners")
    ap.add_argument("--out", default="/tmp/md-compare")
    ap.add_argument("--ours", default=os.path.join(REPO, "target-md-hl", "release", "examples", "headless"))
    args = ap.parse_args()
    try:
        frames_at = sorted({int(f) for f in args.at.split(",") if f.strip()})
    except ValueError:
        ap.error("--at takes frame numbers separated by commas")
    frames_at = [f for f in frames_at if 1 <= f <= args.frames]
    if not frames_at:
        ap.error("--at has no frame within --frames")

    results = []
    for rom in args.roms:
        stem = os.path.splitext(os.path.basename(rom))[0]
        try:
            verdict, worst, note = compare_rom(rom, args, frames_at)
        except Exception as e:  # one broken ROM must not stop the rest
            verdict, worst, note = "crash", None, f"compare.py: {type(e).__name__}: {e}"
        print(f"  verdict: {verdict}" + (f" ({note})" if note else ""))
        results.append((stem, verdict, worst, note))

    width = max(len(r[0]) for r in results)
    print()
    print(f"{'ROM':<{width}}  {'verdict':<18}  worst frame")
    for stem, verdict, worst, note in results:
        share = "-" if worst is None else f"{worst * 100:.2f}%"
        print(f"{stem:<{width}}  {verdict:<18}  {share}" + (f"  {note}" if note else ""))
    print(f"pictures: {args.out}/<rom>/diff_NNNNNN.png (reference | ours | mask: red = differs, dark red = by one step)")
    bad = [r for r in results if r[1] not in ("same", "close (<1% pixels)")]
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
