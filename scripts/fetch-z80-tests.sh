#!/usr/bin/env bash
# Fetch the SingleStepTests Z80 suite the ebobo-z80 tests read into crates/ebobo-z80/tests/data.
# It comes from https://github.com/SingleStepTests/z80 (folder v1) at a pinned commit and is
# not kept in this repo.
#
# Size: 1604 JSON files (one per opcode, 1000 cases each), 1.4 GB on disk; the download is
# the commit tarball, about 0.3 GB, streamed straight into tar (nothing else is stored).
#
# Usage: scripts/fetch-z80-tests.sh [DEST]
#   DEST            defaults to crates/ebobo-z80/tests/data
#   Z80_TESTS_SRC   an existing checkout of that commit to copy from (skips the download)
set -euo pipefail

REPO=https://github.com/SingleStepTests/z80
COMMIT=ebe1875d48f374bcfd4b505d8eb8ee751568b5f7
FILES=1604

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/crates/ebobo-z80/tests/data}

count() { find "$1" -maxdepth 1 -name '*.json' 2>/dev/null | wc -l | tr -d ' '; }

if [ "$(count "$dest")" = "$FILES" ]; then
    echo "$FILES test files already in $dest"
    exit 0
fi

# Unpack beside the destination and move into place, so an interrupted run leaves no half set.
mkdir -p "$(dirname "$dest")"
tmp=$(mktemp -d "$(dirname "$dest")/.z80-fetch.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

src=${Z80_TESTS_SRC:-}
if [ -n "$src" ]; then
    if [ -d "$src/.git" ]; then
        got=$(git -C "$src" rev-parse HEAD)
        [ "$got" = "$COMMIT" ] || { echo "error: $src is at $got, want $COMMIT" >&2; exit 1; }
    fi
    cp "$src"/v1/*.json "$tmp/"
else
    echo "fetching $REPO @ $COMMIT (1.4 GB unpacked)"
    curl -fsSL "$REPO/archive/$COMMIT.tar.gz" | tar -xz -C "$tmp" --strip-components 2 "z80-$COMMIT/v1"
fi

n=$(count "$tmp")
[ "$n" = "$FILES" ] || { echo "error: got $n files, want $FILES" >&2; exit 1; }
rm -rf "$dest"
mv "$tmp" "$dest"
chmod 755 "$dest"
echo "$n test files in $dest"
