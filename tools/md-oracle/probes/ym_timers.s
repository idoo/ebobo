| frames: 780
| The FM chip's timers, counted by the 68000 (interrupts masked, Z80 bus requested, reset released).
| Per case the timer is started on a vertical-blank edge; for exactly 64 frames the status byte is
| polled and every time the overflow flag is seen set it is counted and cleared through register 0x27.
| Result words: 6 counts for timer A values 0x000, 0x200, 0x300, 0x3C0, 0x3F0, 0x3FF (overflow every
| 1024 - N ticks of 144 FM clocks), 5 counts for timer B values 0x00, 0x80, 0xC0, 0xF0, 0xFF (every
| (256 - N) x 16 ticks), then the polls until the busy flag (bit 7) reads 0 after a data write
| (one poll = btst + dbeq, 22 CPU cycles; 0xFFFF = never), then the status bytes of 0xA04000/1 and
| 0xA04002/3 as two words, read with both flags set.
| `tools/md-oracle/sound_clock.py` turns the counts into ticks per second.

    .include "probe.inc"
    .globl  main

YM = 0xA04000

main:
    vdp_reg 1,0x44                  | display on: the blanking flag toggles
    move.w  #0x0100,(Z80_BUSREQ).l
    move.w  #0x0100,(Z80_RESET).l
1:  btst    #0,(Z80_BUSREQ).l
    bne.s   1b
    lea     (YM).l,a0
    lea     (VDP_STATUS_LO).l,a1

    lea     a_values(pc),a2
    moveq   #5,d4
2:  move.w  (a2)+,d3
    moveq   #0x24,d0
    move.w  d3,d1
    lsr.w   #2,d1
    bsr     ymw
    moveq   #0x25,d0
    move.w  d3,d1
    andi.w  #3,d1
    bsr     ymw
    moveq   #1,d1                   | flag bit
    moveq   #0x15,d2                | load A, flag A enabled, clear A
    bsr     count
    dbra    d4,2b

    lea     b_values(pc),a2
    moveq   #4,d4
3:  moveq   #0x26,d0
    moveq   #0,d1
    move.b  (a2)+,d1
    bsr     ymw
    moveq   #2,d1
    moveq   #0x2A,d2                | load B, flag B enabled, clear B
    bsr     count
    dbra    d4,3b

    | busy flag after a data write
    moveq   #0x30,d0
    bsr     wait_ready
    move.b  #0x26,(a0)
    moveq   #-1,d7
    move.b  #0xFF,1(a0)
4:  btst    #7,(a0)                 | 8
    dbeq    d7,4b                   | 14 (taken)
    move.w  d7,d0
    not.w   d0
    jsr     res_w

    | both flags set, then the four addresses
    moveq   #0x24,d0
    move.w  #0xFF,d1
    bsr     ymw
    moveq   #0x27,d0
    moveq   #0x3F,d1
    bsr     ymw
    move.w  #20000,d7
5:  dbra    d7,5b
    move.b  (a0),d0
    lsl.w   #8,d0
    move.b  1(a0),d0
    jsr     res_w
    move.b  2(a0),d0
    lsl.w   #8,d0
    move.b  3(a0),d0
    jsr     res_w
    moveq   #0x27,d0
    moveq   #0x30,d1
    bsr     ymw
    rts

| Waits (bounded) until the busy flag is clear.
wait_ready:
    move.w  #999,d7
1:  btst    #7,(a0)
    dbeq    d7,1b
    rts

| Register d0 = d1 on part I.
ymw:
    bsr.s   wait_ready
    move.b  d0,(a0)
    nop
    nop
    move.b  d1,1(a0)
    rts

| Starts the timer with control value d2 on a blanking edge and counts flag bit d1 for 64 frames.
count:
    moveq   #0x27,d0
    bsr.s   wait_ready
    move.b  d0,(a0)
1:  btst    #ST_VBLANK,(a1)         | leave the blanking
    bne.s   1b
2:  btst    #ST_VBLANK,(a1)         | its start
    beq.s   2b
    move.b  d2,1(a0)                | start
    st      d5                      | in the blanking
    moveq   #0,d7
    moveq   #64,d6
3:  move.b  (a0),d0
    and.b   d1,d0
    beq.s   4f
    addq.w  #1,d7
    move.b  #0x27,(a0)
    move.b  d2,1(a0)                | clear the flag (the timer keeps running: load bit unchanged)
4:  btst    #ST_VBLANK,(a1)
    bne.s   5f
    sf      d5
    bra.s   3b
5:  tst.b   d5
    bne.s   3b
    st      d5
    subq.w  #1,d6
    bne.s   3b
    move.w  d7,d0
    jsr     res_w
    moveq   #0x27,d0                | stop both
    moveq   #0x30,d1
    bsr.s   ymw
    rts

a_values:
    .word   0x000, 0x200, 0x300, 0x3C0, 0x3F0, 0x3FF
b_values:
    .byte   0x00, 0x80, 0xC0, 0xF0, 0xFF
    .align  2
