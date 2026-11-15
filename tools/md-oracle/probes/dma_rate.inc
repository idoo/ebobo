| Body of the dma_rate probes (included right after `main:`; the including file picks the width).
| How long each kind of DMA takes. Interrupts are masked. A 68000 transfer freezes the 68000, so
| the HV word read by the instruction after the trigger is where the transfer ended. Fill and copy
| leave the 68000 running: the status port's DMA-busy bit is polled in a tight loop.
| Result words:
|   [0],[1]    68000 -> video RAM, 128 words, from line 16: HV before the trigger, HV after
|   [2],[3]    68000 -> colour RAM, 64 words, from line 16
|   [4],[5]    68000 -> vertical scroll RAM, 40 words, from line 16
|   [6],[7]    68000 -> video RAM, 1024 words, from the moment the vertical-blank flag sets
|   [8],[9]    68000 -> video RAM, 1024 words, from line 16 with the display switched off
|   [10..12]   fill, 256 bytes, from line 16: HV before, HV when busy cleared, poll passes
|   [13..15]   copy, 128 bytes, from line 16: the same three
|   [16..23]   68000 -> video RAM, 16 words, started on line 32 at H 0x10, 0x50, 0x90, 0xE8:
|              HV before, HV after, for each

    lea     (VDP_DATA).l,a0
    lea     (VDP_HV).l,a1
    lea     (VDP_CTRL).l,a2
    vdp_reg 1,0x54                  | display on, DMA allowed

    move.w  #128,d1                 | 68000 -> video RAM
    bsr     set_len
    bsr     set_src_ram
    bsr     to_line_16
    move.w  #0x4000,d2
    move.w  #0x0080,d3
    bsr     bus_go

    move.w  #64,d1                  | 68000 -> colour RAM
    bsr     set_len
    bsr     set_src_ram
    bsr     to_line_16
    move.w  #0xC000,d2
    move.w  #0x0080,d3
    bsr     bus_go

    move.w  #40,d1                  | 68000 -> vertical scroll RAM
    bsr     set_len
    bsr     set_src_ram
    bsr     to_line_16
    move.w  #0x4000,d2
    move.w  #0x0090,d3
    bsr     bus_go

    move.w  #1024,d1                | 68000 -> video RAM in vertical blanking
    bsr     set_len
    bsr     set_src_ram
    jsr     wait_vblank_end
    jsr     wait_vblank_start
    move.w  #0x4000,d2
    move.w  #0x0080,d3
    bsr     bus_go

    move.w  #1024,d1                | 68000 -> video RAM, display off
    bsr     set_len
    bsr     set_src_ram
    bsr     to_line_16
    move.w  #0x8114,(a2)
    move.w  #0x4000,d2
    move.w  #0x0080,d3
    bsr     bus_go
    move.w  #0x8154,(a2)

    move.w  #256,d1                 | fill
    bsr     set_len
    move.w  #0x8F01,(a2)            | auto-increment 1
    move.w  #0x9780,(a2)
    bsr     to_line_16
    move.w  #0x4000,(a2)
    move.w  #0x0080,(a2)
    move.w  (a1),d5
    move.w  #0x5500,(a0)            | the fill byte: the trigger
    bsr     poll
    move.w  #0x8F02,(a2)

    move.w  #128,d1                 | copy, video RAM 0x1000 -> 0x2000
    bsr     set_len
    move.w  #0x9500,(a2)
    move.w  #0x9610,(a2)
    move.w  #0x97C0,(a2)
    bsr     to_line_16
    move.w  #0x2000,(a2)
    move.w  (a1),d5
    move.w  #0x00C0,(a2)            | the trigger
    bsr     poll

    moveq   #0x10,d4
    bsr     at_h
    moveq   #0x50,d4
    bsr     at_h
    move.w  #0x90,d4
    bsr     at_h
    move.w  #0xE8,d4
    bsr     at_h
    rts

| 68000 -> video RAM, 16 words, started on line 32 once the H counter reaches d4.b.
at_h:
    moveq   #16,d1
    bsr     set_len
    bsr     set_src_ram
    jsr     wait_vblank_end
    move.w  d4,d3
    moveq   #32,d4
    bsr     wait_line
    move.w  d3,d4
    bsr     wait_h
    move.w  #0x4000,d2
    move.w  #0x0080,d3
    bra     bus_go

| Registers 19-20 = d1.w, the DMA length.
set_len:
    move.w  #0x9300,d0
    move.b  d1,d0
    move.w  d0,(a2)
    move.w  d1,d0
    lsr.w   #8,d0
    addi.w  #0x9400,d0
    move.w  d0,(a2)
    rts

| Registers 21-23: a 68000 transfer from 0xFF8000 (zeros).
set_src_ram:
    move.w  #0x9500,(a2)
    move.w  #0x96C0,(a2)
    move.w  #0x977F,(a2)
    rts

| Returns when the V counter becomes 16 in the next picture.
to_line_16:
    jsr     wait_vblank_end
    moveq   #16,d4
| Returns when the V counter becomes d4.b.
wait_line:
1:  cmp.b   (a1),d4
    beq.s   1b
2:  cmp.b   (a1),d4
    bne.s   2b
    rts

| Returns when the H counter, having been below d4.b, reaches it.
wait_h:
1:  cmp.b   1(a1),d4
    bls.s   1b
2:  cmp.b   1(a1),d4
    bhi.s   2b
    rts

| Command words d2, d3: the second triggers a 68000 transfer. Records the HV word before and after.
bus_go:
    move.w  d2,(a2)
    move.w  (a1),d5
    move.w  d3,(a2)
    move.w  (a1),d6
rec2:
    move.w  d5,d0
    jsr     res_w
    move.w  d6,d0
    jsr     res_w
    rts

| Polls the DMA-busy bit until it clears; records d5, the HV word then and the number of passes.
poll:
    moveq   #0,d7
1:  addq.w  #1,d7
    move.w  (a2),d0
    btst    #1,d0
    bne.s   1b
    move.w  (a1),d6
    bsr     rec2
    move.w  d7,d0
    jsr     res_w
    rts
