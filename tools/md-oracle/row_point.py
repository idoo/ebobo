#!/usr/bin/env python3
"""Where a mid-line write starts to show: the row_point probes in the reference and in ebobo.

row_point.py [--frame F] [--column X] [--out DIR]

Runs probes/out/row_point.bin (write k sets plane A's vertical scroll) and row_point_c.bin (write k sets the
backdrop colour) in both emulators, takes the picture and the RAM of frame F (a frame in the middle of the passes)
and prints for each write k the HV counter read right after it and the first row, from row L_k - 2 down, where
column X shows the write: V, the row's colour is the one of scroll 8 * (k + 1) (plane colour = (row + scroll) / 8
% 14 + 1); C, the backdrop is colour k + 1. If that row shows the write only from some pixel on, the pixel is
printed too. Ours runs from $CARGO_TARGET_DIR (default target-md-hl)/release/examples/headless.
"""
import argparse
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from compare import read_png  # noqa: E402
from probe import block  # noqa: E402

REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(HERE, "probes", "out")


def colour(i):
    """Top 3 bits of R, G, B of probe colour i (red i & 7, green (i >> 3) * 2, blue 0)."""
    return (i & 7, (i >> 3) * 2, 0)


def px(pic, x, y):
    w, _, rgb = pic
    o = (y * w + x) * 3
    return tuple(c >> 5 for c in rgb[o:o + 3])


def plane(row, scroll):
    return colour((row + scroll) // 8 % 14 + 1)


def capture(side, rom, frame, out):
    os.makedirs(out, exist_ok=True)
    ram = os.path.join(out, "ram")
    if side == "ref":
        cmd = [sys.executable, os.path.join(HERE, "oracle.py"), "run", rom, "--ram-swap"]
    else:
        target = os.environ.get("CARGO_TARGET_DIR", os.path.join(REPO, "target-md-hl"))
        cmd = [os.path.join(target, "release", "examples", "headless"), rom]
    cmd += ["--frames", str(frame), "--png-at", str(frame), out, "--ram-at", str(frame), ram]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"row_point: {' '.join(cmd)} failed:\n{proc.stdout}{proc.stderr}")
    with open(ram, "rb") as f:
        words = block(f.read())[1]
    return read_png(os.path.join(out, f"frame_{frame:06d}.png")), words


def first_row(pic, k, variant, x):
    """(row, pixel) where write k first shows in column x, pixel = first x of that row showing it, or (None, None)."""
    w, h, _ = pic
    line = 16 + 8 * k
    new = (lambda y: plane(y, 8 * (k + 1))) if variant == "V" else (lambda y: colour(k + 1))
    for y in range(max(0, line - 2), min(h, line + 8)):
        if px(pic, x, y) == new(y):
            first = next(i for i in range(w) if px(pic, i, y) == new(y))
            return y, first
    return None, None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--frame", type=int, default=12)
    ap.add_argument("--column", type=int, default=160)
    ap.add_argument("--out", default="/tmp/row-point")
    args = ap.parse_args()
    for variant, name in (("V", "row_point"), ("C", "row_point_c")):
        rom = os.path.join(OUT, name + ".bin")
        sides = {s: capture(s, rom, args.frame, os.path.join(args.out, name, s)) for s in ("ref", "ours")}
        print(f"== {name} (variant {variant}, frame {args.frame}, column {args.column})")
        print("   k  line   ref HV  row  px    ours HV  row  px")
        for k in range(24):
            cells = []
            for s in ("ref", "ours"):
                pic, words = sides[s]
                y, first = first_row(pic, k, variant, args.column)
                hv = f"{words[k]:04X}" if k < len(words) else "----"
                cells.append(f"{hv}  {y if y is not None else '-':>4} {first if first is not None else '-':>3}")
            print(f"  {k:2}  {16 + 8 * k:4}   " + "     ".join(cells))


if __name__ == "__main__":
    main()
