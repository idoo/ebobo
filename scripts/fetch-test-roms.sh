#!/usr/bin/env bash
# Fetch the test ROMs the ebobo-core tests read into crates/ebobo-core/tests/roms.
# They come from https://github.com/christopherpow/nes-test-roms at a pinned commit
# and are not kept in this repo.
#
# Usage: scripts/fetch-test-roms.sh [DEST]
#   DEST                defaults to crates/ebobo-core/tests/roms
#   NES_TEST_ROMS_SRC   an existing checkout of that commit to copy from (skips the download)
set -euo pipefail

REPO=https://github.com/christopherpow/nes-test-roms
COMMIT=95d8f621ae55cee0d09b91519a8989ae0e64753b

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/crates/ebobo-core/tests/roms}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

src=${NES_TEST_ROMS_SRC:-}
if [ -z "$src" ]; then
    src=$tmp/nes-test-roms
    echo "fetching $REPO @ $COMMIT"
    if git init -q "$src" && git -C "$src" fetch -q --depth 1 "$REPO" "$COMMIT" 2>/dev/null; then
        git -C "$src" -c advice.detachedHead=false checkout -q FETCH_HEAD
    else
        rm -rf "$src" && mkdir -p "$src"
        curl -fsSL "$REPO/archive/$COMMIT.tar.gz" | tar -xz -C "$src" --strip-components 1
    fi
fi
if [ -d "$src/.git" ]; then
    got=$(git -C "$src" rev-parse HEAD)
    [ "$got" = "$COMMIT" ] || { echo "error: $src is at $got, want $COMMIT" >&2; exit 1; }
fi

# One file per line: the upstream path, then (tab) our path when it differs.
n=0
while IFS=$'\t' read -r from to; do
    to=${to:-$from}
    mkdir -p "$dest/$(dirname "$to")"
    cp "$src/$from" "$dest/$to"
    n=$((n + 1))
