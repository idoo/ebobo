#!/usr/bin/env bash
# Mac <-> Steam Deck determinism check: runs the headless example natively and in a
# linux/amd64 Docker container on the same ROMs and pseudo-random 4-pad input scripts,
# then compares the per-frame state hashes. Prints PASS/FAIL per ROM; exits 1 on any FAIL.
#
# Usage: scripts/determinism-check.sh [EXTRA.nes ...]
# Runs the fetched test ROMs (scripts/fetch-test-roms.sh) plus any ROM paths given.
# ROMs are bind-mounted read-only into the container; nothing is copied into the repo.
# Generated inputs and logs live in target/determinism (gitignored).
set -euo pipefail
cd "$(dirname "$0")/.."

FRAMES=${FRAMES:-2000}
image=ebobo-linux-build
work="$PWD/target/determinism"
rm -rf "$work"
mkdir -p "$work/native" "$work/linux" "$work/inputs"

roms=()
add() { [ -f "$1" ] && roms+=("$1") || echo "skip (missing): $1"; }
R=crates/ebobo-core/tests/roms
add "$R/nestest/nestest.nes"
add "$R/instr_test-v5/official_only.nes"
add "$R/cpu_interrupts_v2/cpu_interrupts.nes"
add "$R/apu_test/apu_test.nes"
add "$R/MMC1_A12/mmc1_a12.nes"
add "$R/mmc3_test_2/rom_singles/4-scanline_timing.nes"
for f in "$@"; do
    add "$f"
done
[ "${#roms[@]}" -gt 0 ] || { echo "no ROMs: run scripts/fetch-test-roms.sh or pass ROM paths"; exit 1; }

# Deterministic input script per ROM (xorshift32 seeded by index): a new random byte for
# each of the 4 pads every 1-8 frames.
for i in "${!roms[@]}"; do
    python3 - "$i" "$FRAMES" > "$work/inputs/$i.txt" <<'EOF'
import sys
i, frames = int(sys.argv[1]), int(sys.argv[2])
s = (0x9E3779B9 ^ (i * 0x85EBCA6B + 1)) & 0xFFFFFFFF or 1
def rnd():
    global s
    s ^= (s << 13) & 0xFFFFFFFF; s ^= s >> 17; s ^= (s << 5) & 0xFFFFFFFF
    return s
f = 0
while f < frames:
    print(f, *("%02x" % (rnd() & 0xFF) for _ in range(4)))
    f += 1 + rnd() % 8
EOF
done

echo "== native build"
cargo build --release -p ebobo-core --example headless --quiet
native_bin=target/release/examples/headless

echo "== native run (${#roms[@]} ROMs x $FRAMES frames)"
for i in "${!roms[@]}"; do
    "$native_bin" "${roms[$i]}" --frames "$FRAMES" --four-score \
        --input "$work/inputs/$i.txt" --hash-log "$work/native/$i.log" > /dev/null
done

echo "== linux/amd64 build + run (Docker)"
mounts=()
cmds="cargo build --release -p ebobo-core --example headless --locked --quiet"
for i in "${!roms[@]}"; do
    rom_abs="$(cd "$(dirname "${roms[$i]}")" && pwd)/$(basename "${roms[$i]}")"
    mounts+=(-v "$rom_abs:/roms/$i.nes:ro")
    cmds+=" && /target/release/examples/headless /roms/$i.nes --frames $FRAMES --four-score \
--input /work/inputs/$i.txt --hash-log /work/linux/$i.log > /dev/null"
done
docker run --rm --platform linux/amd64 \
    -v "$PWD":/src \
    -v "$work":/work \
    "${mounts[@]}" \
    -v ebobo-linux-target:/target \
    -v ebobo-linux-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target \
    -w /src \
    "$image" \
    sh -c "uname -m && $cmds"

fail=0
for i in "${!roms[@]}"; do
    a="$work/native/$i.log"
    b="$work/linux/$i.log"
    name="${roms[$i]}"
    na=$(wc -l < "$a" | tr -d ' ')
    nb=$(wc -l < "$b" | tr -d ' ')
    if cmp -s "$a" "$b" && [ "$na" = "$FRAMES" ]; then
        echo "PASS $name ($na frames, final $(tail -n1 "$a" | cut -d' ' -f2))"
    else
        first=$(diff <(cat "$a") <(cat "$b") | grep -m1 -E '^[<>]' | awk '{print $2}' || true)
        echo "FAIL $name: first differing frame ${first:-?} (native $na lines, linux $nb lines)"
        fail=1
    fi
done
if [ "$fail" = 0 ]; then echo "PASS: all ${#roms[@]} ROMs identical on macOS $(uname -m) and linux/amd64"; else echo "FAIL"; fi
exit "$fail"
