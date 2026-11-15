# Timing probes

Tiny cartridges of our own that measure Mega Drive timing. Each runs in Genesis Plus GX (the
reference, used as a black box) and in ebobo's core, and the result words are compared.

- `rt.s` is the runtime every probe links with: vectors, header, start-up (TMSS, VDP registers,
  RAM clear, Z80 in reset, PSG silent), then `jsr main`. `probe.inc` holds the port constants and
  the `vdp_reg r, v` macro. `link.ld` places vectors, header and code.
- Result block at RAM 0xFF0000: word 0 = 0xD0E1 when `main` returned (0xDEAD after an unexpected
  exception, whose result word is 0xEE00 + vector number), word 1 = word count, then the words.
  Append with `jsr res_w` (d0.w) or `jsr res_l` (d0.l); both keep every register.
- `wait_vblank_start` / `wait_vblank_end` poll the status port. The runtime leaves the display
  off, and then the vertical-blank flag reads 1 all the time: turn the display on first.
- A probe may define `hint` (level 4) and `vint` (level 6); otherwise they are a bare `rte`.
- The first line of a probe is `| frames: N`, the frames both emulators run before the RAM is read.

Write `name.s`, then:

```sh
sh tools/md-oracle/probes/build.sh          # Docker image ebobo-md-probes; writes out/*.bin (committed)
python3 tools/md-oracle/probe.py [name]     # both emulators, words side by side, << on differences
python3 tools/md-oracle/probe.py --record   # reference words into expect/<name>[.pal].txt
cargo test -p ebobo-md --test probes -- --nocapture
```

The test fails on a differing probe unless it is listed in `KNOWN_DIFF` in
`crates/ebobo-md/tests/probes.rs` with its reason.
