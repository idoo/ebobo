| frames: 70
| Which request an interrupt acknowledge clears when the horizontal and the vertical one are both
| pending. Register 10 = 0 and both interrupts enabled in the VDP, so a horizontal request is always
| waiting while the CPU masks it. 60 passes, one frame each: wait until the V counter reads 0xDF
| and the H counter reads 0xE8 or more (the vertical request appears near the V step to 0xE0),
| burn 33 dbf passes then 8 + 2n CPU cycles (n = pass number, `lsl.l d2,d1`): the unmask moves in
| 2-cycle steps over H 0x8C..0xF3 of line 0xDF/0xE0, read the HV word, unmask for about two lines, mask. The handlers count
| themselves and log a tag byte (4 or 6) and the V counter byte. Per pass, 4 words:
|   V interrupts taken << 8 + H interrupts taken
|   first log entry (tag << 8 + V), second log entry
|   HV word read just before unmasking
| Early passes: 4 then 6 (one of each). Late passes: 6 then 4. If the acknowledge of the horizontal
| interrupt clears the vertical request instead, passes in between read 0002 with 4, 4.

    .include "probe.inc"
    .globl  main, hint, vint

main:
    vdp_reg 10,0
    vdp_reg 0,0x14                  | horizontal interrupt on
    vdp_reg 1,0x64                  | display on, vertical interrupt on
    moveq   #0,d4
    move.b  #0xDF,d3
next:
    jsr     wait_vblank_end
    lea     (SCRATCH).l,a2
    clr.l   (a2)
    clr.l   4(a2)
    lea     8(a2),a1
    moveq   #0,d5
    moveq   #0,d6
    moveq   #32,d0
    move.w  d4,d2
    moveq   #1,d1
1:  cmp.b   (VDP_HV).l,d3
    bne.s   1b
3:  cmpi.b  #0xE8,(VDP_H).l         | the H counter jumps from 0xB6 to 0xE4, then wraps to 0
    bcs.s   3b
4:  dbf     d0,4b
    lsl.l   d2,d1
    move.w  (VDP_HV).l,d7
    move.w  #0x2300,sr
    moveq   #100,d1
2:  dbf     d1,2b
    move.w  #0x2700,sr
    move.w  d6,d0
    lsl.w   #8,d0
    or.w    d5,d0
    jsr     res_w
    move.w  (SCRATCH).l,d0
    jsr     res_w
    move.w  (SCRATCH+2).l,d0
    jsr     res_w
    move.w  d7,d0
    jsr     res_w
    addq.w  #1,d4
    cmpi.w  #60,d4
    bne.s   next
    vdp_reg 1,0x44
    vdp_reg 0,0x04
    rts

hint:
    addq.w  #1,d5
    cmpa.l  a1,a2
    bcc.s   1f
    move.b  #4,(a2)+
    move.b  (VDP_HV).l,(a2)+
1:  rte

vint:
    addq.w  #1,d6
    cmpa.l  a1,a2
    bcc.s   1f
    move.b  #6,(a2)+
    move.b  (VDP_HV).l,(a2)+
1:  rte
