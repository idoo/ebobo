#!/usr/bin/env python3
"""Compares the sound of ebobo's Mega Drive core with the reference core, by statistics only.

    audio.py ROM [ROM...] [--frames 1200] [--cache DIR] [--timeline]

Runs `headless ROM --frames N --wav` (ours) and `oracle.py run ROM --frames N --wav` (the reference,
a black box), reads both WAVs and prints per ROM: RMS and peak of each, the level ratio in dB, the
left/right balance, the share of our samples above the mixer's limiter knee, the share of energy in
octave bands, and the correlation of the two loudness envelopes (100 ms windows). Nothing is
resampled: every figure is computed at the file's own rate. The last lines are one verdict per ROM.

`--cache DIR` keeps the reference WAVs between runs (the reference does not change when ours does).
`--timeline` adds the RMS of every second of both, to see when each one plays.
The headless binary: $EBOBO_HEADLESS, else target-md-hl/release/examples/headless in the repo.
"""

import math
import os
import struct
import subprocess
import sys
import tempfile
import wave

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
HEADLESS = os.environ.get("EBOBO_HEADLESS", os.path.join(REPO, "target-md-hl", "release", "examples", "headless"))

# Upper edges of the bands in Hz; the last band is everything above the last edge.
BAND_EDGES = (200, 400, 800, 1600, 3200, 6400, 12800)
BAND_NAMES = ("<200", "200-400", "400-800", "0.8-1.6k", "1.6-3.2k", "3.2-6.4k", "6.4-12.8k", ">12.8k")
FFT_SIZE = 1024
FFT_WINDOWS = 120
# `KNEE` of crates/ebobo-md/src/audio.rs: above it the soft limiter bends the signal.
KNEE = 0.75
LEVEL_TOLERANCE_DB = 1.0
# Ours keeps a console-like low-pass and the reference a gentler one, so the share of energy
# above 3.2 kHz may sit this far under (or over) the reference's before the tone counts as off.
TONE_TOLERANCE_DB = 3.0
TONE_FROM_BAND = 5


def read_wav(path):
    """(rate, left, right) with samples as floats in -1..1; a mono file gives the same list twice."""
    with wave.open(path, "rb") as w:
        channels, width, rate, frames = w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getnframes()
        raw = w.readframes(frames)
    if width != 2:
        raise SystemExit(f"{path}: {8 * width}-bit samples, expected 16-bit")
    data = struct.unpack(f"<{len(raw) // 2}h", raw)
    left = [s / 32768.0 for s in data[0::channels]]
    right = [s / 32768.0 for s in data[1::channels]] if channels > 1 else left
    return rate, left, right


def rms(samples):
    return math.sqrt(sum(s * s for s in samples) / len(samples)) if samples else 0.0


def db(ratio):
    return 20.0 * math.log10(ratio) if ratio > 0 else float("-inf")


def fft(re, im):
    """In-place radix-2 FFT; the length is a power of two."""
    n = len(re)
    j = 0
    for i in range(1, n):
        bit = n >> 1
        while j & bit:
            j ^= bit
            bit >>= 1
        j |= bit
        if i < j:
            re[i], re[j] = re[j], re[i]
            im[i], im[j] = im[j], im[i]
    size = 2
    while size <= n:
        half = size // 2
        step = -2.0 * math.pi / size
        for k in range(half):
            wr, wi = math.cos(step * k), math.sin(step * k)
            for a in range(k, n, size):
                b = a + half
                tr = re[b] * wr - im[b] * wi
                ti = re[b] * wi + im[b] * wr
                re[b], im[b] = re[a] - tr, im[a] - ti
                re[a] += tr
                im[a] += ti
        size *= 2