done <<'EOF'
MMC1_A12/linkfile.txt
MMC1_A12/mmc1_a12.nes
apu_reset/4015_cleared.nes
apu_reset/4017_timing.nes
apu_reset/4017_written.nes
apu_reset/irq_flag_cleared.nes
apu_reset/len_ctrs_enabled.nes
apu_reset/readme.txt
apu_reset/source/readme.txt
apu_reset/works_immediately.nes
apu_test/apu_test.nes
apu_test/readme.txt
apu_test/rom_singles/1-len_ctr.nes
apu_test/rom_singles/2-len_table.nes
apu_test/rom_singles/3-irq_flag.nes
apu_test/rom_singles/4-jitter.nes
apu_test/rom_singles/5-len_timing.nes
apu_test/rom_singles/6-irq_flag_timing.nes
apu_test/rom_singles/7-dmc_basics.nes
apu_test/rom_singles/8-dmc_rates.nes
apu_test/source/readme.txt
blargg_apu_2005.07.30/01.len_ctr.nes
blargg_apu_2005.07.30/02.len_table.nes
blargg_apu_2005.07.30/03.irq_flag.nes
blargg_apu_2005.07.30/04.clock_jitter.nes
blargg_apu_2005.07.30/05.len_timing_mode0.nes
blargg_apu_2005.07.30/06.len_timing_mode1.nes
blargg_apu_2005.07.30/07.irq_flag_timing.nes
blargg_apu_2005.07.30/08.irq_timing.nes
blargg_apu_2005.07.30/09.reset_timing.nes
blargg_apu_2005.07.30/10.len_halt_timing.nes
blargg_apu_2005.07.30/11.len_reload_timing.nes
blargg_apu_2005.07.30/readme.txt
blargg_apu_2005.07.30/tests.txt
blargg_ppu_tests_2005.09.15b/palette_ram.nes
blargg_ppu_tests_2005.09.15b/power_up_palette.nes
blargg_ppu_tests_2005.09.15b/readme.txt
blargg_ppu_tests_2005.09.15b/sprite_ram.nes
blargg_ppu_tests_2005.09.15b/vbl_clear_time.nes
blargg_ppu_tests_2005.09.15b/vram_access.nes
branch_timing_tests/1.Branch_Basics.nes
branch_timing_tests/2.Backward_Branch.nes
branch_timing_tests/3.Forward_Branch.nes
branch_timing_tests/readme.txt
cpu_dummy_reads/cpu_dummy_reads.nes
cpu_dummy_reads/source/readme.txt
cpu_dummy_writes/cpu_dummy_writes_oam.nes
cpu_dummy_writes/cpu_dummy_writes_ppumem.nes
cpu_dummy_writes/readme.txt
cpu_dummy_writes/source/readme.txt
cpu_interrupts_v2/cpu_interrupts.nes
cpu_interrupts_v2/readme.txt
cpu_interrupts_v2/rom_singles/1-cli_latency.nes
cpu_interrupts_v2/rom_singles/2-nmi_and_brk.nes
cpu_interrupts_v2/rom_singles/3-nmi_and_irq.nes
cpu_interrupts_v2/rom_singles/4-irq_and_dma.nes
cpu_interrupts_v2/rom_singles/5-branch_delays_irq.nes
cpu_interrupts_v2/source/readme.txt
cpu_reset/ram_after_reset.nes
cpu_reset/readme.txt
cpu_reset/registers.nes
cpu_reset/source/readme.txt
cpu_timing_test6/cpu_timing_test.nes
cpu_timing_test6/readme.txt
dmc_dma_during_read4/dma_2007_read.nes
dmc_dma_during_read4/dma_2007_write.nes
dmc_dma_during_read4/dma_4016_read.nes
dmc_dma_during_read4/double_2007_read.nes
dmc_dma_during_read4/read_write_2007.nes
dmc_dma_during_read4/source/readme.txt
full_palette/flowing_palette.nes
full_palette/full_palette.nes
full_palette/full_palette_smooth.nes
instr_misc/instr_misc.nes
instr_misc/readme.txt
instr_misc/rom_singles/01-abs_x_wrap.nes
instr_misc/rom_singles/02-branch_wrap.nes
instr_misc/rom_singles/03-dummy_reads.nes
instr_misc/rom_singles/04-dummy_reads_apu.nes
instr_misc/source/readme.txt
instr_test-v5/all_instrs.nes
instr_test-v5/official_only.nes
instr_test-v5/readme.txt
instr_test-v5/rom_singles/01-basics.nes
instr_test-v5/rom_singles/02-implied.nes
instr_test-v5/rom_singles/03-immediate.nes
instr_test-v5/rom_singles/04-zero_page.nes
instr_test-v5/rom_singles/05-zp_xy.nes
instr_test-v5/rom_singles/06-absolute.nes
instr_test-v5/rom_singles/07-abs_xy.nes
instr_test-v5/rom_singles/08-ind_x.nes
instr_test-v5/rom_singles/09-ind_y.nes
instr_test-v5/rom_singles/10-branches.nes
instr_test-v5/rom_singles/11-stack.nes
instr_test-v5/rom_singles/12-jmp_jsr.nes
instr_test-v5/rom_singles/13-rts.nes
instr_test-v5/rom_singles/14-rti.nes
instr_test-v5/rom_singles/15-brk.nes
instr_test-v5/rom_singles/16-special.nes
instr_test-v5/source/readme.txt
instr_timing/instr_timing.nes
instr_timing/readme.txt
instr_timing/rom_singles/1-instr_timing.nes
instr_timing/rom_singles/2-branch_timing.nes
instr_timing/source/readme.txt
mmc3_test_2/readme.txt
mmc3_test_2/rom_singles/1-clocking.nes
mmc3_test_2/rom_singles/2-details.nes
mmc3_test_2/rom_singles/3-A12_clocking.nes
mmc3_test_2/rom_singles/4-scanline_timing.nes
mmc3_test_2/rom_singles/5-MMC3.nes
mmc3_test_2/rom_singles/6-MMC3_alt.nes
mmc3_test_2/source/readme.txt
other/nestest.log	nestest/nestest.log
other/nestest.nes	nestest/nestest.nes
oam_read/oam_read.nes
oam_read/readme.txt
oam_read/source/readme.txt
oam_stress/oam_stress.nes
oam_stress/readme.txt
oam_stress/source/readme.txt
pal_apu_tests/01.len_ctr.nes
pal_apu_tests/02.len_table.nes
pal_apu_tests/03.irq_flag.nes
pal_apu_tests/04.clock_jitter.nes
pal_apu_tests/05.len_timing_mode0.nes
pal_apu_tests/06.len_timing_mode1.nes
pal_apu_tests/07.irq_flag_timing.nes
pal_apu_tests/08.irq_timing.nes
pal_apu_tests/10.len_halt_timing.nes
pal_apu_tests/11.len_reload_timing.nes
pal_apu_tests/readme.txt
ppu_open_bus/ppu_open_bus.nes
ppu_open_bus/readme.txt
ppu_open_bus/source/readme.txt
ppu_read_buffer/readme.txt
ppu_read_buffer/source/readme.txt
ppu_read_buffer/test_ppu_read_buffer.nes
ppu_vbl_nmi/ppu_vbl_nmi.nes
ppu_vbl_nmi/readme.txt
ppu_vbl_nmi/rom_singles/01-vbl_basics.nes
ppu_vbl_nmi/rom_singles/02-vbl_set_time.nes
ppu_vbl_nmi/rom_singles/03-vbl_clear_time.nes
ppu_vbl_nmi/rom_singles/04-nmi_control.nes
ppu_vbl_nmi/rom_singles/05-nmi_timing.nes
ppu_vbl_nmi/rom_singles/06-suppression.nes
ppu_vbl_nmi/rom_singles/07-nmi_on_timing.nes
ppu_vbl_nmi/rom_singles/08-nmi_off_timing.nes
ppu_vbl_nmi/rom_singles/09-even_odd_frames.nes
ppu_vbl_nmi/rom_singles/10-even_odd_timing.nes
ppu_vbl_nmi/source/readme.txt
read_joy3/count_errors.nes
read_joy3/count_errors_fast.nes
read_joy3/source/readme.txt
read_joy3/test_buttons.nes
read_joy3/thorough_test.nes
scrolltest/scroll.nes
sprdma_and_dmc_dma/sprdma_and_dmc_dma.nes
sprdma_and_dmc_dma/sprdma_and_dmc_dma_512.nes
sprite_hit_tests_2005.10.05/01.basics.nes
sprite_hit_tests_2005.10.05/02.alignment.nes
sprite_hit_tests_2005.10.05/03.corners.nes
sprite_hit_tests_2005.10.05/04.flip.nes
sprite_hit_tests_2005.10.05/05.left_clip.nes
sprite_hit_tests_2005.10.05/06.right_edge.nes
sprite_hit_tests_2005.10.05/07.screen_bottom.nes
sprite_hit_tests_2005.10.05/08.double_height.nes
sprite_hit_tests_2005.10.05/09.timing_basics.nes
sprite_hit_tests_2005.10.05/10.timing_order.nes
sprite_hit_tests_2005.10.05/11.edge_timing.nes
sprite_hit_tests_2005.10.05/readme.txt
sprite_overflow_tests/1.Basics.nes
sprite_overflow_tests/2.Details.nes
sprite_overflow_tests/3.Timing.nes
sprite_overflow_tests/4.Obscure.nes
sprite_overflow_tests/5.Emulator.nes
sprite_overflow_tests/readme.txt
EOF
echo "$n test files in $dest"
