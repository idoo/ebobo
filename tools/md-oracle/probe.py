#!/usr/bin/env python3
"""Runs timing probes (tools/md-oracle/probes/out/*.bin) in the reference emulator and in ebobo's
core and prints their result words side by side.

probe.py [NAME ...] [--frames N] [--region ntsc-u|pal] [--record]

A probe's result block sits at work RAM 0xFF0000: word 0 is 0xD0E1 when the probe finished (0xDEAD
after an unexpected exception), word 1 the number of result words, then the words (big-endian).
Without NAME every built probe runs. The frame count comes from the probe's `| frames: N` line
unless --frames is given. --record writes the reference's words to probes/expect/<name>[.pal].txt,
which `cargo test -p ebobo-md --test probes` compares the core against.
Ours runs from $CARGO_TARGET_DIR (default target-md-hl)/release/examples/headless; build it with
`cargo build --release -p ebobo-md --example headless`.
"""
import argparse
import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
PROBES = os.path.join(HERE, "probes")
DONE, TRAP = 0xD0E1, 0xDEAD


def frames_of(name):
    with open(os.path.join(PROBES, name + ".s")) as f:
        m = re.match(r"\|\s*frames:\s*(\d+)", f.readline())
    return int(m.group(1)) if m else 60


def block(ram):
    """(done word, list of result words) of a 64 KB dump in the 68000's byte order."""
    word = lambda i: int.from_bytes(ram[2 * i:2 * i + 2], "big")
    count = min(word(1), len(ram) // 2 - 2)
    return word(0), [word(2 + i) for i in range(count)]


def run(cmd, out):
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0 or not os.path.exists(out):
        sys.exit(f"probe: {' '.join(cmd)} failed:\n{proc.stdout}{proc.stderr}")
    with open(out, "rb") as f:
        return f.read()


def core_version():
    proc = subprocess.run([sys.executable, os.path.join(HERE, "oracle.py"), "run", os.path.join(PROBES, "out", "frame_cycles.bin"),
                           "--frames", "1"], capture_output=True, text=True)
    m = re.search(r"^core (.*?) \(", proc.stdout, re.M)
    return m.group(1) if m else "unknown version"


def status(done):
    return {DONE: "done", TRAP: "TRAP"}.get(done, f"not done ({done:04X})")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("names", nargs="*", metavar="NAME")
    parser.add_argument("--frames", type=int, help="frames to run (default: the probe's `| frames: N` line)")
    parser.add_argument("--region", choices=("ntsc-u", "pal"), default="ntsc-u")
    parser.add_argument("--record", action="store_true", help="write the reference's words to probes/expect/")
    args = parser.parse_args()
    names = args.names or sorted(n[:-4] for n in os.listdir(os.path.join(PROBES, "out")) if n.endswith(".bin"))
    target = os.environ.get("CARGO_TARGET_DIR", os.path.join(REPO, "target-md-hl"))
    ours = os.path.join(target, "release", "examples", "headless")
    if not os.path.exists(ours):
        sys.exit(f"probe: {ours} missing; cargo build --release -p ebobo-md --example headless")
    version = core_version() if args.record else None
    differing = []
    with tempfile.TemporaryDirectory() as tmp:
        for name in names:
            rom = os.path.join(PROBES, "out", name + ".bin")
            frames = args.frames or frames_of(name)
            ref_path, our_path = os.path.join(tmp, name + ".ref"), os.path.join(tmp, name + ".ours")
            ref = run([sys.executable, os.path.join(HERE, "oracle.py"), "run", rom, "--frames", str(frames), "--region", args.region,
                       "--ram-at", str(frames), ref_path, "--ram-swap"], ref_path)
            our = run([ours, rom, "--frames", str(frames), "--region", args.region, "--ram-at", str(frames), our_path], our_path)
            (ref_done, ref_w), (our_done, our_w) = block(ref), block(our)
            print(f"== {name} ({args.region}, {frames} frames): reference {status(ref_done)}, ours {status(our_done)}")
            print("   #    ref   ours")
            diff = ref_done != our_done or len(ref_w) != len(our_w)
            for i in range(max(len(ref_w), len(our_w))):
                r = f"{ref_w[i]:04X}" if i < len(ref_w) else "----"
                o = f"{our_w[i]:04X}" if i < len(our_w) else "----"
                mark = "" if r == o else "  <<"
                diff |= r != o
                print(f"  {i:2}   {r}   {o}{mark}")
            if diff:
                differing.append(name)
            if args.record:
                if ref_done != DONE:
                    sys.exit(f"probe: {name}: the reference did not finish; not recording")
                os.makedirs(os.path.join(PROBES, "expect"), exist_ok=True)
                suffix = ".pal" if args.region == "pal" else ""
                path = os.path.join(PROBES, "expect", f"{name}{suffix}.txt")
                with open(path, "w") as f:
                    f.write(f"# reference emulator {version}, {args.region}, {frames} frames\n")
                    f.writelines(f"{w:04X}\n" for w in ref_w)
                print(f"   recorded {path}")
    print(f"{len(names) - len(differing)} of {len(names)} probes match" + (f"; differ: {' '.join(differing)}" if differing else ""))
    return 1 if differing else 0


if __name__ == "__main__":
    sys.exit(main())
