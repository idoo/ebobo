#!/usr/bin/env python3
"""Measured table of the probe `cpu_rate` and the refresh rules tried against it.

cpu_rate.py

Passes per frame of each loop shape (second, clean interval of the probe), in the reference and
in ours with the 128/130 rule (every 128 cycles run, +2, remainder carried). The fraction is
passes x cycles / frame cycles (NTSC 896040 / 7, PAL 313 x 3420 x 10 / 7); the rest of the frame
is the vertical-interrupt handler (about 250 cycles) and the refresh holds.
Each rule is simulated over the shape's instruction lengths and prints its predicted passes.
"""

NTSC, PAL = 896040 / 7, 313 * 3420 / 7
HANDLER = 250  # interrupt entry, res_w, return: CPU cycles (approximate, same in both)

# shape: (instruction lengths of one pass, reference NTSC, ours NTSC, reference PAL, ours PAL)
SHAPES = {
    "reg2": ([4, 4, 8, 10], 4840, 4839, 5783, 5782),
    "reg5": ([4, 4, 4, 4, 4, 8, 10], 3310, 3311, 3955, 3956),
    "rmw": ([20, 8, 10], 3312, 3311, 3958, 3956),
    "ram2ram": ([20, 8, 10], 3312, 3311, 3958, 3956),
    "romread": ([12, 8, 10], 4194, 4194, 5012, 5012),
    "divu": ([4, 136, 8, 10], 806, 796, 964, 952),
    "mulu": ([70, 8, 10], 1436, 1430, 1716, 1708),
    "lsl63": ([134, 8, 10], 838, 828, 1003, 990),
    "io": ([16, 8, 10], 3704, 3700, 4426, 4422),
    "vdpstat": ([16, 8, 10], 3703, 3701, 4426, 4422),
    "pushpop": ([12, 12, 8, 10], 2994, 2995, 3578, 3579),
}


def passes(lens, frame, hold):
    """Passes that fit in a frame when `hold(state, n)` returns the extra cycles of an n-cycle instruction."""
    state, t, count = {"acc": 0}, HANDLER, 0
    while True:
        for n in lens:
            t += n + hold(state, n)
        if t > frame:
            return count
        count += 1


def rule_a(period):
    def hold(s, n):
        s["acc"] += n
        k = s["acc"] // period
        s["acc"] -= k * period
        return 2 * k
    return hold


def rule_b(s, n):
    s["acc"] += n
    if s["acc"] >= 128:
        s["acc"] = 0
        return 2
    return 0


def main():
    print(f"{'shape':9} {'cyc':>4} {'ref':>5} {'ours':>5} {'ref%':>6} | {'pal':>5} {'ours':>5} | a128  a136  b")
    worst = {"a128": 0, "a136": 0, "b": 0}
    for name, (lens, ref, our, rpal, opal) in SHAPES.items():
        cyc = sum(lens)
        preds = {"a128": passes(lens, NTSC, rule_a(128)), "a136": passes(lens, NTSC, rule_a(136)),
                 "b": passes(lens, NTSC, rule_b)}
        for k, v in preds.items():
            worst[k] = max(worst[k], abs(v - ref))
        print(f"{name:9} {cyc:4} {ref:5} {our:5} {100 * ref * cyc / NTSC:6.2f} | {rpal:5} {opal:5} | "
              + "  ".join(f"{preds[k]:4}" for k in ("a128", "a136", "b")))
    print("worst error in passes (NTSC):", worst)


if __name__ == "__main__":
    main()
