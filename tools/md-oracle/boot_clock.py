#!/usr/bin/env python3
"""Prints the counter of the probe `boot_clock` after frames 1, 2, 3, 4, 8 in the reference
emulator and in ebobo's core: how much 68000 time has passed when each runner returns frame N.

boot_clock.py [--region ntsc-u|pal] [--at F,F,...]

One pass of the probe's loop is 210 master clocks; a line is 3420. A constant difference is a
power-on offset (or a different point of the frame at which each runner returns); a growing one
is a rate difference. Both runners count frames from 1 (frame F = after the F-th run).
Ours runs from $CARGO_TARGET_DIR (default target-md-hl)/release/examples/headless.
"""
import argparse
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
ROM = os.path.join(HERE, "probes", "out", "boot_clock.bin")
PASS, LINE = 210, 3420


def counter(cmd, path):
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0 or not os.path.exists(path):
        sys.exit(f"boot_clock: {' '.join(cmd)} failed:\n{proc.stdout}{proc.stderr}")
    with open(path, "rb") as f:
        ram = f.read()
    return int.from_bytes(ram[4:8], "big")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--region", choices=("ntsc-u", "pal"), default="ntsc-u")
    parser.add_argument("--at", default="1,2,3,4,8")
    args = parser.parse_args()
    ours = os.path.join(os.environ.get("CARGO_TARGET_DIR", os.path.join(REPO, "target-md-hl")), "release", "examples", "headless")
    print(f"boot_clock ({args.region}): passes of 210 master clocks")
    print(" frame        ref       ours   ours-ref    clocks    lines   ref step  ours step")
    prev = None
    with tempfile.TemporaryDirectory() as tmp:
        for frame in (int(f) for f in args.at.split(",")):
            n, ref_path, our_path = str(frame), os.path.join(tmp, "ref"), os.path.join(tmp, "ours")
            ref = counter([sys.executable, os.path.join(HERE, "oracle.py"), "run", ROM, "--frames", n, "--region", args.region,
                           "--ram-at", n, ref_path, "--ram-swap"], ref_path)
            our = counter([ours, ROM, "--frames", n, "--region", args.region, "--ram-at", n, our_path], our_path)
            diff = our - ref
            steps = f"{ref - prev[0]:10} {our - prev[1]:10}" if prev else ""
            print(f"{frame:6} {ref:10} {our:10} {diff:10} {diff * PASS:9} {diff * PASS / LINE:8.2f} {steps}")
            prev = (ref, our)
    return 0


if __name__ == "__main__":
    sys.exit(main())
