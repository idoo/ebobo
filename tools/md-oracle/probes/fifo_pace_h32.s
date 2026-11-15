| frames: 30
| FIFO pace in the 256-pixel mode: see fifo_pace.inc for the cases and the result words.

    .include "probe.inc"
    .globl  main

main:
    vdp_reg 12,0x00                 | 256 pixels wide
    .include "fifo_pace.inc"
