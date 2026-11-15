#!/usr/bin/env python3
"""What one horizontal interrupt costs the 68000, measured by the probe `int_cost` in both emulators.

int_cost.py [--region ntsc-u|pal ...]

Per case (second, clean interval of the probe): passes of the main loop, horizontal interrupts
taken, the cost of one interrupt in CPU cycles = (baseline passes - passes) x cycles per pass /
interrupts, that cost minus the handler's own cycles (= accepting the interrupt), and what the
reference charges beyond ours. Case 7's baseline is case 0's passes x 18 / 42 (same frame time).
Runs ours from $CARGO_TARGET_DIR/release/examples/headless (via probe.py's helpers).

Measured (NTSC; PAL within 0.6), cost of one interrupt in CPU cycles, accept = cost - handler:
  case  handler  ints  reference passes cost accept  ours with 44 flat: passes cost accept
  0     -        0     6979    -     -               6981    -     -
  1     36       225   5901   86.2  50.2             6031   76.0  40.0
  2     120      225   4890  167.1  47.1             4982  159.9  39.9
  3     80       225   5339  131.2  51.2             5481  120.0  40.0
  4     52       225   5677  104.2  52.2             5831   92.0  40.0
  5     256      225   3186  303.4  47.4             3282  295.9  39.9
  6     36       28    6845   86.1  50.1             6864   75.2  39.2
  7     36       225   2531   85.9  49.9             2585   75.9  39.9
The reference charges 7-12 cycles more per interrupt in every case, port accesses or not, and
the extra changes with the handler's length: the autovector acknowledge waits for the E clock.
(Accept reads below 44 in both because the refresh holds of the baseline are part of `cost`.)
"""
import argparse
import os
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe  # noqa: E402

# case: (description, cycles per pass of the loop, handler cycles including the 16 of `hint`)
CASES = [
    ("off (baseline)", 18, 0),
    ("every line, rte", 18, 36),
    ("every line, movem pair, rte", 18, 120),
    ("every line, scroll write (ctrl long + data word)", 18, 80),
    ("every line, status read", 18, 52),
    ("every line, SGDK path", 18, 256),
    ("every 8th line, rte", 18, 36),
    ("every line, rte, 42-cycle loop", 42, 36),
]


def words(region, ours_bin, tmp):
    rom = os.path.join(probe.PROBES, "out", "int_cost.bin")
    frames = probe.frames_of("int_cost")
    ref_p, our_p = os.path.join(tmp, f"r{region}"), os.path.join(tmp, f"o{region}")
    ref = probe.run([sys.executable, os.path.join(probe.HERE, "oracle.py"), "run", rom, "--frames", str(frames),
                     "--region", region, "--ram-at", str(frames), ref_p, "--ram-swap"], ref_p)
    our = probe.run([ours_bin, rom, "--frames", str(frames), "--region", region, "--ram-at", str(frames), our_p], our_p)
    return probe.block(ref)[1], probe.block(our)[1]


def table(w):
    """[(passes, interrupts)] per case from the result words (warm-up first, second intervals clean)."""
    return [(w[4 + 4 * c + 2], w[4 + 4 * c + 3]) for c in range(len(CASES)) if 4 + 4 * c + 3 < len(w)]


def cost(rows, c):
    base = rows[0][0] * 18
    passes, ints = rows[c]
    if c == 0 or ints == 0:
        return None
    return (base - passes * CASES[c][1]) / ints


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--region", choices=("ntsc-u", "pal"), action="append")
    args = parser.parse_args()
    target = os.environ.get("CARGO_TARGET_DIR", os.path.join(probe.REPO, "target-md-hl"))
    ours_bin = os.path.join(target, "release", "examples", "headless")
    worst = 0.0
    with tempfile.TemporaryDirectory() as tmp:
        for region in args.region or ["ntsc-u", "pal"]:
            ref_w, our_w = words(region, ours_bin, tmp)
            ref, our = table(ref_w), table(our_w)
            print(f"== {region}")
            print(f"{'case':4} {'hcyc':>4} {'ints':>4} | {'ref':>5} {'cost':>6} {'accept':>6} | {'ours':>5} {'cost':>6} {'accept':>6}"
                  f" | ref-ours  description")
            for c, (desc, _, hcyc) in enumerate(CASES):
                if c >= len(ref) or c >= len(our):
                    print(f"{c:4} missing words")
                    continue
                rc, oc = cost(ref, c), cost(our, c)
                fmt = lambda v: "     -" if v is None else f"{v:6.1f}"
                acc = lambda v: "     -" if v is None else f"{v - hcyc:6.1f}"
                d = "       -" if rc is None or oc is None else f"{rc - oc:8.1f}"
                if rc is not None and oc is not None:
                    worst = max(worst, abs(rc - oc))
                print(f"{c:4} {hcyc:4} {ref[c][1]:4} | {ref[c][0]:5} {fmt(rc)} {acc(rc)} | {our[c][0]:5} {fmt(oc)} {acc(oc)} | {d}  {desc}")
    print(f"worst |ref - ours| per interrupt: {worst:.1f} cycles")


if __name__ == "__main__":
    main()
