| frames: 12
| Where the H and V counters stand at the edges of the blanking flags.
| Result words: [0] HV counter when the vertical-blank flag first reads set, [1] HV counter when
| it first reads clear again, then 12 pairs on the following lines: HV counter when the
| horizontal-blank flag first reads set, HV counter when it first reads clear again (320-pixel
| mode); then the same 12 pairs in the 256-pixel mode, taken after the next vertical blank.

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

    bsr.s   pairs                   | 320-pixel mode (the runtime's register 12)

    vdp_reg 12,0x00                 | 256-pixel mode
    jsr     wait_vblank_start
    jsr     wait_vblank_end
    bsr.s   pairs

    lea     (SCRATCH).l,a2
    moveq   #49,d2
6:  move.w  (a2)+,d0
    jsr     res_w
    dbf     d2,6b
    rts

| 12 pairs of HV counter words to (a2)+: at the flag's rising edge, at its falling edge.
pairs:
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
    rts
