| frames: 30
| Which sprites show on picture line 0? A static scene, no writes after set-up (tools/md-oracle/sprite_line0.py).
| 320-pixel mode, sprite table at 0xD800 (runtime default), planes 64x32. Colour i is CRAM word COLOURS[i].
| Sprites, each in its own column (screen x = X - 128):
|   A  Y 128, 8x8,   colour 1, x 16     first row on line 0
|   B  Y 129, 8x8,   colour 2, x 48     first row on line 1
|   C  Y 127, 8x8,   colour 3, x 80     second row on line 0
|   D  Y 120, 32x32, colour 4, x 112    ninth row on line 0
|   E  Y 128, 8x8,   colour 5, x 160    high priority, first row on line 0
|   F  Y 140, 8x8,   colour 6, x 200    first row on line 12
| Plane A row 0, cells 36..39 (x 288..319): colour 7, so the planes show on line 0 too.
| No result words besides the done mark.

    .include "probe.inc"
    .globl  main

main:
    lea     (VDP_CTRL).l,a0
    lea     (VDP_DATA).l,a2

    move.l  #0xC0000000,(a0)        | CRAM 0..7
    lea     colours(pc),a1
    moveq   #7,d1
1:  move.w  (a1)+,(a2)
    dbf     d1,1b

    move.l  #0x40000000,(a0)        | tiles 0..23: tile t solid colour t (0..7), tiles 8..23 colour 4
    moveq   #0,d1
2:  move.w  d1,d0
    cmpi.w  #8,d0
    bcs.s   3f
    moveq   #4,d0
3:  mulu.w  #0x1111,d0
    moveq   #15,d2
4:  move.w  d0,(a2)
    dbf     d2,4b
    addq.w  #1,d1
    cmpi.w  #24,d1
    bne.s   2b

    move.l  #0x40000003,(a0)        | plane A at 0xC000 and plane B at 0xE000: empty
    move.w  #8191,d1
5:  move.w  #0,(a2)
    dbf     d1,5b
    move.l  #0x40480003,(a0)        | plane A row 0, cells 36..39 (0xC000 + 72): tile 7
    moveq   #3,d1
6:  move.w  #7,(a2)
    dbf     d1,6b
    move.l  #0x70000003,(a0)        | horizontal scroll table at 0xF000: 0
    move.w  #511,d1
7:  move.w  #0,(a2)
    dbf     d1,7b

    move.l  #0x58000003,(a0)        | sprite table at 0xD800
    lea     sprites(pc),a1
    moveq   #6*4-1,d1
8:  move.w  (a1)+,(a2)
    dbf     d1,8b

    vdp_reg 1,0x44                  | display on
    rts

colours:
    .word   0x000, 0x00E, 0x0E0, 0xE00, 0xEEE, 0x0EE, 0xE0E, 0x888

| Y, size << 8 + link, attributes (priority << 15 + tile), X
sprites:
    .word   128, 0x0001, 1, 128+16
    .word   129, 0x0002, 2, 128+48
    .word   127, 0x0003, 3, 128+80
    .word   120, 0x0F04, 8, 128+112
    .word   128, 0x0005, 0x8005, 128+160
    .word   140, 0x0000, 6, 128+200
