| frames: 40
| DMA lengths in the 256-pixel mode: see dma_rate.inc for the cases and the result words.

    .include "probe.inc"
    .globl  main

main:
    vdp_reg 12,0x00                 | 256 pixels wide
    .include "dma_rate.inc"
