| frames: 12
| Length of a frame in iterations of a fixed loop (addq.l 8 + btst #n,(a0) 12 + taken short branch
| 10 = 30 CPU clocks = 210 master clocks), from one rise of the vertical-blank flag to the next.
| An NTSC frame of 262 x 3420 master clocks is 4266.9 iterations when no access is slowed down.
| Result: two longs. The first is a full frame; the second starts a little after the rise
| (the cost of storing the first), so it is a few iterations shorter.

    .include "probe.inc"
    .globl  main

main:
    vdp_reg 1,0x44                  | display on: the blank flag follows the beam
    lea     (VDP_STATUS_LO).l,a0
    jsr     wait_vblank_start
    moveq   #1,d2
again:
    moveq   #0,d0
1:  addq.l  #1,d0                   | while the flag is set
    btst    #ST_VBLANK,(a0)
    bne.s   1b
2:  addq.l  #1,d0                   | while it is clear
    btst    #ST_VBLANK,(a0)
    beq.s   2b
    jsr     res_l
    dbf     d2,again
    rts
