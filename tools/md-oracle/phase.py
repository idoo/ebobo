#!/usr/bin/env python3
"""Is a differing picture a drawing difference, or the same picture a few frames apart?

    phase.py ROM --at F[,F...] [--span 4] [--input FILE] [--region R] [--ours PATH]

For every frame F, ebobo's picture at F is compared (compare.py's pixel test) with the reference's
pictures at F-span .. F+span. A game that waits on something that takes a little longer in one
emulator runs a whole frame or more apart from then on: its pictures are right, only early or
late. Each line shows the share of differing pixels per offset and the best offset; `+k` means
ours shows at F what the reference shows k frames later (ours is ahead).
Ours is --ours, else $CARGO_TARGET_DIR (default target-md-hl)/release/examples/headless.
"""
import argparse
import os
import sys
import tempfile

import compare as C


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("rom")
    ap.add_argument("--at", required=True, help="frames of ours, comma separated")
    ap.add_argument("--span", type=int, default=4, help="reference frames each side (default 4)")
    ap.add_argument("--input", help="input file, passed to both runners")
    ap.add_argument("--region", help="ntsc-u, pal or ntsc-j, passed to both runners")
    ap.add_argument("--ours", default=os.path.join(os.environ.get("CARGO_TARGET_DIR", os.path.join(C.REPO, "target-md-hl")),
                                                   "release", "examples", "headless"))
    args = ap.parse_args()
    frames = sorted({int(f) for f in args.at.split(",") if f.strip()})
    wanted = sorted({f + k for f in frames for k in range(-args.span, args.span + 1) if f + k >= 1})
    tail = []
    if args.input:
        tail += ["--input", args.input]
    if args.region:
        tail += ["--region", args.region]
    code = 0
    with tempfile.TemporaryDirectory(prefix="md-phase-") as tmp:
        dirs = {side: os.path.join(tmp, side) for side in ("ref", "ours")}
        for d in dirs.values():
            os.makedirs(d)
        cmds = {
            "ref": [sys.executable, os.path.join(C.HERE, "oracle.py"), "run", args.rom, "--frames", str(wanted[-1])] + tail
                   + ["--png-at", ",".join(map(str, wanted)), dirs["ref"]],
            "ours": [args.ours, args.rom, "--frames", str(frames[-1])] + tail + ["--png-at", ",".join(map(str, frames)), dirs["ours"]],
        }
        for side, cmd in cmds.items():
            ok, text = C.run(cmd, os.path.join(tmp, side + ".log"))
            if not ok:
                sys.exit(f"phase: {side} failed: {text}")
        pic = lambda side, f: C.read_png(os.path.join(dirs[side], f"frame_{f:06d}.png"))
        for f in frames:
            ours, shares = pic("ours", f), {}
            for k in range(-args.span, args.span + 1):
                if f + k < 1:
                    continue
                ref = pic("ref", f + k)
                shares[k] = C.compare(ref, ours)[0]["share"] if ref[:2] == ours[:2] else None
            sized = {k: s for k, s in shares.items() if s is not None}
            cells = "  ".join(f"{k:+d}:{'size' if s is None else format(s * 100, '.2f')}" for k, s in shares.items())
            if not sized:
                print(f"frame {f:6d}  {cells}   picture size differs at every offset")
                code = 1
                continue
            # The nearest offset among equally good ones.
            best = min(sized, key=lambda k: (sized[k], abs(k)))
            if sized[best] == 0.0:
                verdict = "same" if best == 0 else f"same picture, ours {abs(best)} frame(s) {'ahead' if best > 0 else 'behind'}"
            else:
                verdict = f"differs at every offset (best {best:+d}: {sized[best] * 100:.2f}%)"
                code = 1
            print(f"frame {f:6d}  {cells}   {verdict}")
    return code


if __name__ == "__main__":
    sys.exit(main())
