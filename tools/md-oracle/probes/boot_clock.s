| frames: 8
| standalone
| How much 68000 time has passed when a runner returns frame N. Its own start-up: the result block
| is marked done at once, then a loop adds 1 to the long at words 0, 1 for ever, interrupts masked.
| A pass is addq.l #1,(a0) / bra.s = 30 CPU cycles = 210 master clocks.
| `tools/md-oracle/boot_clock.py` prints the counter after frames 1, 2, 3, 4, 8 for both emulators.

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
    .ascii  "EBOBO BOOT CLOCK PROBE"
    .org    hdr+0x50, 0x20
    .ascii  "EBOBO BOOT CLOCK PROBE"
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
    lea     (RESULT_DONE).l,a0
    move.w  #DONE_MARK,(a0)+
    move.w  #2,(a0)+
    clr.l   (a0)
2:  addq.l  #1,(a0)
    bra.s   2b
