#!/usr/bin/env bash
# Builds the Linux x86_64 release in Docker and packs dist/ebobo-linux-x86_64.tar.gz
# (binary + Steam Deck installer). Works from an Apple Silicon Mac (amd64 emulation).
set -euo pipefail
cd "$(dirname "$0")/.."

image=ebobo-linux-build
docker build --platform linux/amd64 -t "$image" packaging/linux

# Target dir and cargo registry live in named volumes: fast, and Linux artifacts
# never mix with the host's target/.
docker run --rm --platform linux/amd64 \
    -v "$PWD":/src \
    -v ebobo-linux-target:/target \
    -v ebobo-linux-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target \
    "$image" \
    sh -c 'cargo build --release -p ebobo --locked && mkdir -p /src/dist/linux && cp /target/release/ebobo /src/dist/linux/ebobo && strip /src/dist/linux/ebobo'

stage=dist/ebobo-steamdeck
rm -rf "$stage"
mkdir -p "$stage"
cp dist/linux/ebobo "$stage/ebobo"
cp packaging/linux/install-steamdeck.sh "$stage/install.sh"
cp packaging/icon/ebobo.png "$stage/ebobo.png" 2>/dev/null || true
cp README.md "$stage/README.md" 2>/dev/null || true
mkdir -p "$stage/steam"
cp packaging/steam/*.png "$stage/steam/"
chmod +x "$stage/ebobo" "$stage/install.sh"
# bsdtar on macOS would embed com.apple.provenance xattrs that GNU tar warns about.
COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata -C dist -czf dist/ebobo-steamdeck.tar.gz ebobo-steamdeck
cp packaging/linux/install-online.sh dist/install-steamdeck.sh
echo "dist/ebobo-steamdeck.tar.gz dist/install-steamdeck.sh"
