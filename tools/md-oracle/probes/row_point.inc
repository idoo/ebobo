| Body of the row_point probes (row_point.s sets VARIANT = 0, row_point_c.s VARIANT = 1).
| Up to which point of a line does a write still change that line's row of the picture?
| 320-pixel mode, interrupts masked, planes 64x64. Colour i (0..31) is red i & 7, green (i >> 3) * 2.
| Every frame, for k = 0..23: wait until the HV counter word is at least the k-th target, write once,
| read the HV counter. The targets step the H counter across the line; write k has the band of
| lines 16 + 8k .. 23 + 8k to itself. (The V counter moves on at H 0xA5, so a target at or past
| 0xA5 on line L carries V = L + 1.)
| VARIANT 0: plane A's tile row r is solid colour (r % 14) + 1; write k sets the full-plane vertical
|            scroll of plane A to 8 * (k + 1), so the rows below the write show one colour step more.
| VARIANT 1: both planes empty; write k sets the backdrop (register 7) to colour k + 1.
| Result words: [0..23] the HV counter read right after write k, of the last pass run (each pass starts the block over
| once the vertical blank has ended, so a RAM dump taken at the end of a frame holds the words of that frame's writes).
| The same writes repeat for 16 frames, so any frame in between shows them (tools/md-oracle/row_point.py).

    .include "probe.inc"
    .globl  main

N = 24
PASSES = 16
| Line order of the H counter: A5..B6, E4..FF, 00..A4, and the V counter moves on at A5. The wait
| therefore compares V << 8 + ((H - 0xA5) & 0xFF), which grows through the frame's picture.
.macro tgt k, t
    .if \t < 0xA5
    .word   ((16+8*\k)<<8)+((\t-0xA5)&0xFF)
    .else
    .word   ((17+8*\k)<<8)+((\t-0xA5)&0xFF)
    .endif
.endm

main:
    lea     (VDP_CTRL).l,a0
    lea     (VDP_HV).l,a1
    lea     (VDP_DATA).l,a2
    vdp_reg 16,0x11                 | planes 64x64

    move.l  #0xC0000000,(a0)        | CRAM 0..31
    moveq   #0,d1
1:  move.w  d1,d0
    andi.w  #7,d0
    add.w   d0,d0
    move.w  d1,d2
    andi.w  #0x18,d2
    lsl.w   #3,d2
    or.w    d2,d0
    move.w  d0,(a2)
    addq.w  #1,d1
    cmpi.w  #32,d1
    bne.s   1b

    move.l  #0x40000000,(a0)        | tile 0 blank, tiles 1..14 solid colour 1..14
    moveq   #0,d0
    moveq   #14,d1
2:  moveq   #7,d2
3:  move.l  d0,(a2)
    dbf     d2,3b
    addi.l  #0x11111111,d0
    dbf     d1,2b

    move.l  #0x40000003,(a0)        | plane A at 0xC000: 64 rows of 64 cells
    moveq   #1,d0
    moveq   #63,d1
4:  moveq   #63,d2
5:
    .if VARIANT == 0
    move.w  d0,(a2)
    .else
    move.w  #0,(a2)
    .endif
    dbf     d2,5b
    addq.w  #1,d0
    cmpi.w  #15,d0
    bne.s   6f
    moveq   #1,d0
6:  dbf     d1,4b
    move.w  #4095,d1                | plane B at 0xE000 (and the scroll table inside it): empty
7:  move.w  #0,(a2)
    dbf     d1,7b

    vdp_reg 1,0x44                  | display on
    moveq   #PASSES-1,d7
pass:
    jsr     wait_vblank_start
    .if VARIANT == 0
    move.l  #0x40000010,(a0)        | vertical scroll of plane A back to 0
    move.w  #0,(a2)
    .else
    move.w  #0x8700,(a0)            | backdrop back to colour 0
    .endif
    jsr     wait_vblank_end
    clr.w   (RESULT_COUNT).l        | this pass's words start the block over
8:  cmpi.b  #1,(a1)                 | into the picture: from here the HV word only grows
    bne.s   8b
    lea     targets(pc),a3
    moveq   #N-1,d6
    moveq   #1,d5                   | k + 1
next:
    move.w  (a3)+,d1
    .if VARIANT == 0
    move.l  #0x40000010,(a0)
    move.w  d5,d4
    lsl.w   #3,d4
    .else
    move.w  #0x8700,d4
    add.w   d5,d4
    .endif
9:  move.w  (a1),d0
    subi.b  #0xA5,d0
    cmp.w   d1,d0
    bcs.s   9b
    .if VARIANT == 0
    move.w  d4,(a2)                 | the write
    .else
    move.w  d4,(a0)
    .endif
    move.w  (a1),d0
    jsr     res_w
    addq.w  #1,d5
    dbf     d6,next
    dbf     d7,pass
    rts

targets:
    tgt 0,0x08
    tgt 1,0x18
    tgt 2,0x28
    tgt 3,0x38
    tgt 4,0x48
    tgt 5,0x58
    tgt 6,0x68
    tgt 7,0x78
    tgt 8,0x80
    tgt 9,0x88
    tgt 10,0x90
    tgt 11,0x98
    tgt 12,0xA0
    tgt 13,0xA4
    tgt 14,0xA8
    tgt 15,0xAC
    tgt 16,0xB0
    tgt 17,0xB4
    tgt 18,0xE4
    tgt 19,0xE8
    tgt 20,0xEC
    tgt 21,0xF4
    tgt 22,0xF8
    tgt 23,0xFC
