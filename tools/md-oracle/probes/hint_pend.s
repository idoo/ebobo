| frames: 44
| Where a horizontal-interrupt request latched while the CPU masks it goes, register 10 = 0 (then 1).
| The CPU stays masked; for each target it waits for the V counter to read the target V, unmasks for
| two NOPs and masks again. Target 0x01 instead unmasks right where the vertical-blank flag ends (as
| hint_lines does); after the first 0x01 target register 10 becomes 1. Target 0x02 turns the H
| interrupt off in the VDP for a whole frame and on again where the vertical-blank flag ends, then
| unmasks: is a request raised while the VDP disables it kept? Result per target: interrupts taken
| in the window, HV word the handler read last, HV word just before unmasking.

    .include "probe.inc"
    .globl  main, hint

main:
    vdp_reg 1,0x44                  | display on, V interrupt off
    vdp_reg 10,0
    vdp_reg 0,0x14                  | horizontal interrupt on
    lea     targets(pc),a3
    moveq   #9,d4
next:
    move.b  (a3)+,d3
    cmpi.b  #0x02,d3
    bne.s   4f
    vdp_reg 0,0x04
4:  jsr     wait_vblank_start
    jsr     wait_vblank_end
    cmpi.b  #0x02,d3
    bne.s   5f
    vdp_reg 0,0x14
    bra.s   2f
5:  cmpi.b  #0x01,d3
    beq.s   2f
1:  cmp.b   (VDP_HV).l,d3
    bne.s   1b
2:  move.w  (VDP_HV).l,d7
    moveq   #0,d5
    moveq   #0,d6
    move.w  #0x2300,sr
    nop
    nop
    move.w  #0x2700,sr
    move.w  d5,d0
    jsr     res_w
    move.w  d6,d0
    jsr     res_w
    move.w  d7,d0
    jsr     res_w
    cmpi.b  #0x01,d3
    bne.s   3f
    vdp_reg 10,1
3:  dbf     d4,next
    vdp_reg 0,0x04
    rts

hint:
    addq.w  #1,d5
    move.w  (VDP_HV).l,d6
    rte

targets:
    .byte   0xDF, 0xE0, 0xE1, 0xE4, 0xF0, 0xFF, 0x00, 0x01, 0x01, 0x02
    .even
