| frames: 12
| Is a horizontal-interrupt request latched while the display is off, and on vertical blanking lines?
| Register 10 = 0 (a request every counted line), CPU masked. A request raised while the VDP disables
| the interrupt is kept (hint_pend), so each case first drains: at the start of its window it enables
| the interrupt in the VDP and unmasks for two NOPs, taking whatever the lines before left. At the end
| of the window it unmasks for two NOPs again: only a request raised inside the window is taken then.
| Per case: interrupts taken at the end of the window, V counter byte the first one read.
|   case 1   window = V 0x32 to 0x64 with the display off (register 1 = 0x04) throughout
|   case 2   window = V 0xE8 to 0xFC, first time round: blanking lines only
|   case 3   window = V 0x64 to 0x66, display on: the control, a request is expected

    .include "probe.inc"
    .globl  main, hint

| Unmasks for two NOPs with the counters cleared.
.macro window_edge
    moveq   #0,d5
    moveq   #0,d6
    move.w  #0x2300,sr
    nop
    nop
    move.w  #0x2700,sr
.endm

.macro wait_v v
1:  cmpi.b  #\v,(VDP_HV).l
    bne.s   1b
.endm

.macro result
    move.w  d5,d0
    jsr     res_w
    move.w  d6,d0
    jsr     res_w
.endm

main:
    vdp_reg 1,0x44                  | display on, V interrupt off
    vdp_reg 10,0
    vdp_reg 0,0x04

    | case 1
    jsr     wait_vblank_start
    jsr     wait_vblank_end
    wait_v  0x32
    vdp_reg 1,0x04                  | display off
    vdp_reg 0,0x14
    window_edge
    wait_v  0x64
    window_edge
    vdp_reg 0,0x04
    vdp_reg 1,0x44
    result

    | case 2
    jsr     wait_vblank_start
    jsr     wait_vblank_end
    jsr     wait_vblank_start
    wait_v  0xE8
    vdp_reg 0,0x14
    window_edge
    wait_v  0xFC
    window_edge
    vdp_reg 0,0x04
    result

    | case 3
    jsr     wait_vblank_start
    jsr     wait_vblank_end
    wait_v  0x64
    vdp_reg 0,0x14
    window_edge
    wait_v  0x66
    window_edge
    vdp_reg 0,0x04
    result
    rts

hint:
    addq.w  #1,d5
    cmpi.w  #1,d5
    bne.s   1f
    move.b  (VDP_HV).l,d6
1:  rte
