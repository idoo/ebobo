| frames: 20
| How fast the VDP takes data-port writes: 512 words poured in a tight loop, V counter before
| and after. 320-pixel mode, display on.
| Result words: [0],[1] video RAM words from the start of the picture; [2],[3] colour RAM words
| from the start of the picture; [4],[5] video RAM words from the start of vertical blanking;
| [6],[7] colour RAM words from the start of vertical blanking.

    .include "probe.inc"
    .globl  main

main:
    vdp_reg 1,0x44                  | display on
    lea     (VDP_DATA).l,a0
    lea     (VDP_HV).l,a1
    lea     (VDP_CTRL).l,a2

    move.l  #0x40000000,d2          | video RAM write at 0x0000
    jsr     wait_vblank_end
    bsr.s   pour
    move.l  #0xC0000000,d2          | colour RAM write at 0
    jsr     wait_vblank_end
    bsr.s   pour
    move.l  #0x40000000,d2
    jsr     wait_vblank_start
    bsr.s   pour
    move.l  #0xC0000000,d2
    jsr     wait_vblank_start
    bsr.s   pour
    rts

| Sets the command in d2, then writes 512 zero words; the V counter before and after is recorded.
pour:
    move.l  d2,(a2)
    moveq   #0,d0
    move.b  (a1),d0
    jsr     res_w
    move.w  #511,d1
    moveq   #0,d3
1:  move.w  d3,(a0)
    dbf     d1,1b
    moveq   #0,d0
    move.b  (a1),d0
    jsr     res_w
    rts
