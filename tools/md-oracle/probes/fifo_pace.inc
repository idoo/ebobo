| Body of the fifo_pace probes (included right after `main:`; the including file picks the width).
| When the 68000 gets free after N back-to-back data-port word writes. Interrupts are masked,
| the display is on. For N = 1, 2, 3, 4, 5, 6, 8, 12, 16, 24 (in this order, on lines 40 + 4k,
| k = 0..9): wait for the line and the H counter, read the HV counter, write N words, read the HV
| counter and the status word. Three result words each: HV before, HV after, status AND 0x0300
| (bit 9 FIFO empty, bit 8 full; bits 15-10 are not driven by the chip).
| The HV word after is flat up to N = 4 (nothing held), then steps once per held write.
| Result words (30 per case):
|   [0..29]     video RAM words, started at H >= 0x10
|   [30..59]    video RAM words, started at H >= 0x80
|   [60..89]    video RAM words, started at H >= 0xE8 (the blanking part of the line)
|   [90..119]   colour RAM words, started at H >= 0x10
|   [120..149]  video RAM words, started at H >= 0x10, display off (register 1 = 0x04)
|   [150..179]  video RAM words in vertical blanking: V counter 0xE1 + k, H >= 0x10, one a frame

    lea     (VDP_DATA).l,a0
    lea     (VDP_HV).l,a1
    lea     (VDP_CTRL).l,a2
    move.w  #0x2700,sr
    vdp_reg 1,0x44                  | display on
    moveq   #0,d3
    moveq   #0,d1                   | d1 = 0: lines of the picture

| One measurement: n words, started on the line whose V counter reads `line`, at H >= d7.
.macro pace n, line
    move.w  #\line,d6
    jsr     pace_wait
    move.w  (a1),d4
    .rept \n
    move.w  d3,(a0)
    .endr
    move.w  (a1),d5
    move.w  (a2),d0
    jsr     pace_store
.endm

| The ten measurements of a case, `step` lines apart from `base`.
.macro ten base, step
    pace 1,\base
    pace 2,\base+\step
    pace 3,\base+2*\step
    pace 4,\base+3*\step
    pace 5,\base+4*\step
    pace 6,\base+5*\step
    pace 8,\base+6*\step
    pace 12,\base+7*\step
    pace 16,\base+8*\step
    pace 24,\base+9*\step
.endm

    move.l  #0x40000000,(a2)        | video RAM write at 0x0000
    move.w  #0x10,d7
    ten     40,4

    move.l  #0x40000000,(a2)
    move.w  #0x80,d7
    ten     40,4

    move.l  #0x40000000,(a2)
    move.w  #0xE8,d7
    ten     40,4

    move.l  #0xC0000000,(a2)        | colour RAM write at 0
    move.w  #0x10,d7
    ten     40,4

    vdp_reg 1,0x04                  | display off
    move.l  #0x40000000,(a2)
    ten     40,4
    vdp_reg 1,0x44                  | display on

    move.l  #0x40000000,(a2)
    moveq   #1,d1                   | d1 = 1: first pass V counter 0xDF, then the line
    ten     0xE1,1
    rts

| Returns on the line whose V counter reads d6.b once the H counter is >= d7.b. With d1 != 0 it
| first waits for V counter 0xDF (the lines after it are in vertical blanking).
pace_wait:
    tst.b   d1
    beq.s   1f
0:  cmpi.b  #0xDF,(a1)
    bne.s   0b
1:  cmp.b   (a1),d6
    bne.s   1b
2:  cmpi.b  #0x40,1(a1)             | the V counter steps at the right edge: wait for the line's start
    bcc.s   2b
3:  cmp.b   1(a1),d7
    bhi.s   3b
    rts

| Appends d4 (HV before), d5 (HV after) and d0 (status).
pace_store:
    move.w  d0,d2
    andi.w  #0x0300,d2
    move.w  d4,d0
    jsr     res_w
    move.w  d5,d0
    jsr     res_w
    move.w  d2,d0
    jsr     res_w
    rts
