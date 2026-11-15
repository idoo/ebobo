#!/usr/bin/env bash
# Fetch freely distributable Mega Drive / Genesis ROMs for the ebobo-md tests into
# crates/ebobo-md/tests/roms: SGDK sample programs, demoscene demos and homebrew games whose
# authors publish the ROM themselves. No commercial games. Nothing is kept in this repo.
#
# Usage: scripts/fetch-md-test-roms.sh [DEST]
#   DEST   defaults to crates/ebobo-md/tests/roms
#
# Every file is pinned by SHA-256 (and by tag or commit in the URL where the host allows).
# A file already there with the right hash is skipped; one that fails to download or to match
# is reported and skipped. Exit status 1 when DEST ends up with none of the files.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/crates/ebobo-md/tests/roms}
mkdir -p "$dest"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

SGDK=https://raw.githubusercontent.com/Stephane-D/SGDK/v2.00/sample
SIK=https://raw.githubusercontent.com/sikthehedgehog

sha256() {
    if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1; else sha256sum "$1" | cut -d' ' -f1; fi
}

# "SEGA" somewhere in the 16-byte console name at 0x100.
has_sega() { dd if="$1" bs=1 skip=256 count=16 2>/dev/null | LC_ALL=C grep -q SEGA; }

# The domestic game name of the header (48 bytes at 0x120), trailing blanks cut.
header_name() { dd if="$1" bs=1 skip=288 count=48 2>/dev/null | LC_ALL=C tr -c '[:print:]' ' ' | sed 's/ *$//'; }

# fetch_one NAME URL SHA [ZIP_MEMBER]: prints one status line; non-zero when the file is not in DEST afterwards.
fetch_one() {
    local name=$1 url=$2 want=$3 member=${4:-} out=$dest/$1 dl=$tmp/dl got
    if [ -f "$out" ] && [ "$(sha256 "$out")" = "$want" ]; then
        echo "have    $name"
        return 0
    fi
    if ! curl -fsSL --retry 2 --max-time 300 -o "$dl" "$url"; then
        echo "FAILED  $name: download failed ($url)"
        return 1
    fi
    if [ -n "$member" ]; then
        if ! unzip -p "$dl" "$member" >"$tmp/rom" 2>/dev/null; then
            echo "FAILED  $name: '$member' is not in the archive"
            return 1
        fi
        mv "$tmp/rom" "$dl"
    fi
    got=$(sha256 "$dl")
    if [ "$got" != "$want" ]; then
        echo "FAILED  $name: sha256 $got, want $want"
        return 1
    fi
    if ! has_sega "$dl"; then
        echo "FAILED  $name: no SEGA at 0x100"
        return 1
    fi
    mv "$dl" "$out"
    echo "ok      $name"
}

# One file per line, tab separated: name, URL, SHA-256, member inside the zip (when the URL is a zip).
# The comment before each line: what it is; author; licence or permission; page.
names=()
failed=0
while IFS=$'\t' read -r name url sha member; do
    case $name in '' | '#'*) continue ;; esac
    names+=("$name")
    fetch_one "$name" "$url" "$sha" "$member" || failed=$((failed + 1))
