| frames: 270
| How many T-states a frame the Z80 gets. The 68000 loads a counting loop into Z80 RAM, lets the
| Z80 run for exactly 60 frames (vertical-blank edges polled, interrupts of the 68000 masked),
| requests the bus and reads the 24-bit pass counter at Z80 0x0040.
| Cases (two result words each: counter low 16 bits, then its high byte):
|   a  the loop alone:                                   56 T-states a pass
|   b  the loop also reads 0x8000 (bank 0, the ROM):     69 T-states a pass
|   c  loop a; the 68000 requests the bus, waits for the grant and releases it once per line
|   d  loop a with the interrupt enabled (mode 0 after reset; the bus reads 0xFF: rst 0x38);
|      the handler at 0x38 is `ei` + `reti`
|      (13 acknowledge + 4 + 14 = 31 T-states per interrupt taken)
| Nominal: 896040 / 15 = 59736 T-states per NTSC frame, 313 x 3420 / 15 = 71364 per PAL frame.
| `tools/md-oracle/sound_clock.py` has the same T-state table.

    .include "probe.inc"
    .globl  main

ZRAM = 0xA00000

main:
    vdp_reg 1,0x44                  | display on: the blanking flag toggles
    lea     (VDP_STATUS_LO).l,a1
    lea     (Z80_BUSREQ).l,a3

    lea     prog_a(pc),a2           | a
    moveq   #0x00,d3
    moveq   #0,d2
    bsr     case
    lea     prog_b(pc),a2           | b
    moveq   #0x00,d3
    moveq   #0,d2
    bsr     case
    lea     prog_a(pc),a2           | c
    moveq   #0x00,d3
    moveq   #1,d2
    bsr     case
    lea     prog_a(pc),a2           | d
    move.w  #0xFB,d3                | ei
    moveq   #0,d2
    bsr     case
    rts

| Runs the 24-byte program at a2 with byte 3 replaced by d3 for 60 frames; d2 != 0: bus request
| once per line. Stores the counter.
case:
    move.w  #0x0100,(a3)
    move.w  #0x0100,(Z80_RESET).l
1:  btst    #0,(a3)
    bne.s   1b
    lea     (ZRAM).l,a0
    moveq   #0x7F,d0
2:  clr.b   (a0)+                   | 0x00-0x7F: nop, counter 0
    dbra    d0,2b
    lea     (ZRAM).l,a0
    moveq   #23,d0
3:  move.b  (a2)+,(a0)+
    dbra    d0,3b
    move.b  d3,(ZRAM+3).l
    move.b  #0xFB,(ZRAM+0x38).l     | ei
    move.b  #0xED,(ZRAM+0x39).l     | reti
    move.b  #0x4D,(ZRAM+0x3A).l
    move.w  #0x0000,(Z80_RESET).l   | reset the Z80
    moveq   #31,d0
4:  dbra    d0,4b
    move.w  #0x0000,(a3)
5:  btst    #ST_VBLANK,(a1)
    bne.s   5b
6:  btst    #ST_VBLANK,(a1)
    beq.s   6b
    move.w  #0x0100,(Z80_RESET).l   | go
    st      d5
    moveq   #60,d6
    move.b  (VDP_HV).l,d4
7:  tst.b   d2
    beq.s   9f
    move.b  (VDP_HV).l,d0
    cmp.b   d0,d4
    beq.s   9f
    move.b  d0,d4
    move.w  #0x0100,(a3)            | a new line: take the bus
8:  btst    #0,(a3)
    bne.s   8b
    move.w  #0x0000,(a3)            | and give it back
9:  btst    #ST_VBLANK,(a1)
    bne.s   10f
    sf      d5
    bra.s   7b
10: tst.b   d5
    bne.s   7b
    st      d5
    subq.w  #1,d6
    bne.s   7b
    move.w  #0x0100,(a3)            | stop
11: btst    #0,(a3)
    bne.s   11b
    move.b  (ZRAM+0x41).l,d0
    lsl.w   #8,d0
    move.b  (ZRAM+0x40).l,d0
    jsr     res_w
    moveq   #0,d0
    move.b  (ZRAM+0x42).l,d0
    jsr     res_w
    rts

| Z80 code, T-states on the right. Byte 3 is `nop` or `ei`.
prog_a:
    .byte   0x31, 0xF0, 0x1F        | 0000 ld sp,0x1FF0 (the stack in Z80 RAM)
    .byte   0x00                    | 0003 nop / ei
    .byte   0x2A, 0x40, 0x00        | 0004 ld hl,(0x0040)      16
    .byte   0x23                    | 0007 inc hl               6
    .byte   0x22, 0x40, 0x00        | 0008 ld (0x0040),hl      16
    .byte   0x7C                    | 000B ld a,h               4
    .byte   0xB5                    | 000C or l                 4
    .byte   0xC2, 0x04, 0x00        | 000D jp nz,0x0004        10   = 56
    .byte   0x21, 0x42, 0x00        | 0010 ld hl,0x0042             (once in 65536 passes)
    .byte   0x34                    | 0013 inc (hl)
    .byte   0xC3, 0x04, 0x00        | 0014 jp 0x0004
    .byte   0x00
prog_b:
    .byte   0x31, 0xF0, 0x1F        | 0000 ld sp,0x1FF0
    .byte   0x00                    | 0003 nop
    .byte   0x3A, 0x00, 0x80        | 0004 ld a,(0x8000)       13
    .byte   0x2A, 0x40, 0x00        | 0007 ld hl,(0x0040)      16
    .byte   0x23                    | 000A inc hl               6
    .byte   0x22, 0x40, 0x00        | 000B ld (0x0040),hl      16
    .byte   0x7C                    | 000E ld a,h               4
    .byte   0xB5                    | 000F or l                 4
    .byte   0xC2, 0x04, 0x00        | 0010 jp nz,0x0004        10   = 69
    .byte   0x34                    | 0013 (hl is 0: never reached within 60 frames)
    .byte   0xC3, 0x04, 0x00        | 0014 jp 0x0004
    .byte   0x00
    .align  2
