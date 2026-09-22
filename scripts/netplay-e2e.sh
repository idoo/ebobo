#!/usr/bin/env bash
# End-to-end online netplay check: a host and N-1 guests (each with an empty
# ROM folder, so the ROM must be transferred) play against the live room
# server; PASS when every peer starts the game with the same slots and every
# `frame N hash H` line printed by two or more peers agrees across all of them.
# Inputs stay idle (the app has no input-script hook); the title/attract
# screens still advance the state, which is enough for the hash comparison.
# Usage: scripts/netplay-e2e.sh [--relay] [--players N] [--rom PATH]
set -u
cd "$(dirname "$0")/.."

usage() { echo "usage: $0 [--relay] [--players 2-4] [--rom PATH]" >&2; exit 2; }
relay=()
players=2
rom=""
while [ $# -gt 0 ]; do
  case "$1" in
    --relay) relay=(--relay) ;;
    --players) [ $# -ge 2 ] || usage; players="$2"; shift ;;
    --rom) [ $# -ge 2 ] || usage; rom="$2"; shift ;;
    *) usage ;;
  esac
  shift
done
case "$players" in 2|3|4) ;; *) usage ;; esac
if [ -n "$rom" ] && [ ! -f "$rom" ]; then echo "FAIL: no ROM at $rom"; exit 1; fi

export CARGO_TARGET_DIR=target-fe
cargo build --release -p ebobo || { echo FAIL: build; exit 1; }
bin="$CARGO_TARGET_DIR/release/ebobo"

if [ -z "$rom" ]; then
  rom="crates/ebobo-core/tests/roms/nestest/nestest.nes"
  [ -f "$rom" ] || { echo "FAIL: no ROM at $rom (run scripts/fetch-test-roms.sh or pass --rom)"; exit 1; }
fi
echo "ROM: $rom"
echo "players: $players"

# --exit-after counts wall-clock seconds from launch. Two players keep the
# original 25 s / 20 s; with more, guests join one second apart and the game
# starts only when the last one is in (after the ROM transfer), so the last
# guest gets 34 s (≥ 25 s of game) and every earlier peer one second more per
# later joiner (host 5 s more on top).
if [ "$players" -eq 2 ]; then
  host_exit=25
  guest_exit() { echo 20; }
else
  host_exit=$((39 + players - 1))
  guest_exit() { echo $((34 + players - 1 - $1)); }
fi

tmp=$(mktemp -d)
names=(host)
logs=("$tmp/host.log")
pids=()
cleanup() {
  for p in ${pids[@]+"${pids[@]}"}; do kill "$p" 2>/dev/null; done
  rm -rf "$tmp"
}
trap cleanup EXIT

"$bin" --windowed --host "$rom" --name Host --autostart "$players" --exit-after "$host_exit" \
  --screenshot /tmp/e2e-host.png ${relay[@]+"${relay[@]}"} >"${logs[0]}" 2>&1 &
host_pid=$!
pids+=("$host_pid")

code=""
for _ in $(seq 1 150); do
  code=$(sed -n 's/^ebobo: room \([A-Z0-9]*\).*/\1/p' "${logs[0]}" | head -n 1)
  [ -n "$code" ] && break
  kill -0 "$host_pid" 2>/dev/null || break
  sleep 0.1
done
if [ -z "$code" ]; then
  echo "--- host"; cat "${logs[0]}"
  echo "FAIL: no room code from host"
  exit 1
fi
echo "room: $code"

for i in $(seq 1 $((players - 1))); do
  if [ "$players" -eq 2 ]; then name=guest; label=Guest; shot=/tmp/e2e-guest.png
  else name="guest$i"; label="Guest$i"; shot="/tmp/e2e-guest$i.png"; fi
  home="$tmp/$name-home"
  mkdir -p "$home"
  log="$tmp/$name.log"
  names+=("$name")
  logs+=("$log")
  HOME="$home" "$bin" --windowed --join "$code" --name "$label" --exit-after "$(guest_exit "$i")" \
    --screenshot "$shot" ${relay[@]+"${relay[@]}"} >"$log" 2>&1 &
  pids+=($!)
  [ "$i" -lt $((players - 1)) ] && sleep 1
done

for p in "${pids[@]}"; do wait "$p"; done
pids=()

for k in "${!names[@]}"; do echo "--- ${names[$k]}"; cat "${logs[$k]}"; done

fail=""
slots=""
for k in "${!names[@]}"; do
  line=$(grep -m1 '^ebobo: game started' "${logs[$k]}")
  if [ -z "$line" ]; then
    fail="${fail:+$fail; }${names[$k]} never started the game"
    continue
  fi
  s=$(echo "$line" | sed -n 's/.*slots=\(\[[^]]*\]\).*/\1/p')
  if [ -z "$slots" ]; then slots="$s"
  elif [ "$s" != "$slots" ]; then fail="${fail:+$fail; }${names[$k]} slots=$s differ from slots=$slots"; fi
done

# "frame peer hash" rows from every log; a frame counts when 2+ peers print it.
for k in "${!names[@]}"; do
  sed -n "s/^ebobo: frame \([0-9]*\) hash \([0-9a-f]*\)\$/\1 ${names[$k]} \2/p" "${logs[$k]}"
done | sort -n >"$tmp/all"
shared=$(awk '{ n[$1]++ } END { for (f in n) if (n[f] > 1) c++; print c + 0 }' "$tmp/all")
bad=$(awk '
  { n[$1]++; row[$1] = row[$1] " " $2 "=" $3
    if (!($1 in h)) h[$1] = $3; else if (h[$1] != $3) diff[$1] = 1 }
  END { for (f in diff) if (n[f] > 1) print "frame " f ":" row[f] }' "$tmp/all" | sort -n -k2)
if [ -z "$fail" ]; then
  if [ "$shared" -eq 0 ]; then
    fail="no frame printed by two peers"
  elif [ -n "$bad" ]; then
    fail="hash mismatch: $bad"
  fi
fi

routes=""
for k in "${!names[@]}"; do
  routes="${routes:+$routes }${names[$k]}[$(sed -n 's/.*route=//p' "${logs[$k]}")]"
done
echo "routes: $routes"
if [ -n "$fail" ]; then
  echo "FAIL: $fail"
  exit 1
fi
echo "PASS: $shared shared frames agree${slots:+ (slots=$slots)}"