def band_shares(mono, rate):
    """Share of the energy in each band, from Hann-windowed spectra spread evenly over the file."""
    bands = [0.0] * (len(BAND_EDGES) + 1)
    if len(mono) < FFT_SIZE:
        return bands
    hann = [0.5 - 0.5 * math.cos(2.0 * math.pi * i / FFT_SIZE) for i in range(FFT_SIZE)]
    which = []
    for k in range(FFT_SIZE // 2):
        hz = k * rate / FFT_SIZE
        which.append(sum(1 for edge in BAND_EDGES if hz >= edge))
    span = len(mono) - FFT_SIZE
    for w in range(FFT_WINDOWS):
        start = span * w // max(1, FFT_WINDOWS - 1)
        re = [mono[start + i] * hann[i] for i in range(FFT_SIZE)]
        im = [0.0] * FFT_SIZE
        fft(re, im)
        for k in range(1, FFT_SIZE // 2):
            bands[which[k]] += re[k] * re[k] + im[k] * im[k]
    total = sum(bands)
    return [b / total for b in bands] if total > 0 else bands


def envelope(mono, rate, seconds):
    """RMS of consecutive windows of `seconds`."""
    size = max(1, int(rate * seconds))
    return [rms(mono[i : i + size]) for i in range(0, len(mono) - size + 1, size)]


def correlation(a, b):
    n = min(len(a), len(b))
    if n < 2:
        return 0.0
    a, b = a[:n], b[:n]
    ma, mb = sum(a) / n, sum(b) / n
    va = sum((x - ma) ** 2 for x in a)
    vb = sum((y - mb) ** 2 for y in b)
    if va <= 0 or vb <= 0:
        return 0.0
    return sum((x - ma) * (y - mb) for x, y in zip(a, b)) / math.sqrt(va * vb)


def measure(path):
    rate, left, right = read_wav(path)
    mono = [(l + r) * 0.5 for l, r in zip(left, right)]
    both = left + right
    return {
        "rate": rate,
        "rms": rms(both),
        "peak": max((abs(s) for s in both), default=0.0),
        "balance": db(rms(left) / rms(right)) if rms(right) > 0 and rms(left) > 0 else 0.0,
        "over": sum(1 for s in both if abs(s) > KNEE) / max(1, len(both)),
        "bands": band_shares(mono, rate),
        "env": envelope(mono, rate, 0.1),
        "seconds": envelope(mono, rate, 1.0),
    }


def render(rom, frames, cache):
    """Runs both emulators side by side; returns (our WAV, the reference's WAV)."""
    name = f"{os.path.basename(rom)}.{frames}"
    ours = os.path.join(cache, name + ".ours.wav")
    ref = os.path.join(cache, name + ".ref.wav")
    jobs = [subprocess.Popen([HEADLESS, rom, "--frames", str(frames), "--wav", ours], stdout=subprocess.DEVNULL)]
    if not os.path.exists(ref):
        oracle = [sys.executable, os.path.join(HERE, "oracle.py"), "run", rom, "--frames", str(frames), "--wav", ref]
        jobs.append(subprocess.Popen(oracle, stdout=subprocess.DEVNULL))
    for job in jobs:
        if job.wait() != 0:
            raise SystemExit(f"{rom}: {job.args[0]} exited with {job.returncode}")
    return ours, ref


def report(rom, frames, cache, timeline):
    ours_path, ref_path = render(rom, frames, cache)
    ours, ref = measure(ours_path), measure(ref_path)
    name = os.path.basename(rom)
    print(f"== {name}, {frames} frames")
    for label, m in (("ours", ours), ("reference", ref)):
        print(
            f"  {label:9} {m['rate']} Hz  rms {m['rms']:.4f}  peak {m['peak']:.3f}  L/R {m['balance']:+.2f} dB"
            f"  above the knee ({KNEE}) {100.0 * m['over']:.4f} %"
        )
    print("  band       " + " ".join(f"{n:>9}" for n in BAND_NAMES))
    for label, m in (("ours", ours), ("reference", ref)):
        print(f"  {label:9}  " + " ".join(f"{b:9.3f}" for b in m["bands"]))
    diffs = [10.0 * math.log10(a / b) if a > 0 and b > 0 else float("nan") for a, b in zip(ours["bands"], ref["bands"])]
    print("  ours-ref dB" + " ".join(f"{d:+9.1f}" for d in diffs))
    corr = correlation(ours["env"], ref["env"])
    if timeline:
        for label, m in (("ours", ours), ("reference", ref)):
            print(f"  {label:9} per second: " + " ".join(f"{v:.4f}" for v in m["seconds"]))
    if ours["rms"] <= 0 or ref["rms"] <= 0:
        state = "both silent" if ours["rms"] <= 0 and ref["rms"] <= 0 else ("OURS SILENT" if ours["rms"] <= 0 else "reference silent")
        print(f"  level: {state} (rms {ours['rms']:.5f} | {ref['rms']:.5f}, peak {ours['peak']:.4f} | {ref['peak']:.4f})")
        return f"{name:24} {state}; peak {ours['peak']:.4f} | {ref['peak']:.4f}"
    level = db(ours["rms"] / ref["rms"])
    high_ours, high_ref = sum(ours["bands"][TONE_FROM_BAND:]), sum(ref["bands"][TONE_FROM_BAND:])
    tone = 10.0 * math.log10(high_ours / high_ref) if high_ours > 0 and high_ref > 0 else float("nan")
    print(f"  level {level:+.2f} dB  energy above 3.2 kHz {high_ours:.3f} | {high_ref:.3f} ({tone:+.1f} dB)  envelope correlation {corr:.3f}")
    level_ok = abs(level) <= LEVEL_TOLERANCE_DB
    tone_ok = abs(tone) <= TONE_TOLERANCE_DB
    return (
        f"{name:24} level {level:+.2f} dB {'ok' if level_ok else 'OFF'}  tone {tone:+.1f} dB {'ok' if tone_ok else 'OFF'}"
        f"  env {corr:.2f}  peak {ours['peak']:.2f} | {ref['peak']:.2f}  limiter {100.0 * ours['over']:.4f} %"
    )


def main():
    args = sys.argv[1:]
    frames, cache, timeline, roms = 1200, None, False, []
    i = 0
    while i < len(args):
        if args[i] == "--frames":
            frames = int(args[i + 1])
            i += 1
        elif args[i] == "--cache":
            cache = args[i + 1]
            i += 1
        elif args[i] == "--timeline":
            timeline = True
        elif args[i] in ("-h", "--help"):
            print(__doc__)
            return
        else:
            roms.append(args[i])
        i += 1
    if not roms:
        raise SystemExit(__doc__)
    if not os.path.exists(HEADLESS):
        raise SystemExit(f"{HEADLESS} is missing: cargo build --release -p ebobo-md --example headless")
    temp = None
    if cache is None:
        temp = tempfile.TemporaryDirectory(prefix="md-audio-")
        cache = temp.name
    os.makedirs(cache, exist_ok=True)
    verdicts = [report(rom, frames, cache, timeline) for rom in roms]
    print(f"\nverdict (level within {LEVEL_TOLERANCE_DB} dB, energy above 3.2 kHz within {TONE_TOLERANCE_DB} dB of the reference)")
    for line in verdicts:
        print("  " + line)
    if temp:
        temp.cleanup()


if __name__ == "__main__":
    main()
