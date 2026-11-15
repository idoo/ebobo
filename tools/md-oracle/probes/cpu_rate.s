| frames: 30
| How much of a frame the 68000 gets, by the kind of instruction it runs. The vertical interrupt
| ends each measurement: its handler stores the pass counter d7 as a result word, clears it, and
| after every second interrupt puts the next loop's address into the stacked PC.
| Result: 24 words, two intervals per shape in the order of `shapes` (the first interval of the
| first shape starts at start-up, every other one is a whole frame less the handler).
| An NTSC frame is 896040 / 7 = 128005.7 CPU cycles, a PAL frame 152922.9.
| The cycles of one pass are written next to each loop; `tools/md-oracle/cpu_rate.py` has the
| same table and prints the fraction of the frame the CPU got.

    .include "probe.inc"
    .globl  main, vint

main:
    movea.l sp,a5                   | finish returns from here whatever the loops left on the stack
    lea     (SCRATCH).l,a0
    lea     (SCRATCH+4).l,a1
    lea     romdata(pc),a2
    lea     shapes(pc),a6
    move.l  #0x0000FFFF,d2          | divisor / multiplier
    moveq   #63,d4                  | shift count
    moveq   #0,d6                   | flips on every interrupt
    moveq   #0,d7
    vdp_reg 1,0x64                  | display on, vertical interrupt on
    move.w  #0x2500,sr
    movea.l (a6)+,a3
    jmp     (a3)

vint:
    move.w  d7,d0
    jsr     res_w
    moveq   #0,d7
    not.w   d6
    bne.s   1f
    move.l  (a6)+,2(sp)             | second interrupt of this shape: return into the next one
1:  rte

finish:
    move.w  #0x2700,sr
    movea.l a5,sp
    rts

romdata:
    .long   0x12345678

shapes:
    .long   reg0, reg2, reg5, rmw, ram2ram, romread, divu, mulu, lsl63, io, vdpstat, pushpop, finish

| registers only: 18 cycles
reg0:
    addq.l  #1,d7                   | 8
    bra.s   reg0                    | 10

| 2 nops: 26 cycles
reg2:
    nop                             | 4
    nop
    addq.l  #1,d7
    bra.s   reg2

| 5 nops: 38 cycles
reg5:
    nop
    nop
    nop
    nop
    nop
    addq.l  #1,d7
    bra.s   reg5

| work RAM read-modify-write: 38 cycles
rmw:
    addq.l  #1,(a0)                 | 20
    addq.l  #1,d7
    bra.s   rmw

| work RAM to work RAM: 38 cycles
ram2ram:
    move.l  (a0),(a1)               | 20
    addq.l  #1,d7
    bra.s   ram2ram

| cartridge data read: 30 cycles
romread:
    move.l  (a2),d1                 | 12
    addq.l  #1,d7
    bra.s   romread

| longest divide (1 / 0xFFFF: every quotient bit 0): 158 cycles
divu:
    moveq   #1,d1                   | 4
    divu    d2,d1                   | 136
    addq.l  #1,d7
    bra.s   divu

| multiply by 0xFFFF (16 one bits): 88 cycles
mulu:
    mulu    d2,d1                   | 38 + 2 x 16 = 70
    addq.l  #1,d7
    bra.s   mulu

| shift by 63: 152 cycles
lsl63:
    lsl.l   d4,d1                   | 8 + 2 x 63 = 134
    addq.l  #1,d7
    bra.s   lsl63

| I/O read: 34 cycles
io:
    move.b  (IO_VERSION).l,d1       | 16
    addq.l  #1,d7
    bra.s   io

| video status read: 34 cycles
vdpstat:
    move.w  (VDP_STATUS).l,d1       | 16
    addq.l  #1,d7
    bra.s   vdpstat

| push and pop on the stack in work RAM: 42 cycles
pushpop:
    move.l  d1,-(sp)                | 12
    move.l  (sp)+,d1                | 12
    addq.l  #1,d7
    bra.s   pushpop
