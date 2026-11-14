#!/usr/bin/env bash
# Fetch the 68000 single-step tests the ebobo-m68k harness (tests/singlestep.rs) reads into
# crates/ebobo-m68k/tests/data. They come from https://github.com/SingleStepTests/680x0
# (folder 68000/v1) at a pinned commit and are not kept in this repo.
# 124 gzipped JSON files, one per instruction, about 8000 cases each: 203 MB to download.
# Files already present are kept, so an interrupted run can be repeated.
#
# Usage: scripts/fetch-m68k-tests.sh [DEST]
#   DEST   defaults to crates/ebobo-m68k/tests/data
set -euo pipefail

REPO=SingleStepTests/680x0
COMMIT=e0d5ece9670205cc84a0101081837deb446f86a3
DIR=68000/v1

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/crates/ebobo-m68k/tests/data}
mkdir -p "$dest"

echo "fetching https://github.com/$REPO @ $COMMIT ($DIR)"
# The file list comes from the tree of the pinned commit (one API call), the files from raw URLs.
files=$(curl -fsSL "https://api.github.com/repos/$REPO/git/trees/$COMMIT?recursive=1" |
    tr ',{' '\n\n' | sed -n "s|.*\"path\": *\"$DIR/\([^\"/]*\.json\.gz\)\".*|\1|p")
[ -n "$files" ] || { echo "error: no files listed for $DIR" >&2; exit 1; }

fetch_one() {
    local f=$1 dest=$2 base=$3
    [ -s "$dest/$f" ] && return 0
    curl -fsSL --retry 3 -o "$dest/$f.part" "$base/$f" && mv "$dest/$f.part" "$dest/$f"
}
export -f fetch_one
printf '%s\n' $files | xargs -P 8 -I{} bash -c 'fetch_one "$@"' _ {} "$dest" "https://raw.githubusercontent.com/$REPO/$COMMIT/$DIR"

want=$(printf '%s\n' $files | wc -l | tr -d ' ')
got=$(find "$dest" -name '*.json.gz' | wc -l | tr -d ' ')
[ "$got" -ge "$want" ] || { echo "error: $got of $want files in $dest" >&2; exit 1; }
echo "$got test files in $dest"