done <<EOF
# SGDK samples: Stephane Dallongeville and contributors; MIT (SGDK license.txt); https://github.com/Stephane-D/SGDK/tree/v2.00/sample
# joy-test: 3- and 6-button pads, Team Player and EA 4-Way multitaps, mouse, port identification.
sgdk-joy-test.bin	$SGDK/joy-test/out/rom.bin	71f8f0d148c61764f224f31d813dcd7c183ea2749890580100836f73cd845d6c
# fx/hilight-shadow: shadow/highlight mode with sprites as operators.
sgdk-hilight-shadow.bin	$SGDK/fx/hilight-shadow/out/rom.bin	f2678d44dd487b6fe49ae96981a670cdf535159552811331892e9072500bc4c0
# fx/h-int/wobble: horizontal interrupt every line changing scroll (raster effect).
sgdk-hint-wobble.bin	$SGDK/fx/h-int/wobble/out/rom.bin	e27714d12f294a2096f82e9640ba47d5f2c6ae9374f420358bc284e21958719f
# fx/h-int/scaling: horizontal interrupt changing vertical scroll per line.
sgdk-hint-scaling.bin	$SGDK/fx/h-int/scaling/out/rom.bin	f5cd7c6dba4ca0b296ad115cc5ca633941a5820051d7649dd87172c798529376
# fx/scroll/linescroll: per-line horizontal scroll table.
sgdk-linescroll.bin	$SGDK/fx/scroll/linescroll/out/rom.bin	84627c69511c94ea9228adf819212c882046d20ea6d4f2bd9fef769556c64b57
# game/platformer: a small platformer; sprites, scrolling planes, DMA queue, pad.
sgdk-platformer.bin	$SGDK/game/platformer/out/rom.bin	36ff8ed2217427c105e9babd96fe797fbcdb16adadcf56347e6de052c883dbdf
# benchmark: 68000 arithmetic, memory, DMA and fill, sprite and tile-map throughput, with on-screen numbers.
sgdk-benchmark.bin	$SGDK/benchmark/out/rom.bin	b67b6fac32f13fa04b2e7a3428ac96fabbedb0ee9919eb0e0d4987bb9eb3ff1e
# bitmap/cube 3D: software bitmap mode, heavy 68000 maths and DMA uploads every frame.
sgdk-cube3d.bin	$SGDK/bitmap/cube%203D/out/rom.bin	eb32bf2e1f095747a528bc47f6197b7fb992a728a86c33210f34e6ea56f9423e
# demo/starfield-donut: many sprites, plane scrolling.
sgdk-starfield-donut.bin	$SGDK/demo/starfield-donut/out/rom.bin	d33ba328c2369a373545719e40f10881f7a624979600361598bbb0f89a37928e
# flash-save: save data written to cartridge memory.
sgdk-flash-save.bin	$SGDK/flash-save/out/rom.bin	b5814e4a2e5a59ee48619d733aede5de07207e11628b00034723af4da9cb8a5f
# sys/console: text console on a plane, window plane.
sgdk-console.bin	$SGDK/sys/console/out/rom.bin	698a116ac77a8c411b45752c5b938455f31778bb3e5b7180b2367cb5248e83e4
# Cave Story MD 0.8.8 (English): fan port by andwn of Pixel's freeware game, free release by the author; battery RAM, 6-button pad, FM+PSG+PCM, 4 MB; https://github.com/andwn/cave-story-md/releases/tag/v0.8.8
cave-story-md.gen	https://github.com/andwn/cave-story-md/releases/download/v0.8.8/doukutsu-en.gen	053ed9009b47512972d7fd0954f3c251774422be8be6eb8df90f39fe22b50061
# KleleAtoms MD 1.2.1: puzzle/strategy game for up to four players by Nightwolf-47; MIT; https://github.com/Nightwolf-47/KleleAtoms-MD/releases/tag/v1.2.1
kleleatoms.bin	https://github.com/Nightwolf-47/KleleAtoms-MD/releases/download/v1.2.1/kleleatoms-md-121.bin	d0660df5af9d39696272b0c6774f25f296ee38e86eacf8b52645eab3ea8b003a
# Miniplanets REMIX (REV04): puzzle platformer by Sik (Javier Degirolmo); zlib licence, ROM in the author's repository; https://github.com/sikthehedgehog/miniplanets
miniplanets.bin	$SIK/miniplanets/0ac9c395289cc5d0837c96d45e61a27300e8eebc/Miniplanets%20REMIX%20Ver%20(REV04).bin	e8aee1b9fedca87b638a7624709d7f6e1a2aff8a5222083346d1cd63963acbe7
# Dragon's Castle (prototype): falling-block puzzle game by Sik; zlib licence, ROM in the author's repository; https://github.com/sikthehedgehog/dragon
dragons-castle.bin	$SIK/dragon/72ef51b648ee1e5bd1a7e9c6731b95f16cce7f28/witch.bin	c44c1d349ac7835b9a410ef3f58e9800897726c72be646208cf62dc9c8e0b975
# Project MD (2012-04-29 build): platformer by Sik, Echo sound driver (Z80, FM, PSG, PCM); ROM published by the author in the game's official repository; https://github.com/sikthehedgehog/projectmd
project-md.bin	$SIK/projectmd/c1d77eb640a9b4c14d5eb4b8f8a2779ea97cb856/bin/projmd-20120429.bin	d7df62401fb446563cb2fe4f27d104bb39c0e1a746a4451ab27d789ba552dbee
# Masiaka (2017): demo by Resistance, released free by the group with its source; https://github.com/ResistanceVault/demo-Masiaka/releases/tag/party-version-cleanup
masiaka.bin	https://github.com/ResistanceVault/demo-Masiaka/releases/download/party-version-cleanup/Masiaka-by-Resistance-2017.zip	b6b122107cd3edefdcc7f81e913fd323a862de718d3e83c0c34db5b2ccf0813b	Masiaka-by-Resistance-2017.bin
# Overdrive 1.1 final (hardware version): demo by Titan, Evoke 2013, free scene release uploaded by the group; raster and VDP timing tricks, shadow/highlight, PCM; https://www.pouet.net/prod.php?which=61724
titan-overdrive.bin	https://files.scene.org/get/parties/2013/evoke13/wild/titan-overdrivemegademo-v1.1-final.zip	8c093b911a154d0fc8767724ca2bca16ed4c3383c2115b3fe366c471b6015f70	TiTAN - Overdrive (Rev1.1-106-Final) (Hardware).bin
# Overdrive 2: demo by Titan, Revision 2017, free scene release uploaded by the group; 8 MB with the SSF bank mapper, extreme VDP timing (FIFO, mid-line changes, border); https://www.pouet.net/prod.php?which=69648
titan-overdrive2.bin	https://files.scene.org/get/parties/2017/revision17/oldskool_demo/titan-overdrive2.zip	f722fc342bbd7008a743b9970e9ccaa7f2891c4b841126cb98d5dad93da45611	titan-overdrive2.bin
EOF

echo
echo "in $dest:"
have=0
for name in "${names[@]}"; do
    [ -f "$dest/$name" ] || continue
    printf '  %-26s %8d bytes  %s\n' "$name" "$(wc -c <"$dest/$name")" "$(header_name "$dest/$name")"
    have=$((have + 1))
done
echo "$have of ${#names[@]} files present, $failed failed"
[ "$have" -gt 0 ]
