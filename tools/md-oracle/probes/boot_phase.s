| frames: 8
| standalone
| Where in the frame the machine powers on. Its own start-up: nothing runs before the first samples.
|   word 0      HV counter read by the very first instruction
|   word 1      status word read by the second
|   word 2, 3   HV and status right after the display is turned on (mode 5, 256 wide, no interrupts)
|   word 4, 5   passes of the poll loop until the vertical-blank flag is first seen set (long)
|   word 6      HV at that edge
|   word 7, 8   passes until it is then seen clear: the length of the blanking
|   word 9      HV at that edge
|   word 10, 11 passes until it is seen set again: the length of the picture
|   word 12     HV at that edge
| A pass is addq.l / move.w (a4),d1 / btst / branch taken = 36 CPU cycles = 252 master clocks.

    .include "probe.inc"

    .globl  _start

    .section .vectors,"ax"
    .long   0x00FFFE00              | initial supervisor stack
    .long   _start
    .rept   62
    .long   trap
    .endr

    .section .header,"ax"
hdr:
    .ascii  "SEGA MEGA DRIVE "
    .ascii  "(C)EBOBO 2026   "
    .ascii  "EBOBO BOOT PHASE PROBE"
    .org    hdr+0x50, 0x20
    .ascii  "EBOBO BOOT PHASE PROBE"
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

_start:
    move.w  (VDP_HV).l,d6           | word 0
    move.w  (VDP_STATUS).l,d7       | word 1
    lea     (VDP_CTRL).l,a4
    lea     (VDP_HV).l,a5
    lea     (RESULT_WORDS).l,a6
    move.w  d6,(a6)+
    move.w  d7,(a6)+
    move.b  (IO_VERSION).l,d0
    andi.b  #0x0F,d0
    beq.s   1f
    move.l  #0x53454741,(TMSS).l    | "SEGA"
1:  move.w  #0x8004,(a4)            | register 0: no H interrupt, HV counter runs
    move.w  #0x8144,(a4)            | register 1: mode 5, display on, no V interrupt
    move.w  (a5),(a6)+              | word 2
    move.w  (a4),(a6)+              | word 3

    moveq   #0,d0
2:  addq.l  #1,d0
    move.w  (a4),d1
    btst    #ST_VBLANK,d1
    beq.s   2b
    move.w  (a5),d2
    move.l  d0,(a6)+                | words 4, 5
    move.w  d2,(a6)+                | word 6

    moveq   #0,d0
3:  addq.l  #1,d0
    move.w  (a4),d1
    btst    #ST_VBLANK,d1
    bne.s   3b
    move.w  (a5),d2
    move.l  d0,(a6)+                | words 7, 8
    move.w  d2,(a6)+                | word 9

    moveq   #0,d0
4:  addq.l  #1,d0
    move.w  (a4),d1
    btst    #ST_VBLANK,d1
    beq.s   4b
    move.w  (a5),d2
    move.l  d0,(a6)+                | words 10, 11
    move.w  d2,(a6)+                | word 12

    move.w  #13,(RESULT_COUNT).l
    move.w  #DONE_MARK,(RESULT_DONE).l
5:  bra.s   5b
