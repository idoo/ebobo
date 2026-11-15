| frames: 30
| FIFO pace in the 320-pixel mode: see fifo_pace.inc for the cases and the result words.

    .include "probe.inc"
    .globl  main

main:
    .include "fifo_pace.inc"
