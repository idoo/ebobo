| frames: 12
| Where the V and H counters stand when the status flags change.
| Result words: [0] V counter when the vertical-blank flag first reads set, [1] V counter when it
| first reads clear again, [2] V counter when the horizontal-blank flag first reads set after
| that, [3..18] the first 16 distinct values of the H counter byte read in a tight loop that
| starts when the horizontal-blank flag falls (the start of a line).

    .include "probe.inc"
    .globl  main

main:
    vdp_reg 1,0x64                  | display on (V interrupt enabled in the VDP, masked in the CPU)
    lea     (VDP_STATUS_LO).l,a0
    lea     (VDP_HV).l,a1
    jsr     wait_vblank_end         | now in the picture
    moveq   #0,d0

1:  btst    #ST_VBLANK,(a0)
    beq.s   1b
    move.b  (a1),d0
    jsr     res_w

2:  btst    #ST_VBLANK,(a0)
    bne.s   2b
    move.b  (a1),d0
    jsr     res_w

3:  btst    #ST_HBLANK,(a0)
    beq.s   3b
    move.b  (a1),d0
    jsr     res_w

4:  btst    #ST_HBLANK,(a0)         | to the end of this horizontal blank
    bne.s   4b
    lea     (SCRATCH).l,a2
    moveq   #15,d2
    move.b  1(a1),d1
5:  move.b  d1,(a2)+
6:  move.b  1(a1),d3
    cmp.b   d3,d1
    beq.s   6b
    move.b  d3,d1
    dbf     d2,5b

    lea     (SCRATCH).l,a2
    moveq   #15,d2
7:  move.b  (a2)+,d0
    jsr     res_w
    dbf     d2,7b
    rts
