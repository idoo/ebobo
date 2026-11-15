//! Everything on the two buses: both CPUs' memory maps, the devices behind them, and the
//! stepping of each CPU against the master clock. Owner: bus slice.
//!
//! Contract with `system.rs` (which owns the frame loop, snapshots and the public `Md`):
//! - [`State`] is the whole emulated state, plain data; `system.rs` clones, hashes and
//!   serializes it. Nothing else in [`Machine`] feeds back into it.
//! - Time is master clocks since power-on. `State::now` is where the 68000 is; an
//!   instruction's bus accesses all happen at the `now` it started at.
//! - 68000 map: 0x000000-0x3FFFFF cartridge; 0xA00000-0xA0FFFF the Z80's address space
//!   (its RAM, the YM2612, the bank register), reachable while the 68000 holds the Z80's
//!   bus; 0xA10000-0xA1001F controller ports; 0xA11100 Z80 bus request (bit 8; reads give
//!   bit 8 = 0 once the bus is granted); 0xA11200 Z80 reset (bit 8 = 0 holds the Z80 and the
//!   YM2612 in reset); 0xA13000-0xA130FF cartridge registers; 0xC00000-0xC0001F VDP ports
//!   (PSG writes at 0xC00011); 0xE00000-0xFFFFFF 64 KB RAM, mirrored. Unmapped reads give
//!   0xFF bytes.
//! - Z80 map: 0x0000-0x1FFF 8 KB RAM (mirrored to 0x3FFF); 0x4000-0x5FFF YM2612 (ports 0-3
//!   mirrored); 0x6000-0x60FF bank register (a write shifts bit 0 of the value in from the
//!   top of 9 bits); 0x7F00-0x7F1F VDP ports (PSG writes at 0x7F11); 0x8000-0xFFFF a 32 KB
//!   window of 68000 memory picked by the bank register.
//! - Before any device access the device is brought to the current time: `Vdp::run_to`,
//!   the sound chips through [`Machine::sync_sound`].
//!
//! Details the map above leaves open, as this file does them:
//! - The 68000 reaches the Z80's address space while it requests the bus or while the Z80 is
//!   in reset (the grant is immediate); otherwise reads give 0xFF and writes are lost. Only
//!   the low 32 KB exist from that side (0xA08000 mirrors 0xA00000).
//! - The Z80 is run up to `now` before either of its lines (0xA11100, 0xA11200) changes, so
//!   it stops and starts at the master clock of the write.
//! - Sound chip accesses of the Z80 happen at `z80_now`, those of the 68000 at `now`; the
//!   chips only move forward, so an access behind their position lands at their position.
//! - Through the Z80's window only the cartridge and RAM answer. The VDP and the controller
//!   ports there, and reads of the VDP ports at 0x7F00, give 0xFF and ignore writes.
//! - The VDP ports are mirrored as the console decodes them: any address in
//!   0xC00000-0xDFFFFF with bits 16-18, 8-10 and 5-7 clear.

use serde::{Deserialize, Serialize};

use crate::audio::AudioOut;
use crate::cart::{Cart, CartState};
use crate::io::Io;
use crate::psg::Psg;
use crate::vdp::{DmaSource, Screen, Vdp};
use crate::ym2612::Ym2612;
use crate::{FM_SAMPLE_MCLK, M68K_DIV, MCLK_NTSC, MCLK_PAL, PSG_SAMPLE_MCLK, Region, Z80_DIV};

/// The whole emulated state of the console.
#[derive(Clone, Hash, Serialize, Deserialize)]
pub(crate) struct State {
    pub m68k: ebobo_m68k::Cpu,
    pub z80: ebobo_z80::Cpu,
    /// 64 KB work RAM.
    pub ram: Vec<u8>,
    /// 8 KB Z80 RAM.
    pub zram: Vec<u8>,
    pub vdp: Vdp,
    pub ym: Ym2612,
    pub psg: Psg,
    pub io: Io,
    pub cart: CartState,
    /// Master clock the 68000 has run to.
    pub now: u64,
    /// Master clock the Z80 has run to.
    pub z80_now: u64,
    /// Master clocks at which the next FM and PSG samples are due.
    pub fm_next: u64,
    pub psg_next: u64,
    /// The 68000 asked for the Z80's bus (0xA11100): the Z80 stands still.
    pub z80_busreq: bool,
    /// The Z80's reset line is released (0xA11200). While it is held the Z80 does not run
    /// and the YM2612 is in reset.
    pub z80_run: bool,
    /// The Z80's 9-bit bank register: bits 15-23 of the 68000 address its window shows.
    pub z80_bank: u32,
}

