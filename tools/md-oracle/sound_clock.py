#!/usr/bin/env python3
"""Rates of the FM chip's timers and of the Z80, reference against ours, from the probes
`ym_timers` and `z80_rate`.

sound_clock.py            (runs both probes in both emulators, NTSC and PAL)

Timer rows: overflows counted in 64 frames; ticks per second = count x period / (64 frames),
period = 1024 - N (timer A) or (256 - N) x 16 (timer B) ticks of 144 FM clocks; nominal is
master clock / 7 / 144 (NTSC 53267, PAL 52781). Short periods are limited by the 68000's poll
loop, so their rate is only a comparison between the two emulators.
Z80 rows: passes in 60 frames x T-states of a pass / 60 = T-states a frame; nominal is a frame's
master clocks / 15 (NTSC 59736, PAL 71364). Case d adds 31 T-states per interrupt taken, which the
row does not count: a lower figure there is time spent in the handler.
The last column is ours against the reference in percent.
Ours is `$CARGO_TARGET_DIR/release/examples/headless`, as for probe.py.
"""
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
A_VALUES = [0x000, 0x200, 0x300, 0x3C0, 0x3F0, 0x3FF]
B_VALUES = [0x00, 0x80, 0xC0, 0xF0, 0xFF]
Z80_CASES = [("a loop", 56), ("b bank read", 69), ("c busreq/line", 56), ("d int mode 1", 56)]
REGIONS = {"ntsc": (53693175, 262), "pal": (53203424, 313)}


def words(name, region):
    """(reference words, our words) of a probe, parsed from probe.py's table."""
    cmd = [sys.executable, str(HERE / "probe.py"), name] + (["--region", "pal"] if region == "pal" else [])
    out = subprocess.run(cmd, capture_output=True, text=True).stdout
    ref, ours = [], []
    for line in out.splitlines():
        m = re.match(r"\s*\d+\s+([0-9A-Fa-f]{4})\s+([0-9A-Fa-f]{4}|-+)", line)
        if m:
            ref.append(int(m.group(1), 16))
            ours.append(int(m.group(2), 16) if re.fullmatch(r"[0-9A-Fa-f]{4}", m.group(2)) else 0)
    if not ref:
        sys.exit(f"{name} ({region}): no words in probe.py output:\n{out}")
    return ref, ours


def pct(ref, ours):
    return f"{100 * (ours - ref) / ref:+7.2f} %" if ref else "      -"


def main():
    for region, (mclk, lines) in REGIONS.items():
        frame_s = lines * 3420 / mclk
        print(f"== {region}: timers (nominal {mclk / 7 / 144:.0f} ticks/s)")
        print(f"{'case':16} {'ref':>6} {'ours':>6} {'ref ticks/s':>12} {'ours ticks/s':>12} {'diff':>9}")
        ref, ours = words("ym_timers", region)
        cases = [(f"A 0x{n:03X}", 1024 - n) for n in A_VALUES] + [(f"B 0x{n:02X}", (256 - n) * 16) for n in B_VALUES]
        for i, (label, period) in enumerate(cases):
            r, o = ref[i], ours[i]
            print(f"{label:16} {r:6} {o:6} {r * period / (64 * frame_s):12.0f} {o * period / (64 * frame_s):12.0f} {pct(r, o):>9}")
        print(f"{'busy polls':16} {ref[11]:6} {ours[11]:6}")
        print(f"{'status 4000-3':16} {ref[12]:04X}{ref[13]:04X} {ours[12]:04X}{ours[13]:04X}")
        print(f"== {region}: Z80 (nominal {lines * 3420 / 15:.0f} T-states/frame)")
        print(f"{'case':16} {'ref':>6} {'ours':>6} {'ref T/frame':>12} {'ours T/frame':>12} {'diff':>9}")
        ref, ours = words("z80_rate", region)
        for i, (label, t) in enumerate(Z80_CASES):
            r = ref[2 * i] + (ref[2 * i + 1] << 16)
            o = ours[2 * i] + (ours[2 * i + 1] << 16)
            print(f"{label:16} {r:6} {o:6} {r * t / 60:12.0f} {o * t / 60:12.0f} {pct(r, o):>9}")


if __name__ == "__main__":
    main()
