#!/usr/bin/env python3
"""Which sprites show on picture line 0: the sprite_line0 probe in the reference and in ebobo.

sprite_line0.py [--frame F] [--out DIR]

Runs probes/out/sprite_line0.bin in both emulators, takes the picture of frame F and prints, for each sprite of the
probe (and the plane pattern on line 0), the picture lines its column shows non-black in, against the lines the
sprite's Y puts it on. Ours runs from $CARGO_TARGET_DIR (default target-md-hl)/release/examples/headless.
"""
import argparse
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from compare import read_png  # noqa: E402
from row_point import capture  # noqa: E402

ROM = os.path.join(HERE, "probes", "out", "sprite_line0.bin")
# name, screen column, Y, height
OBJECTS = [("A", 16, 128, 8), ("B", 48, 129, 8), ("C", 80, 127, 8), ("D", 112, 120, 32), ("E", 160, 128, 8),
           ("F", 200, 140, 8), ("plane", 300, 128, 8)]


def lines(pic, x):
    w, h, rgb = pic
    shown = [y for y in range(min(h, 48)) if any(rgb[(y * w + x) * 3:(y * w + x) * 3 + 3])]
    return f"{shown[0]}..{shown[-1]}" if shown and shown == list(range(shown[0], shown[-1] + 1)) else str(shown)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--frame", type=int, default=30)
    ap.add_argument("--out", default="/tmp/sprite-line0")
    args = ap.parse_args()
    pics = {s: capture(s, ROM, args.frame, os.path.join(args.out, s))[0] for s in ("ref", "ours")}
    print(f"== sprite_line0, frame {args.frame}: picture lines shown in each object's column")
    print("  obj    col  Y    expected   ref        ours")
    for name, x, y, hgt in OBJECTS:
        exp = f"{max(0, y - 128)}..{y - 128 + hgt - 1}"
        print(f"  {name:6} {x:3}  {y:3}  {exp:10} {lines(pics['ref'], x + 2):10} {lines(pics['ours'], x + 2)}")


if __name__ == "__main__":
    main()