/// The console: emulated state plus what is not state (the cartridge as built, the picture,
/// the audio output).
pub(crate) struct Machine {
    pub s: State,
    pub cart: Cart,
    pub screen: Screen,
    pub audio: AudioOut,
    /// False in fast-forward: no pixels are drawn and the audio output is muted. Emulated
    /// state is the same either way.
    pub output: bool,
}

const CART_END: u32 = 0x40_0000;
const RAM_START: u32 = 0xE0_0000;

/// Cartridge and work RAM: the 68000 memory that a DMA and the Z80's window read.
struct Mem<'a> {
    cart: &'a Cart,
    cs: &'a mut CartState,
    ram: &'a mut [u8],
}

impl Mem<'_> {
    /// `addr` is a 24-bit 68000 address. Anything but cartridge and RAM gives 0xFF.
    fn read8(&self, addr: u32) -> u8 {
        if addr < CART_END {
            self.cart.read8(self.cs, addr)
        } else if addr >= RAM_START {
            self.ram[(addr & 0xFFFF) as usize]
        } else {
            0xFF
        }
    }

    fn write8(&mut self, addr: u32, val: u8) {
        if addr < CART_END {
            self.cart.write8(self.cs, addr, val);
        } else if addr >= RAM_START {
            self.ram[(addr & 0xFFFF) as usize] = val;
        }
    }
}

impl DmaSource for Mem<'_> {
    fn dma_read16(&mut self, addr: u32) -> u16 {
        let addr = addr & 0xFF_FFFE;
        u16::from_be_bytes([self.read8(addr), self.read8(addr | 1)])
    }
}

/// True for the addresses the console decodes as VDP ports.
fn is_vdp(addr: u32) -> bool {
    addr & 0xE7_00E0 == 0xC0_0000
}

impl Machine {
    /// Power-on with `cart` in the slot: memories cleared, both CPUs reset (the 68000 loads
    /// its stack pointer and PC from the cartridge; `tas_no_write` on), the Z80 held in
    /// reset, the console's region and video standard from `cart.info().region`.
    pub(crate) fn new(cart: Cart) -> Machine {
        let region = cart.info().region;
        let pal = region == Region::Europe;
        let mut m68k = ebobo_m68k::Cpu::new();
        m68k.tas_no_write = true;
        let s = State {
            m68k,
            z80: ebobo_z80::Cpu::new(),
            ram: vec![0; 0x10000],
            zram: vec![0; 0x2000],
            vdp: Vdp::new(pal),
            ym: Ym2612::new(),
            psg: Psg::new(),
            io: Io::new(region),
            cart: cart.power_on(),
            now: 0,
            z80_now: 0,
            fm_next: FM_SAMPLE_MCLK,
            psg_next: PSG_SAMPLE_MCLK,
            z80_busreq: false,
            z80_run: false,
            z80_bank: 0,
        };
        let audio = AudioOut::new(if pal { MCLK_PAL } else { MCLK_NTSC });
        let mut machine = Machine { s, cart, screen: Screen::new(), audio, output: true };
        machine.reset_m68k();
        machine
    }

    /// Reset button: both CPUs and the YM2612 reset, RAM and the VDP keep their contents.
    /// The Z80 is held in reset again, its bus is not requested and its bank register is 0,
    /// as after power-on.
    pub(crate) fn reset(&mut self) {
        self.sync_sound();
        self.s.ym.reset();
        self.s.z80.reset();
        self.s.z80_run = false;
        self.s.z80_busreq = false;
        self.s.z80_bank = 0;
        self.reset_m68k();
    }

    /// Resets the 68000 through the bus: SSP and PC come from the cartridge's vectors.
    fn reset_m68k(&mut self) {
        let mut cpu = std::mem::take(&mut self.s.m68k);
        cpu.reset(&mut M68kView(self));
        self.s.m68k = cpu;
    }

