| Constants and macros shared by the runtime (rt.s) and every probe. `|` starts a comment, so
| write sums with `+`, never with an or sign.

VDP_DATA      = 0xC00000
VDP_CTRL      = 0xC00004          | write: control port; read: status word
VDP_STATUS    = 0xC00004
VDP_STATUS_LO = 0xC00005          | low byte of the status: bit 3 vertical blank, bit 2 horizontal blank
VDP_HV        = 0xC00008          | byte: V counter
VDP_H         = 0xC00009          | byte: H counter
PSG           = 0xC00011
IO_VERSION    = 0xA10001
Z80_BUSREQ    = 0xA11100
Z80_RESET     = 0xA11200
TMSS          = 0xA14000

ST_VBLANK     = 3                 | bit numbers in VDP_STATUS_LO
ST_HBLANK     = 2

RAM           = 0xFF0000
RESULT_DONE   = 0xFF0000          | word: 0xD0E1 when main returned, 0xDEAD after an unexpected exception
RESULT_COUNT  = 0xFF0002          | word: number of result words
RESULT_WORDS  = 0xFF0004
SCRATCH       = 0xFF8000          | free for the probe, up to the stack (which grows down from 0xFFFE00)

DONE_MARK     = 0xD0E1
TRAP_MARK     = 0xDEAD

| Sets VDP register r (0..23) to v.
.macro vdp_reg r, v
    move.w  #0x8000+(\r<<8)+\v,(VDP_CTRL).l
.endm
