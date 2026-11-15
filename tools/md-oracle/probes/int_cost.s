| frames: 24
| What one horizontal interrupt costs the 68000. The main loop counts passes in d7 (18 cycles a
| pass, 42 in the last case); the horizontal interrupt's entry `hint` counts itself in d5 and
| jumps to the case's handler through a4 (addq.w + jmp (a4) = 16 cycles, the same in every case).
| The vertical interrupt ends each measurement: its handler stores d7 and d5 as two result words,
| clears both, and after every second interrupt switches to the next case (loop, handler,
| register 10, register 0). Display on, 320-pixel mode.
| Result: 36 words = (warm-up, cases 0-7) x 2 intervals x (passes, horizontal interrupts taken);
| the second interval of each case is the clean one (a whole frame less the vertical handler).
| Cost of one interrupt = (passes of case 0 - passes) x cycles per pass / interrupts taken;
| minus the handler's cycles below = what accepting the interrupt costs.
| `tools/md-oracle/int_cost.py` has the same table and prints both emulators side by side.
|
|   case  line interval  handler cycles (with the 16 of `hint`)
|   0     off            -
|   1     1              36    rte
|   2     1              120   movem push 40, movem pop 44, rte
|   3     1              80    move.l #,(ctrl) 28, move.w d0,(data) 16, rte
|   4     1              52    move.w (status),d0 16, rte
|   5     1              256   SGDK's path: movem 40, movea.l (ptr).l 20, jsr (a0) 16, the two
|                              writes 44, addq.w #1,(xxx).l 20, add.w d0,(xxx).l 20, rts 16,
|                              movem 44, rte
|   6     8              36    rte
|   7     1              36    rte; the loop is 6 nops longer (42 cycles a pass)

    .include "probe.inc"
    .globl  main, hint, vint

PTR     = SCRATCH
COUNTER = SCRATCH+4
OTHER   = SCRATCH+6

main:
    movea.l sp,a5                   | finish returns from here whatever the loops left on the stack
    lea     cases(pc),a6
    move.l  #callback,(PTR).l
    moveq   #0,d5                   | horizontal interrupts taken
    moveq   #0,d6                   | flips on every vertical interrupt
    moveq   #0,d7                   | passes
    vdp_reg 12,0x81                 | 320 pixels
    movea.l (a6)+,a3
    movea.l (a6)+,a4
    move.w  (a6)+,(VDP_CTRL).l
    move.w  (a6)+,(VDP_CTRL).l
    vdp_reg 1,0x64                  | display on, vertical interrupt on
    move.w  #0x2300,sr
    jmp     (a3)

hint:
    addq.w  #1,d5                   | 8
    jmp     (a4)                    | 8

vint:
    move.w  d7,d0
    jsr     res_w
    move.w  d5,d0
    jsr     res_w
    not.w   d6
    bne.s   1f
    move.l  (a6)+,2(sp)             | second interrupt of this case: return into the next one,
    move.w  #0x2300,(sp)            | unmasked (the vertical interrupt may have cut into `hint`)
    movea.l (a6)+,a4
    move.w  (a6)+,(VDP_CTRL).l
    move.w  (a6)+,(VDP_CTRL).l
1:  moveq   #0,d7
    moveq   #0,d5
    rte

finish:
    move.w  #0x2700,sr
    movea.l a5,sp
    rts

| loop, handler, register 10, register 0 (0x14 = horizontal interrupt on); the first entry is a
| warm-up (its intervals start at start-up), then cases 0-7
cases:
    .long   loop18, h_rte
    .word   0x8AFF, 0x8004
    .long   loop18, h_rte
    .word   0x8AFF, 0x8004
    .long   loop18, h_rte
    .word   0x8A00, 0x8014
    .long   loop18, h_movem
    .word   0x8A00, 0x8014
    .long   loop18, h_scroll
    .word   0x8A00, 0x8014
    .long   loop18, h_status
    .word   0x8A00, 0x8014
    .long   loop18, h_sgdk
    .word   0x8A00, 0x8014
    .long   loop18, h_rte
    .word   0x8A07, 0x8014
    .long   loop42, h_rte
    .word   0x8A00, 0x8014
    .long   finish, h_rte
    .word   0x8AFF, 0x8004

| 18 cycles
loop18:
    addq.l  #1,d7                   | 8
    bra.s   loop18                  | 10

| 42 cycles
loop42:
    nop                             | 4
    nop
    nop
    nop
    nop
    nop
    addq.l  #1,d7
    bra.s   loop42

| 20 cycles
h_rte:
    rte                             | 20

| 104 cycles
h_movem:
    movem.l d0-d1/a0-a1,-(sp)       | 40
    movem.l (sp)+,d0-d1/a0-a1       | 44
    rte

| 64 cycles
h_scroll:
    move.l  #0x40000010,(VDP_CTRL).l    | 28
    move.w  d0,(VDP_DATA).l             | 16
    rte

| 36 cycles
h_status:
    move.w  (VDP_STATUS).l,d0       | 16
    rte

| 240 cycles
h_sgdk:
    movem.l d0-d1/a0-a1,-(sp)       | 40
    movea.l (PTR).l,a0              | 20
    jsr     (a0)                    | 16
    movem.l (sp)+,d0-d1/a0-a1       | 44
    rte

callback:
    move.l  #0x40000010,(VDP_CTRL).l    | 28
    move.w  d0,(VDP_DATA).l             | 16
    addq.w  #1,(COUNTER).l              | 20
    add.w   d0,(OTHER).l                | 20
    rts                                 | 16