    /// One 68000 instruction or interrupt at `s.now`, with the VDP's interrupt level on its
    /// pins (the acknowledge clears the VDP's request); advances `s.now` by its length.
    /// While a DMA holds the bus, moves `s.now` to the end of the hold instead.
    pub(crate) fn step_m68k(&mut self) {
        self.sync_vdp();
        let frozen = self.s.vdp.m68k_frozen_until();
        if frozen > self.s.now {
            self.s.now = frozen;
            return;
        }
        let mut cpu = std::mem::take(&mut self.s.m68k);
        cpu.irq = self.s.vdp.irq_level();
        let cycles = cpu.step(&mut M68kView(self));
        self.s.m68k = cpu;
        self.s.now += u64::from(cycles) * M68K_DIV;
    }

    /// Runs the Z80 from `s.z80_now` up to `s.now`, its /INT line following the VDP; while
    /// it is in reset or its bus is taken, time just passes.
    ///
    /// The VDP only moves forward, so /INT is the VDP's line as it stands (at most at
    /// `s.now`), not as it was at `z80_now`: the Z80 sees an edge up to one slice late.
    pub(crate) fn run_z80(&mut self) {
        while self.s.z80_now < self.s.now {
            if !self.s.z80_run || self.s.z80_busreq {
                self.s.z80_now = self.s.now;
                break;
            }
            let mut cpu = std::mem::take(&mut self.s.z80);
            cpu.irq = self.s.vdp.z80_irq();
            let t_states = cpu.step(&mut Z80View(self));
            self.s.z80 = cpu;
            self.s.z80_now += u64::from(t_states) * Z80_DIV;
        }
    }

    /// Brings the VDP to `s.now`, drawing into the screen when `output` is on.
    pub(crate) fn sync_vdp(&mut self) {
        self.s.vdp.run_to(self.s.now, if self.output { Some(&mut self.screen) } else { None });
    }

    /// Runs the YM2612 and the PSG up to `s.now`, one sample period at a time in time order,
    /// feeding the audio output.
    pub(crate) fn sync_sound(&mut self) {
        self.sync_sound_to(self.s.now);
    }

    /// Runs both sound chips up to master clock `t` (nothing happens when they are past it).
    fn sync_sound_to(&mut self, t: u64) {
        let s = &mut self.s;
        loop {
            if s.fm_next <= s.psg_next {
                if s.fm_next > t {
                    break;
                }
                let (left, right) = s.ym.tick();
                self.audio.fm(left, right);
                s.fm_next += FM_SAMPLE_MCLK;
            } else {
                if s.psg_next > t {
                    break;
                }
                let sample = s.psg.tick();
                self.audio.psg(sample);
                s.psg_next += PSG_SAMPLE_MCLK;
            }
        }
    }

