| The runtime every probe links with: vectors, cartridge header, start-up, result block.
| The probe defines `main` (and, if it wants them, `hint` and `vint`).

    .include "probe.inc"

    .globl  _start, res_w, res_l, wait_vblank_start, wait_vblank_end
    .weak   hint, vint

| ---- vectors: 64 longs ----
    .section .vectors,"ax"
    .long   0x00FFFE00              | initial supervisor stack
    .long   _start
    .set    vn, 2
    .rept   62
    .if     vn == 28
    .long   hint                    | level 4 autovector: horizontal interrupt
    .elseif vn == 30
    .long   vint                    | level 6 autovector: vertical interrupt
    .else
    .long   stubs+(vn-2)*6
    .endif
    .set    vn, vn+1
    .endr

| ---- header at 0x100 ----
    .section .header,"ax"
hdr:
    .ascii  "SEGA MEGA DRIVE "
    .ascii  "(C)EBOBO 2026   "
    .ascii  "EBOBO TIMING PROBE"
    .org    hdr+0x50, 0x20
    .ascii  "EBOBO TIMING PROBE"
    .org    hdr+0x80, 0x20
    .ascii  "GM 00000000-00"
    .word   0                       | checksum (not checked by anything we run)
    .ascii  "J               "
    .long   0, _rom_end-1           | ROM range
    .long   0x00FF0000, 0x00FFFFFF  | RAM range
    .org    hdr+0xF0, 0x20
    .ascii  "JUE             "

    .text
| One stub per vector 2..63, 6 bytes each: the vector's number, then the common handler.
stubs:
    .set    vn, 2
    .rept   62
    moveq   #vn,d0
    bra.w   trap_common
    .set    vn, vn+1
    .endr

| An exception nobody asked for: append 0xEE00 + vector number, mark the block, stop.
trap_common:
    move.w  #0x2700,sr
    addi.w  #0xEE00,d0
    bsr.w   res_w
    move.w  #TRAP_MARK,(RESULT_DONE).l
1:  stop    #0x2700
    bra.s   1b

default_int:
hint:
vint:
    rte

_start:
    move.w  #0x2700,sr
    move.b  (IO_VERSION).l,d0
    andi.b  #0x0F,d0
    beq.s   1f
    move.l  #0x53454741,(TMSS).l    | "SEGA"
1:  lea     (VDP_CTRL).l,a0
    move.w  (a0),d0                 | reading the status ends a half-written command
    lea     vdp_defaults(pc),a1
    move.w  #0x8000,d0
    moveq   #23,d1
2:  move.b  (a1)+,d0
    move.w  d0,(a0)
    addi.w  #0x0100,d0
    dbf     d1,2b

    lea     (RAM).l,a1              | clear the 64 KB (nothing is on the stack yet)
    move.w  #16383,d1
3:  clr.l   (a1)+
    dbf     d1,3b

    move.w  #0x0100,(Z80_BUSREQ).l  | Z80 off the bus and held in reset
    move.w  #0x0000,(Z80_RESET).l
    lea     (PSG).l,a1
    move.b  #0x9F,(a1)
    move.b  #0xBF,(a1)
    move.b  #0xDF,(a1)
    move.b  #0xFF,(a1)

    moveq   #0,d0
    moveq   #0,d1
    jsr     main
    move.w  #0x2700,sr
    move.w  #DONE_MARK,(RESULT_DONE).l
4:  bra.s   4b

| Mode 5, display off, 320 wide, no interrupts, auto-increment 2, planes 64x32.
| NOTE: with the display off the status port's vertical-blank flag reads 1 all the time; a probe
| that polls it turns the display on first (register 1 = 0x44, or 0x64 with the V interrupt).
vdp_defaults:
    .byte   0x04, 0x04, 0x30, 0x3C, 0x07, 0x6C, 0x00, 0x00
    .byte   0x00, 0x00, 0xFF, 0x00, 0x81, 0x3F, 0x00, 0x02
    .byte   0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
    .even

| Appends d0.w to the result block. Keeps every register.
res_w:
    movem.l d1/a0,-(sp)
    lea     (RESULT_COUNT).l,a0
    move.w  (a0),d1
    addq.w  #1,(a0)
    add.w   d1,d1
    move.w  d0,2(a0,d1.w)
    movem.l (sp)+,d1/a0
    rts

| Appends d0.l, high word first. Keeps every register.
res_l:
    swap    d0
    bsr.w   res_w
    swap    d0
    bra.w   res_w

| Returns right after the vertical-blank flag goes from 0 to 1. Keeps every register.
wait_vblank_start:
1:  btst    #ST_VBLANK,(VDP_STATUS_LO).l
    bne.s   1b
2:  btst    #ST_VBLANK,(VDP_STATUS_LO).l
    beq.s   2b
    rts

| Returns right after the vertical-blank flag goes from 1 to 0. Keeps every register.
wait_vblank_end:
1:  btst    #ST_VBLANK,(VDP_STATUS_LO).l
    beq.s   1b
2:  btst    #ST_VBLANK,(VDP_STATUS_LO).l
    bne.s   2b
    rts
