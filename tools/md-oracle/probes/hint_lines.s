| frames: 20
| On which lines the horizontal interrupt arrives. For register 10 = 0, 1 and 7: the interrupt is
| enabled in the VDP while the CPU masks it, the CPU unmasks it right after a vertical blank ends,
| and the handler stores the V counter byte of each interrupt until the next vertical blank.
| Result: per setting 17 words: how many interrupts came (the table holds the first 16 at most),
| then the 16 table entries (0 where none came). An interrupt left pending from before the
| unmasking is taken first, so entry 0 can be the line of the unmasking itself.

    .include "probe.inc"
    .globl  main, hint

ARMED = SCRATCH+0x100

main:
    vdp_reg 1,0x44                  | display on, V interrupt off
    lea     settings(pc),a3
    moveq   #2,d4
next:
    moveq   #0,d0
    move.b  (a3)+,d0
    addi.w  #0x8A00,d0
    move.w  d0,(VDP_CTRL).l         | register 10: lines between interrupts, minus 1
    vdp_reg 0,0x14                  | horizontal interrupt on
    lea     (SCRATCH).l,a2
    clr.l   (a2)
    clr.l   4(a2)
    clr.l   8(a2)
    clr.l   12(a2)
    moveq   #0,d5                   | interrupts seen
    jsr     wait_vblank_start
    jsr     wait_vblank_end
    st      (ARMED).l
    move.w  #0x2300,sr
    jsr     wait_vblank_start
    move.w  #0x2700,sr
    clr.b   (ARMED).l
    vdp_reg 0,0x04

    move.w  d5,d0
    jsr     res_w
    lea     (SCRATCH).l,a2
    moveq   #15,d2
    moveq   #0,d0
1:  move.b  (a2)+,d0
    jsr     res_w
    dbf     d2,1b
    dbf     d4,next
    rts

hint:
    tst.b   (ARMED).l
    beq.s   9f
    cmpi.w  #16,d5
    bcc.s   8f
    move.b  (VDP_HV).l,(a2)+
8:  addq.w  #1,d5
9:  rte

settings:
    .byte   0, 1, 7
    .even
