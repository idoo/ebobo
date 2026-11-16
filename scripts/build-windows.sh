#!/usr/bin/env bash
# Cross-builds the Windows x86_64 release in Docker (mingw-w64) and packs dist/ebobo-windows.zip
# (ebobo.exe + README). `scripts/build-windows.sh check` only type-checks the Windows build,
# which is the quick way to see whether `cfg(windows)` code compiles.
set -euo pipefail
cd "$(dirname "$0")/.."

image=ebobo-windows-build
docker build --platform linux/amd64 -t "$image" packaging/windows

target=x86_64-pc-windows-gnu
run() {
    docker run --rm --platform linux/amd64 \
        -v "$PWD":/src \
        -v ebobo-windows-target:/target \
        -v ebobo-windows-cargo:/usr/local/cargo/registry \
        -e CARGO_TARGET_DIR=/target \
        "$image" sh -c "$1"
}

if [ "${1:-}" = check ]; then
    run "cargo check -p ebobo --locked --target $target"
    exit
fi

# -static: libgcc and winpthreads go into the exe, so it starts on a PC without mingw's DLLs.
run "RUSTFLAGS='-C link-arg=-static' cargo build --release -p ebobo --locked --target $target \
    && mkdir -p /src/dist/windows \
    && cp /target/$target/release/ebobo.exe /src/dist/windows/ebobo.exe \
    && x86_64-w64-mingw32-strip /src/dist/windows/ebobo.exe \
    && x86_64-w64-mingw32-objdump -p /src/dist/windows/ebobo.exe | sed -n 's/.*DLL Name: //p' | sort -u > /src/dist/windows/dlls.txt"

stage=dist/ebobo-windows
rm -rf "$stage" dist/ebobo-windows.zip
mkdir -p "$stage"
cp dist/windows/ebobo.exe "$stage/ebobo.exe"
cp README.md "$stage/README.md" 2>/dev/null || true
(cd dist && COPYFILE_DISABLE=1 zip -q -r -X ebobo-windows.zip ebobo-windows)
echo "dist/ebobo-windows.zip"
