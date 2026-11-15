| frames: 12
| Where the H and V counters stand at the edges of the blanking flags.
| Result words: [0] HV counter when the vertical-blank flag first reads set, [1] HV counter when
| it first reads clear again, then 12 pairs on the following lines: HV counter when the
| horizontal-blank flag first reads set, HV counter when it first reads clear again.

    .include "probe.inc"
    .globl  main

main:
    vdp_reg 1,0x64                  | display on (V interrupt enabled in the VDP, masked in the CPU)
    lea     (VDP_STATUS_LO).l,a0
    lea     (VDP_HV).l,a1
    lea     (SCRATCH).l,a2
    jsr     wait_vblank_end         | now in the picture

1:  btst    #ST_VBLANK,(a0)
    beq.s   1b
    move.w  (a1),(a2)+

2:  btst    #ST_VBLANK,(a0)
    bne.s   2b
    move.w  (a1),(a2)+

    moveq   #11,d2
3:  btst    #ST_HBLANK,(a0)         | out of any horizontal blank first
    bne.s   3b
4:  btst    #ST_HBLANK,(a0)
    beq.s   4b
    move.w  (a1),(a2)+
5:  btst    #ST_HBLANK,(a0)
    bne.s   5b
    move.w  (a1),(a2)+
    dbf     d2,3b

    lea     (SCRATCH).l,a2
    moveq   #25,d2
6:  move.w  (a2)+,d0
    jsr     res_w
    dbf     d2,6b
    rts
