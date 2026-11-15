| frames: 8
| standalone
| Is a horizontal-interrupt request latched right after power-on, before register 10 is written?
| Its own start-up: only the TMSS unlock runs before stage 1.
|   word 0      status word read first
|   stage 1     display on, H interrupt on, unmask for 64 NOPs straight away:
|   word 1, 2   H interrupts taken; V counter byte the first one read
|   stage 2     H interrupt off in the VDP and CPU masked through the first vertical blanking and up to
|               line 16, then on and unmasked for two NOPs (register 10 still at its power-on value):
|   word 3, 4   H interrupts taken (a request left from the lines before); V byte
|   stage 3     register 10 = 0xFF written in the next blanking, then as stage 2:
|   word 5, 6   H interrupts taken; V byte

    .include "probe.inc"

    .globl  _start

    .section .vectors,"ax"
    .long   0x00FFFE00              | initial supervisor stack
    .long   _start
    .rept   26
    .long   trap
    .endr
    .long   hint                    | vector 28: level 4
    .rept   35
    .long   trap
    .endr

    .section .header,"ax"
hdr:
    .ascii  "SEGA MEGA DRIVE "
    .ascii  "(C)EBOBO 2026   "
    .ascii  "EBOBO INT BOOT PROBE"
    .org    hdr+0x50, 0x20
    .ascii  "EBOBO INT BOOT PROBE"
    .org    hdr+0x80, 0x20
    .ascii  "GM 00000000-00"
    .word   0
    .ascii  "J               "
    .long   0, _rom_end-1           | ROM range
    .long   0x00FF0000, 0x00FFFFFF  | RAM range
    .org    hdr+0xF0, 0x20
    .ascii  "JUE             "

    .text
trap:
    move.w  #0x2700,sr
    move.w  #TRAP_MARK,(RESULT_DONE).l
1:  bra.s   1b

hint:
    addq.w  #1,d5
    cmpi.w  #1,d5
    bne.s   1f
    move.b  (VDP_HV).l,d6
1:  rte

_start:
    move.b  (IO_VERSION).l,d0
    andi.b  #0x0F,d0
    beq.s   1f
    move.l  #0x53454741,(TMSS).l    | "SEGA"
1:  lea     (VDP_CTRL).l,a4
    lea     (RESULT_WORDS).l,a6
    moveq   #0,d5
    moveq   #0,d6
    move.w  (a4),d7                 | word 0
    move.w  #0x8144,(a4)            | register 1: mode 5, display on, no V interrupt
    move.w  #0x8014,(a4)            | register 0: H interrupt on
    move.w  #0x2300,sr
    .rept   64
    nop
    .endr
    move.w  #0x2700,sr
    move.w  #0x8004,(a4)
    move.w  d7,(a6)+
    move.w  d5,(a6)+                | word 1
    move.w  d6,(a6)+                | word 2

    moveq   #1,d4                   | stage 2, then stage 3
stage:
2:  move.w  (a4),d1
    btst    #ST_VBLANK,d1
    beq.s   2b
    tst.w   d4
    bne.s   3f
    move.w  #0x8AFF,(a4)            | stage 3: register 10 = 0xFF, written in the blanking
3:  move.w  (a4),d1
    btst    #ST_VBLANK,d1
    bne.s   3b
4:  cmpi.b  #16,(VDP_HV).l
    bne.s   4b
    moveq   #0,d5
    moveq   #0,d6
    move.w  #0x8014,(a4)
    move.w  #0x2300,sr
    nop
    nop
    move.w  #0x2700,sr
    move.w  #0x8004,(a4)
    move.w  d5,(a6)+
    move.w  d6,(a6)+
    dbf     d4,stage

    move.w  #7,(RESULT_COUNT).l
    move.w  #DONE_MARK,(RESULT_DONE).l
5:  bra.s   5b
