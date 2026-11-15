#!/usr/bin/env python3
"""Diff the 64 KB work RAM of the reference emulator and ebobo's core at given frames.

    ramdiff.py ROM [--frames N] [--at F,F,..] [--first]

Prints the differing byte ranges (offsets into the RAM at 0xFF0000) with both values, reference
first. `--first` bisects for the first frame at which any byte below 0xFC00 (not the stack)
differs. Ours is `target-md-hl/release/examples/headless` (build it first).
"""
import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
OURS = ROOT / "target-md-hl/release/examples/headless"
STACK = 0xFC00


def dump(rom, frame, tmp):
    """(reference RAM, our RAM) after `frame` frames."""
    ref, ours = Path(tmp) / f"ref{frame}.bin", Path(tmp) / f"ours{frame}.bin"
    quiet = dict(stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
    n = str(frame + 1)
    subprocess.run([sys.executable, str(HERE / "oracle.py"), "run", rom, "--frames", n, "--ram-at", str(frame), str(ref), "--ram-swap"], **quiet)
    subprocess.run([str(OURS), rom, "--frames", n, "--ram-at", str(frame), str(ours)], **quiet)
    return ref.read_bytes(), ours.read_bytes()


def ranges(a, b, limit=0x10000):
    """Runs of differing bytes as (start, end exclusive); a gap of one equal byte does not split a run."""
    out, i = [], 0
    while i < limit:
        if a[i] != b[i]:
            j = i + 1
            while j < limit and (a[j] != b[j] or (j + 1 < limit and a[j + 1] != b[j + 1])):
                j += 1
            out.append((i, j))
            i = j
        else:
            i += 1
    return out


def main():
    p = argparse.ArgumentParser()
    p.add_argument("rom")
    p.add_argument("--frames", type=int, default=600, help="upper bound for --first")
    p.add_argument("--at", default="", help="frames to diff, comma separated")
    p.add_argument("--first", action="store_true")
    args = p.parse_args()
    frames = [int(f) for f in args.at.split(",") if f]
    with tempfile.TemporaryDirectory() as tmp:
        if args.first:
            differs = lambda f: bool(ranges(*dump(args.rom, f, tmp), STACK))
            lo, hi = 0, args.frames
            if not differs(hi):
                print(f"no difference below 0x{STACK:04X} at frame {hi}")
            else:
                while hi - lo > 1:
                    mid = (lo + hi) // 2
                    lo, hi = (lo, mid) if differs(mid) else (mid, hi)
                print(f"first differing frame: {hi}")
                frames.append(hi)
        for f in frames:
            ref, ours = dump(args.rom, f, tmp)
            rs = ranges(ref, ours)
            print(f"frame {f}: {len(rs)} differing ranges")
            for s, e in rs:
                e = min(e, s + 16)
                note = "  (stack)" if s >= STACK else ""
                print(f"  {s:04X}..{e - 1:04X}: {ref[s:e].hex().upper()} | {ours[s:e].hex().upper()}{note}")


if __name__ == "__main__":
    main()
