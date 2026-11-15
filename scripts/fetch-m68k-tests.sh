#!/usr/bin/env bash
# Fetch the 68000 single-step tests the ebobo-m68k harnesses read into crates/ebobo-m68k/tests/data.
# They are not kept in this repo. Two suites, each at a pinned commit:
# - https://github.com/SingleStepTests/680x0 (folder 68000/v1) for tests/singlestep.rs, into DEST:
#   124 gzipped JSON files, one per instruction, about 8000 cases each: 203 MB to download.
# - https://github.com/SingleStepTests/m68000 (folder v1) for tests/mame.rs, into DEST/mame:
#   generated from MAME's microcoded 68000, *.json.bin files (a compact binary form of the JSON,
#   defined by that repo's decode.py). The size is printed at the end of the run.
# Files already present are kept, so an interrupted run can be repeated.
#
# Usage: scripts/fetch-m68k-tests.sh [DEST]
#   DEST   defaults to crates/ebobo-m68k/tests/data
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/crates/ebobo-m68k/tests/data}

fetch_one() {
    local f=$1 dest=$2 base=$3
    [ -s "$dest/$f" ] && return 0
    curl -fsSL --retry 3 -o "$dest/$f.part" "$base/$f" && mv "$dest/$f.part" "$dest/$f"
}
export -f fetch_one

# fetch_suite REPO COMMIT DIR SUFFIX DEST: every DIR/*SUFFIX of REPO at COMMIT into DEST.
fetch_suite() {
    local repo=$1 commit=$2 dir=$3 suffix=$4 dest=$5 files want got
    mkdir -p "$dest"
    echo "fetching https://github.com/$repo @ $commit ($dir)"
    # The file list comes from the tree of the pinned commit (one API call), the files from raw URLs.
    files=$(curl -fsSL "https://api.github.com/repos/$repo/git/trees/$commit?recursive=1" |
        tr ',{' '\n\n' | sed -n "s|.*\"path\": *\"$dir/\([^\"/]*${suffix//./\\.}\)\".*|\1|p")
    [ -n "$files" ] || { echo "error: no files listed for $dir" >&2; exit 1; }
    printf '%s\n' $files | xargs -P 8 -I{} bash -c 'fetch_one "$@"' _ {} "$dest" "https://raw.githubusercontent.com/$repo/$commit/$dir"
    want=$(printf '%s\n' $files | wc -l | tr -d ' ')
    got=$(find "$dest" -maxdepth 1 -name "*$suffix" | wc -l | tr -d ' ')
    [ "$got" -ge "$want" ] || { echo "error: $got of $want files in $dest" >&2; exit 1; }
    echo "$got test files in $dest ($(du -sh "$dest" | cut -f1) with subfolders)"
}

fetch_suite SingleStepTests/680x0 e0d5ece9670205cc84a0101081837deb446f86a3 68000/v1 .json.gz "$dest"
fetch_suite SingleStepTests/m68000 64b253116a3de04aaac4346c43680960dc9b67e5 v1 .json.bin "$dest/mame"
