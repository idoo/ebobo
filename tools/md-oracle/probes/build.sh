#!/bin/sh
# Builds every probe (*.s except rt.s) into out/<name>.bin with the cross tools of the Docker
# image `ebobo-md-probes` (docker build -t ebobo-md-probes tools/md-oracle/probes).
# Each image is padded with 0xFF to a multiple of 512 bytes, at least 1024.
set -eu
cd "$(dirname "$0")"
docker run --rm -v "$PWD":/work -w /work ebobo-md-probes sh -c '
set -eu
mkdir -p out build
AS="m68k-linux-gnu-as -m68000 --register-prefix-optional -I ."
$AS -o build/rt.o rt.s
for src in *.s; do
    name=${src%.s}
    if [ "$name" = rt ]; then continue; fi
    $AS -o build/$name.o $src
    m68k-linux-gnu-ld -T link.ld -o build/$name.elf build/rt.o build/$name.o
    m68k-linux-gnu-objcopy -O binary build/$name.elf out/$name.bin
    python3 -c "
import sys
path = sys.argv[1]
data = open(path, \"rb\").read()
size = max(1024, -(-len(data) // 512) * 512)
open(path, \"wb\").write(data + b\"\\xff\" * (size - len(data)))
print(path, len(data), \"->\", size, \"bytes\")
" out/$name.bin
done
rm -rf build
' 2>&1 | grep -v config.json