    /// Cartridge and RAM, borrowed apart from the rest of the state.
    fn mem(&mut self) -> Mem<'_> {
        Mem { cart: &self.cart, cs: &mut self.s.cart, ram: &mut self.s.ram }
    }

    /// A word read of VDP port `addr` at `s.now`.
    fn vdp_read(&mut self, addr: u32) -> u16 {
        self.sync_vdp();
        self.s.vdp.read(addr, self.s.now)
    }

    /// A word write to VDP port `addr` at `s.now`; a DMA it starts reads cartridge and RAM.
    fn vdp_write(&mut self, addr: u32, val: u16) {
        self.sync_vdp();
        let now = self.s.now;
        let State { vdp, ram, cart, .. } = &mut self.s;
        vdp.write(addr, val, now, &mut Mem { cart: &self.cart, cs: cart, ram });
    }

    /// A byte to the PSG at master clock `t`.
    fn psg_write(&mut self, val: u8, t: u64) {
        self.sync_sound_to(t);
        self.s.psg.write(val);
    }

    /// The Z80's address space below the banked window, at master clock `t`.
    fn zspace_read(&mut self, addr: u16, t: u64) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.s.zram[usize::from(addr & 0x1FFF)],
            0x4000..=0x5FFF => {
                self.sync_sound_to(t);
                self.s.ym.read()
            }
            // The bank register is write-only; the VDP ports are not wired up from this side.
            _ => 0xFF,
        }
    }

    fn zspace_write(&mut self, addr: u16, val: u8, t: u64) {
        match addr {
            0x0000..=0x3FFF => self.s.zram[usize::from(addr & 0x1FFF)] = val,
            0x4000..=0x5FFF => {
                self.sync_sound_to(t);
                self.s.ym.write((addr & 3) as u8, val);
            }
            0x6000..=0x60FF => self.s.z80_bank = (self.s.z80_bank >> 1) | (u32::from(val & 1) << 8),
            0x7F00..=0x7F1F if matches!(addr & 0x1F, 0x11 | 0x13 | 0x15 | 0x17) => self.psg_write(val, t),
            _ => {}
        }
    }

    /// The 68000 holds the Z80's bus: it asked for it, or the Z80 is in reset.
    fn z80_bus_granted(&self) -> bool {
        self.s.z80_busreq || !self.s.z80_run
    }

    /// 0xA11100: `request` is bit 8 of the word written.
    fn set_busreq(&mut self, request: bool) {
        if request != self.s.z80_busreq {
            self.run_z80();
            self.s.z80_busreq = request;
        }
    }

    /// 0xA11200: `released` is bit 8 of the word written. Releasing the line resets the Z80;
    /// holding it resets the YM2612.
    fn set_z80_reset(&mut self, released: bool) {
        if released {
            if !self.s.z80_run {
                self.run_z80();
                self.s.z80.reset();
                self.s.z80_run = true;
            }
        } else {
            self.run_z80();
            self.sync_sound();
            self.s.ym.reset();
            self.s.z80_run = false;
        }
    }
}

/// The 68000's side of the machine.
struct M68kView<'a>(&'a mut Machine);

impl ebobo_m68k::Bus for M68kView<'_> {
    fn read8(&mut self, addr: u32) -> u8 {
        let m = &mut *self.0;
        let addr = addr & 0xFF_FFFF;
        match addr {
            0x00_0000..=0x3F_FFFF | 0xE0_0000..=0xFF_FFFF => m.mem().read8(addr),
            0xA0_0000..=0xA0_FFFF if m.z80_bus_granted() => m.zspace_read((addr & 0x7FFF) as u16, m.s.now),
            0xA1_0000..=0xA1_001F => m.s.io.read((addr >> 1) & 0xF, m.s.now),
            // Bit 0 of the even byte is bit 8 of the word: 0 once the bus is granted.
            0xA1_1100 => u8::from(!m.s.z80_busreq),
            0xA1_1101 => 0,
            _ if is_vdp(addr) => {
                let [high, low] = m.vdp_read(addr & 0x1E).to_be_bytes();
                if addr & 1 == 0 { high } else { low }
            }
            _ => 0xFF,
        }
    }

    fn read16(&mut self, addr: u32) -> u16 {
        let m = &mut *self.0;
        let addr = addr & 0xFF_FFFE;
        match addr {
            // One access to the I/O chip: both bytes carry the register.
            0xA1_0000..=0xA1_001F => u16::from(m.s.io.read((addr >> 1) & 0xF, m.s.now)) * 0x0101,
            0xA1_1100 => u16::from(!m.s.z80_busreq) << 8,
            _ if is_vdp(addr) => m.vdp_read(addr & 0x1E),
            _ => u16::from_be_bytes([self.read8(addr), self.read8(addr | 1)]),
        }
    }

    fn write8(&mut self, addr: u32, val: u8) {
        let m = &mut *self.0;
        let addr = addr & 0xFF_FFFF;
        match addr {
            0x00_0000..=0x3F_FFFF | 0xE0_0000..=0xFF_FFFF => m.mem().write8(addr, val),
            0xA0_0000..=0xA0_FFFF if m.z80_bus_granted() => m.zspace_write((addr & 0x7FFF) as u16, val, m.s.now),
            0xA1_0000..=0xA1_001F if addr & 1 != 0 => m.s.io.write((addr >> 1) & 0xF, val, m.s.now),
            0xA1_1100 => m.set_busreq(val & 1 != 0),
            0xA1_1200 => m.set_z80_reset(val & 1 != 0),
            0xA1_3000..=0xA1_30FF => m.cart.write_reg(&mut m.s.cart, addr, val),
            _ if is_vdp(addr) => match addr & 0x1F {
                // The byte goes out on both halves of the data bus.
                port @ 0x00..=0x07 => m.vdp_write(port & 0x1E, u16::from(val) * 0x0101),
                0x11 | 0x13 | 0x15 | 0x17 => m.psg_write(val, m.s.now),
                _ => {}
            },
            _ => {}
        }
    }

    fn write16(&mut self, addr: u32, val: u16) {
        let m = &mut *self.0;
        let addr = addr & 0xFF_FFFE;
        match addr {
            0xA1_1100 => m.set_busreq(val & 0x0100 != 0),
            0xA1_1200 => m.set_z80_reset(val & 0x0100 != 0),
            _ if is_vdp(addr) => match addr & 0x1E {
                port @ 0x00..=0x0F => m.vdp_write(port, val),
                0x10..=0x16 => m.psg_write(val as u8, m.s.now),
                _ => {}
            },
            _ => {
                let [high, low] = val.to_be_bytes();
                self.write8(addr, high);
                self.write8(addr | 1, low);
            }
        }
    }

    fn int_ack(&mut self, level: u8) -> u8 {
        self.0.s.vdp.irq_ack(level);
        24 + level
    }
}

