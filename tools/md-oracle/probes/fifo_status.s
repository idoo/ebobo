| frames: 12
| The status port's FIFO bits (bit 9 empty, bit 8 full) right after 1, 2, 3, 4 and 5
| back-to-back video RAM word writes at the start of the picture. 320-pixel mode, display on.
| Result words: [0] before any write, [1..5] status AND 0x0300 after that many writes. The FIFO
| drains for about four lines between the rounds.

    .include "probe.inc"
    .globl  main

main:
    vdp_reg 1,0x44                  | display on
    lea     (VDP_DATA).l,a0
    lea     (VDP_CTRL).l,a2
    jsr     wait_vblank_end
    move.l  #0x40000000,(a2)        | video RAM write at 0x0000
    moveq   #0,d3

    move.w  (a2),d0
    bsr.s   rec

    move.w  d3,(a0)
    move.w  (a2),d0
    bsr.s   rec

    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  (a2),d0
    bsr.s   rec

    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  (a2),d0
    bsr.s   rec

    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  (a2),d0
    bsr.s   rec

    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  d3,(a0)
    move.w  (a2),d0
    bsr.s   rec
    rts

| Records the FIFO bits of d0, then waits about four lines.
rec:
    andi.w  #0x0300,d0
    jsr     res_w
    move.w  #200,d1
1:  dbf     d1,1b
    rts
