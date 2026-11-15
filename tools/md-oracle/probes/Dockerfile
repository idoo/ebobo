# Toolchain for the probe ROMs: GNU as and ld for the 68000, z80asm for the Z80.
# Build once: docker build -t ebobo-md-probes tools/md-oracle/probes
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends binutils-m68k-linux-gnu z80asm make python3 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /work