/// The Z80's side of the machine.
struct Z80View<'a>(&'a mut Machine);

impl Z80View<'_> {
    /// The 68000 address behind `addr` of the banked window.
    fn banked(&self, addr: u16) -> u32 {
        (self.0.s.z80_bank << 15) | u32::from(addr & 0x7FFF)
    }
}

impl ebobo_z80::Bus for Z80View<'_> {
    fn read(&mut self, addr: u16) -> u8 {
        if addr < 0x8000 {
            self.0.zspace_read(addr, self.0.s.z80_now)
        } else {
            // Cartridge and RAM only; the VDP and controller ports give 0xFF here.
            let target = self.banked(addr);
            self.0.mem().read8(target)
        }
    }

    fn write(&mut self, addr: u16, val: u8) {
        if addr < 0x8000 {
            self.0.zspace_write(addr, val, self.0.s.z80_now);
        } else {
            let target = self.banked(addr);
            self.0.mem().write8(target, val);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::hash::{DefaultHasher, Hash, Hasher};

    use ebobo_m68k::Bus as _;
    use ebobo_z80::Bus as _;

    use super::*;

    const SSP: u32 = 0x00FF_FE00;
    const ENTRY: u32 = 0x200;

    /// 64 KB image: vectors, a header, a small program, and a byte pattern from 0x8000 on.
    fn rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x10000];
        rom[0..4].copy_from_slice(&SSP.to_be_bytes());
        rom[4..8].copy_from_slice(&ENTRY.to_be_bytes());
        rom[0x100..0x110].copy_from_slice(b"SEGA MEGA DRIVE ");
        rom[0x1F0..0x1F3].copy_from_slice(b"U  ");
        let program: [u16; 8] = [
            0x4E71, // 0x200  NOP
            0x41F9, 0x00FF, 0x0000, // 0x202  LEA $FF0000,A0
            0x7000, // 0x208  MOVEQ #0,D0
            0x5240, // 0x20A  ADDQ.W #1,D0
            0x3080, // 0x20C  MOVE.W D0,(A0)
            0x60FA, // 0x20E  BRA.S $20A
        ];
        for (i, word) in program.iter().enumerate() {
            rom[0x200 + 2 * i..0x202 + 2 * i].copy_from_slice(&word.to_be_bytes());
        }
        for (i, byte) in rom[0x8000..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        rom
    }

    fn machine() -> Machine {
        Machine::new(Cart::new(&rom(), None).unwrap())
    }

    fn hash<T: Hash>(value: &T) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn reset_loads_ssp_and_pc_from_the_cartridge() {
        let mut m = machine();
        assert_eq!((m.s.m68k.ssp, m.s.m68k.pc), (SSP, ENTRY));
        assert!(m.s.m68k.tas_no_write && m.output);
        assert!(!m.s.z80_run && !m.s.z80_busreq);
        assert_eq!((m.s.ram.len(), m.s.zram.len()), (0x10000, 0x2000));
        assert_eq!((m.s.fm_next, m.s.psg_next), (1008, 240));
        m.s.m68k.pc = 0x1234;
        m.s.ram[5] = 9;
        m.reset();
        assert_eq!((m.s.m68k.ssp, m.s.m68k.pc, m.s.ram[5]), (SSP, ENTRY, 9));
    }

    #[test]
    fn ram_is_mirrored() {
        let mut m = machine();
        M68kView(&mut m).write16(0xFF_0010, 0x1234);
        assert_eq!(M68kView(&mut m).read16(0xE0_0010), 0x1234);
        assert_eq!(M68kView(&mut m).read8(0xE1_0011), 0x34);
        M68kView(&mut m).write8(0xE5_0010, 0xAB);
        assert_eq!(m.s.ram[0x10], 0xAB);
        assert_eq!(M68kView(&mut m).read16(0xFF_0010), 0xAB34);
        // Unmapped.
        assert_eq!(M68kView(&mut m).read16(0x80_0000), 0xFFFF);
    }

    #[test]
    fn control_port_word_sets_a_register() {
        let mut m = machine();
        M68kView(&mut m).write16(0xC0_0004, 0x8144);
        assert_eq!(m.s.vdp.reg(1), 0x44);
        M68kView(&mut m).write16(0xC0_0006, 0x8F02);
        assert_eq!(m.s.vdp.reg(15), 0x02);
    }

    #[test]
    fn z80_bus_request_and_reset_protocol() {
        let mut m = machine();
        // Z80 RAM is all NOPs: the PC tells how far it ran.
        m.s.now = 1500;
        m.run_z80();
        assert_eq!((m.s.z80.pc, m.s.z80_now), (0, 1500), "held in reset");

        M68kView(&mut m).write16(0xA1_1200, 0x0100);
        assert!(m.s.z80_run);
        m.s.now += 1500;
        m.run_z80();
        assert_eq!(m.s.z80.pc, 25, "25 NOPs of 4 T-states in 1500 master clocks");
        assert_eq!(M68kView(&mut m).read16(0xA1_1100) & 0x0100, 0x0100, "bus not granted");
        assert_eq!(M68kView(&mut m).read8(0xA0_0010), 0xFF, "no access without the bus");

        M68kView(&mut m).write16(0xA1_1100, 0x0100);
        assert_eq!(M68kView(&mut m).read16(0xA1_1100) & 0x0100, 0);
        assert_eq!(M68kView(&mut m).read8(0xA1_1100) & 1, 0);
        m.s.now += 1500;
        m.run_z80();
        assert_eq!((m.s.z80.pc, m.s.z80_now), (25, m.s.now), "stands still while requested");
        M68kView(&mut m).write8(0xA0_0010, 0x5A);
        assert_eq!(m.s.zram[0x10], 0x5A);
        assert_eq!(M68kView(&mut m).read8(0xA0_2010), 0x5A, "RAM mirror");

        M68kView(&mut m).write8(0xA1_1100, 0x00);
        m.s.now += 1500;
        m.run_z80();
        assert!(m.s.z80.pc > 25, "runs again once released");

        // Reset held: the Z80 stops; releasing it starts from address 0.
        M68kView(&mut m).write16(0xA1_1200, 0x0000);
        let pc = m.s.z80.pc;
        m.s.now += 1500;
        m.run_z80();
        assert_eq!(m.s.z80.pc, pc);
        M68kView(&mut m).write16(0xA1_1200, 0x0100);
        assert_eq!(m.s.z80.pc, 0);
    }

    #[test]
    fn bank_register_shifts_in_nine_bits_and_the_window_reads_the_cartridge() {
        let mut m = machine();
        let rom = rom();
        for _ in 0..9 {
            Z80View(&mut m).write(0x6000, 1);
        }
        assert_eq!(m.s.z80_bank, 0x1FF);
        // 0x008000: bit 0 first, then eight zeros.
        Z80View(&mut m).write(0x6000, 1);
        for _ in 0..8 {
            Z80View(&mut m).write(0x60FF, 0);
        }
        assert_eq!(m.s.z80_bank, 1);
        for addr in [0x8000u16, 0x8005, 0xFFFF] {
            assert_eq!(Z80View(&mut m).read(addr), rom[usize::from(addr)]);
        }
        // 0xFF8000: work RAM, read and written.
        for _ in 0..9 {
            Z80View(&mut m).write(0x6000, 1);
        }
        m.s.ram[0x8003] = 0x77;
        assert_eq!(Z80View(&mut m).read(0x8003), 0x77);
        Z80View(&mut m).write(0x8004, 0x78);
        assert_eq!(m.s.ram[0x8004], 0x78);
    }

    #[test]
    fn psg_byte_reaches_the_chip() {
        let mut m = machine();
        let mut psg = Psg::new();
        assert_eq!(hash(&m.s.psg), hash(&psg));
        for byte in [0x8E, 0x0F, 0x90] {
            M68kView(&mut m).write8(0xC0_0011, byte);
            psg.write(byte);
        }
        assert_eq!(hash(&m.s.psg), hash(&psg));
        Z80View(&mut m).write(0x7F11, 0xA3);
        psg.write(0xA3);
        assert_eq!(hash(&m.s.psg), hash(&psg));
    }

    #[test]
    fn sound_ticks_once_per_sample_period() {
        let mut m = machine();
        m.s.now = 1008 * 240;
        m.sync_sound();
        // FM samples at 1008, 2016, ... 1008*240; PSG samples at 240, 480, ... 240*1008.
        assert_eq!((m.s.fm_next, m.s.psg_next), (1008 * 241, 240 * 1009));
        let (mut ym, mut psg) = (Ym2612::new(), Psg::new());
        for _ in 0..240 {
            ym.tick();
        }
        for _ in 0..1008 {
            psg.tick();
        }
        assert_eq!((hash(&m.s.ym), hash(&m.s.psg)), (hash(&ym), hash(&psg)));
        m.sync_sound();
        assert_eq!((m.s.fm_next, m.s.psg_next), (1008 * 241, 240 * 1009));
    }

    #[test]
    fn step_advances_seven_master_clocks_per_cycle() {
        let mut m = machine();
        m.step_m68k();
        assert_eq!((m.s.now, m.s.m68k.pc), (4 * 7, 0x202), "NOP is 4 cycles");
        m.step_m68k();
        assert_eq!((m.s.now, m.s.m68k.pc), (4 * 7 + 12 * 7, 0x208), "LEA (xxx).L,A0 is 12 cycles");
    }

    #[test]
    fn step_jumps_over_a_dma_freeze() {
        let mut m = machine();
        // DMA on, increment 2, 16 words from 68000 address 0x000000 to VRAM 0x0000.
        for word in [0x8114, 0x8F02, 0x9310, 0x9400, 0x9500, 0x9600, 0x9700, 0x4000, 0x0080] {
            M68kView(&mut m).write16(0xC0_0004, word);
        }
        let frozen = m.s.vdp.m68k_frozen_until();
        assert!(frozen > m.s.now, "the DMA holds the 68000's bus");
        assert_eq!(&m.s.vdp.vram()[..8], &rom()[..8], "the DMA read the cartridge");
        m.step_m68k();
        assert_eq!((m.s.now, m.s.m68k.pc), (frozen, ENTRY), "time passed, nothing ran");
        m.step_m68k();
        assert_eq!((m.s.now, m.s.m68k.pc), (frozen + 28, 0x202));
    }

    #[test]
    fn clone_taken_mid_run_continues_identically() {
        fn run(m: &mut Machine, steps: usize) {
            for _ in 0..steps {
                m.step_m68k();
                m.run_z80();
                m.sync_sound();
            }
            m.sync_vdp();
        }
        let mut a = machine();
        M68kView(&mut a).write16(0xA1_1200, 0x0100);
        run(&mut a, 500);
        let mut b = machine();
        b.s = a.s.clone();
        b.output = false;
        assert_eq!(hash(&a.s), hash(&b.s));
        run(&mut a, 20_000);
        run(&mut b, 20_000);
        assert_eq!(hash(&a.s), hash(&b.s));
        assert_ne!(a.s.ram[..2], [0, 0], "the loop counts in RAM");
        assert!(a.s.z80.pc > 0 && a.s.now > 20_000 * 28);
    }
}
