| frames: 40
| DMA lengths in the 320-pixel mode: see dma_rate.inc for the cases and the result words.

    .include "probe.inc"
    .globl  main

main:
    .include "dma_rate.inc"
