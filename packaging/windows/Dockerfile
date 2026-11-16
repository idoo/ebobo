# Windows x86_64 cross-build environment for ebobo: Rust's x86_64-pc-windows-gnu target with
# mingw-w64. SDL2 is compiled from source by sdl2-sys ("bundled" + "static-link") and linked in,
# so the result is one ebobo.exe that needs only DLLs every Windows 10 has.
FROM --platform=linux/amd64 rust:1-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake ninja-build mingw-w64 zip \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-pc-windows-gnu

WORKDIR /src
