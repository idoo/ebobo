#!/usr/bin/env bash
# Mac <-> Steam Deck determinism check: runs both cores' headless examples natively and in a
# linux/amd64 Docker container on the same ROMs and pseudo-random 4-pad input scripts,
# then compares the per-frame state hashes. Prints PASS/FAIL per ROM; exits 1 on any FAIL.
#
# Usage: scripts/determinism-check.sh [EXTRA_ROM ...]
# Runs the fetched test ROMs (scripts/fetch-test-roms.sh, scripts/fetch-md-test-roms.sh) plus
# any ROM paths given; a `.nes` file runs in the NES core, anything else in the Mega Drive core.
# ROMs are bind-mounted read-only into the container; nothing is copied into the repo.
# Generated inputs, binaries and logs live in target/determinism (gitignored).
set -euo pipefail
cd "$(dirname "$0")/.."

FRAMES=${FRAMES:-2000}
image=ebobo-linux-build
work="$PWD/target/determinism"
rm -rf "$work"
mkdir -p "$work/native" "$work/linux" "$work/inputs" "$work/bin-native" "$work/bin-linux"

roms=()
add() { [ -f "$1" ] && roms+=("$1") || echo "skip (missing): $1"; }
R=crates/ebobo-core/tests/roms
add "$R/nestest/nestest.nes"
add "$R/instr_test-v5/official_only.nes"
add "$R/cpu_interrupts_v2/cpu_interrupts.nes"
add "$R/apu_test/apu_test.nes"
add "$R/MMC1_A12/mmc1_a12.nes"
add "$R/mmc3_test_2/rom_singles/4-scanline_timing.nes"
M=crates/ebobo-md/tests/roms
add "$M/cave-story-md.gen"
add "$M/sgdk-platformer.bin"
add "$M/titan-overdrive.bin"
add "$M/miniplanets.bin"
add "$M/masiaka.bin"
add "$M/sgdk-hilight-shadow.bin"
for f in "$@"; do
    add "$f"
done
[ "${#roms[@]}" -gt 0 ] || { echo "no ROMs: run scripts/fetch-test-roms.sh and scripts/fetch-md-test-roms.sh, or pass ROM paths"; exit 1; }

is_nes() { case "$1" in *.nes | *.NES) return 0 ;; *) return 1 ;; esac; }

# The command line of one run: `run_cmd BIN_DIR ROM INPUT LOG`. The NES example writes one hash
# per frame into its log; the Mega Drive example prints `frame N state H` lines.
run_cmd() {
    if is_nes "$2"; then
        echo "$1/headless-nes '$2' --frames $FRAMES --four-score --input '$3' --hash-log '$4' > /dev/null"
    else
        echo "$1/headless-md '$2' --frames $FRAMES --multitap --input '$3' --state-hash-every 1 > '$4'"
    fi
}

# Frames a log holds a state hash for.
hashed() {
    if is_nes "$1"; then wc -l < "$2" | tr -d ' '; else grep -c ' state ' "$2" || true; fi
}

# Deterministic input script per ROM (xorshift32 seeded by index): a new random word for each
# of the 4 pads every 1-8 frames; 8 pad bits on the NES, 12 on the Mega Drive (whose frames
# count from 1).
for i in "${!roms[@]}"; do
    if is_nes "${roms[$i]}"; then bits=8; else bits=12; fi
    python3 - "$i" "$FRAMES" "$bits" > "$work/inputs/$i.txt" <<'EOF'
import sys
i, frames, bits = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
s = (0x9E3779B9 ^ (i * 0x85EBCA6B + 1)) & 0xFFFFFFFF or 1
def rnd():
    global s
    s ^= (s << 13) & 0xFFFFFFFF; s ^= s >> 17; s ^= (s << 5) & 0xFFFFFFFF
    return s
f = 0 if bits == 8 else 1
while f < frames:
    print(f, *("%0*x" % ((bits + 3) // 4, rnd() & ((1 << bits) - 1)) for _ in range(4)))
    f += 1 + rnd() % 8
EOF
done

# Both examples are called `headless`, so each is copied aside before the other is built.
build="cargo build --release --example headless --quiet"

echo "== native build"
native_target="${CARGO_TARGET_DIR:-target}"
$build -p ebobo-core
cp "$native_target/release/examples/headless" "$work/bin-native/headless-nes"
$build -p ebobo-md
cp "$native_target/release/examples/headless" "$work/bin-native/headless-md"

echo "== native run (${#roms[@]} ROMs x $FRAMES frames)"
for i in "${!roms[@]}"; do
    eval "$(run_cmd "$work/bin-native" "${roms[$i]}" "$work/inputs/$i.txt" "$work/native/$i.log")"
done

echo "== linux/amd64 build + run (Docker)"
mounts=()
cmds="$build --locked -p ebobo-core && cp /target/release/examples/headless /work/bin-linux/headless-nes"
cmds+=" && $build --locked -p ebobo-md && cp /target/release/examples/headless /work/bin-linux/headless-md"
for i in "${!roms[@]}"; do
    rom="${roms[$i]}"
    rom_abs="$(cd "$(dirname "$rom")" && pwd)/$(basename "$rom")"
    inside="/roms/$i.${rom##*.}"
    mounts+=(-v "$rom_abs:$inside:ro")
    cmds+=" && $(run_cmd /work/bin-linux "$inside" "/work/inputs/$i.txt" "/work/linux/$i.log")"
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
    na=$(hashed "$name" "$a")
    nb=$(hashed "$name" "$b")
    if cmp -s "$a" "$b" && [ "$na" = "$FRAMES" ]; then
        echo "PASS $name ($na frames, final $(tail -n1 "$a" | awk '{print $NF}'))"
    else
        first=$(diff "$a" "$b" | grep -m1 -E '^[<>]' | cut -c3- || true)
        echo "FAIL $name: first differing line \"${first:-?}\" (native $na frames, linux $nb frames)"
        fail=1
    fi
done
if [ "$fail" = 0 ]; then echo "PASS: all ${#roms[@]} ROMs identical on macOS $(uname -m) and linux/amd64"; else echo "FAIL"; fi
exit "$fail"
