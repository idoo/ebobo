# Linux x86_64 build environment for ebobo (Steam Deck / SteamOS and other distros).
# Debian 12 → glibc 2.36: runs on SteamOS 3.x (glibc 2.38+), Debian 12+, Ubuntu 23.04+, Fedora 37+.
# SDL2 is compiled from source by sdl2-sys ("bundled" + "static-link"); its X11/Wayland/ALSA/
# Pulse/udev backends are dlopen'ed at runtime, the -dev packages here only provide headers.
FROM --platform=linux/amd64 rust:1-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        cmake ninja-build pkg-config \
        libx11-dev libxext-dev libxrandr-dev libxcursor-dev libxi-dev libxss-dev \
        libxfixes-dev libxinerama-dev libxkbcommon-dev \
        libwayland-dev wayland-protocols libegl1-mesa-dev libgl1-mesa-dev libgles2-mesa-dev \
        libdrm-dev libgbm-dev \
        libasound2-dev libpulse-dev libudev-dev libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
