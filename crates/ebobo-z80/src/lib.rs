//! ebobo-z80: the Zilog Z80 (the Mega Drive's sound CPU).
//!
//! Contract (the Mega Drive system and the single-step tests are written against it; change
//! it only together with them):
//! - [`Cpu::step`] runs one instruction (prefixes included), or accepts one interrupt, and
//!   returns its length in T-states as on the real chip.
//! - Documented and undocumented instructions and flags (bits 3 and 5 of F, `wz`, `q`)
//!   behave as the SingleStepTests cases say.
//! - The field names match those cases, so a test loads and compares state directly.
//! - Deterministic plain data: no clocks, no floats, no randomness.
//!
//! Written from Zilog's manual and published hardware research, not from another
//! emulator's code.

mod alu;
mod cb;
mod ed;

use serde::{Deserialize, Serialize};

/// The Z80's view of the machine.
pub trait Bus {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, val: u8);
    /// IN: the full 16-bit port address. The Mega Drive has no ports (reads give 0xFF).
    fn port_in(&mut self, _port: u16) -> u8 {
        0xFF
    }
    fn port_out(&mut self, _port: u16, _val: u8) {}
    /// Byte on the data bus during interrupt acknowledge (used by modes 0 and 2).
    fn int_ack(&mut self) -> u8 {
        0xFF
    }
}

/// Flag bits of F.
pub mod flag {
    pub const C: u8 = 0x01;
    pub const N: u8 = 0x02;
    pub const PV: u8 = 0x04;
    pub const X: u8 = 0x08;
    pub const H: u8 = 0x10;
    pub const Y: u8 = 0x20;
    pub const Z: u8 = 0x40;
    pub const S: u8 = 0x80;
}

/// Complete CPU state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cpu {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    /// Shadow pairs AF', BC', DE', HL'.
    pub af_: u16,
    pub bc_: u16,
    pub de_: u16,
    pub hl_: u16,
    pub ix: u16,
    pub iy: u16,
    pub sp: u16,
    pub pc: u16,
    /// Internal MEMPTR register (shows in undocumented flags of BIT n,(HL) and others).
    pub wz: u16,
    pub i: u8,
    pub r: u8,
    pub iff1: bool,
    pub iff2: bool,
    /// Interrupt mode 0-2.
    pub im: u8,
    /// EI ran last: maskable interrupts stay off until after the next instruction.
    pub ei: bool,
    /// LD A,I / LD A,R ran last (an interrupt accepted right after resets P/V).
    pub p: bool,
    /// F as the last instruction wrote it, or 0 when it did not touch the flags (SCF/CCF
    /// bits 3 and 5 depend on it).
    pub q: u8,
    pub halted: bool,
    /// The /INT line is low. Level sensitive: the system keeps it up to date.
    pub irq: bool,
    /// An /NMI edge is waiting to be taken.
    pub nmi: bool,
}

/// Which register pair stands for HL in the current instruction (set by a DD / FD prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Idx {
    Hl,
    Ix,
    Iy,
}

impl Cpu {
    /// State after power-on followed by [`Cpu::reset`].
    pub fn new() -> Cpu {
        let mut cpu = Cpu { a: 0xFF, f: 0xFF, sp: 0xFFFF, ..Cpu::default() };
        cpu.reset();
        cpu
    }

    /// /RESET: PC, I and R to zero, interrupts off, mode 0, not halted. The other registers
    /// keep their values.
    pub fn reset(&mut self) {
        self.pc = 0;
        self.i = 0;
        self.r = 0;
        self.iff1 = false;
        self.iff2 = false;
        self.im = 0;
        self.ei = false;
        self.p = false;
        self.q = 0;
        self.halted = false;
        self.nmi = false;
    }

    /// R + 1 in its low 7 bits; bit 7 kept (one per opcode fetch, M1 cycle).
    pub(crate) fn inc_r(&mut self) {
        self.r = (self.r & 0x80) | (self.r.wrapping_add(1) & 0x7F);
    }

    /// Reads the byte at PC and advances PC (operands, displacements; R unchanged).
    pub(crate) fn fetch(&mut self, bus: &mut impl Bus) -> u8 {
        let v = bus.read(self.pc);
        self.pc = self.pc.wrapping_add(1);
        v
    }

    /// Opcode fetch (M1): [`Cpu::fetch`] plus R + 1.
    pub(crate) fn fetch_m1(&mut self, bus: &mut impl Bus) -> u8 {
        self.inc_r();
        self.fetch(bus)
    }

    /// Little-endian word at PC; PC + 2.
    pub(crate) fn fetch16(&mut self, bus: &mut impl Bus) -> u16 {
        let lo = self.fetch(bus) as u16;
        lo | (self.fetch(bus) as u16) << 8
    }

    /// Little-endian word at `addr`.
    pub(crate) fn read16(bus: &mut impl Bus, addr: u16) -> u16 {
        bus.read(addr) as u16 | (bus.read(addr.wrapping_add(1)) as u16) << 8
    }

    /// Little-endian word to `addr`, low byte first.
    pub(crate) fn write16(bus: &mut impl Bus, addr: u16, v: u16) {
        bus.write(addr, v as u8);
        bus.write(addr.wrapping_add(1), (v >> 8) as u8);
    }

    /// PUSH: high byte to SP-1, low byte to SP-2.
    pub(crate) fn push16(&mut self, bus: &mut impl Bus, v: u16) {
        self.sp = self.sp.wrapping_sub(1);
        bus.write(self.sp, (v >> 8) as u8);
        self.sp = self.sp.wrapping_sub(1);
        bus.write(self.sp, v as u8);
    }

    /// POP: low byte from SP, high byte from SP+1.
    pub(crate) fn pop16(&mut self, bus: &mut impl Bus) -> u16 {
        let v = Cpu::read16(bus, self.sp);
        self.sp = self.sp.wrapping_add(2);
        v
    }

    pub(crate) fn af(&self) -> u16 {
        (self.a as u16) << 8 | self.f as u16
    }
    pub(crate) fn set_af(&mut self, v: u16) {
        self.a = (v >> 8) as u8;
        self.f = v as u8;
    }
    pub(crate) fn bc(&self) -> u16 {
        (self.b as u16) << 8 | self.c as u16
    }
    pub(crate) fn set_bc(&mut self, v: u16) {
        self.b = (v >> 8) as u8;
        self.c = v as u8;
    }
    pub(crate) fn de(&self) -> u16 {
        (self.d as u16) << 8 | self.e as u16
    }
    pub(crate) fn set_de(&mut self, v: u16) {
        self.d = (v >> 8) as u8;
        self.e = v as u8;
    }
    pub(crate) fn hl(&self) -> u16 {
        (self.h as u16) << 8 | self.l as u16
    }
    pub(crate) fn set_hl(&mut self, v: u16) {
        self.h = (v >> 8) as u8;
        self.l = v as u8;
    }

    /// HL, IX or IY.
    pub(crate) fn idx(&self, idx: Idx) -> u16 {
        match idx {
            Idx::Hl => self.hl(),
            Idx::Ix => self.ix,
            Idx::Iy => self.iy,
        }
    }
    pub(crate) fn set_idx(&mut self, idx: Idx, v: u16) {
        match idx {
            Idx::Hl => self.set_hl(v),
            Idx::Ix => self.ix = v,
            Idx::Iy => self.iy = v,
        }
    }

    /// Register numbered 0-7 as in opcode bits (B C D E H L - A); 4 and 5 are IXH/IXL or
    /// IYH/IYL under a prefix. 6 is memory and never comes here.
    pub(crate) fn reg(&self, idx: Idx, n: u8) -> u8 {
        match n & 7 {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => (self.idx(idx) >> 8) as u8,
            5 => self.idx(idx) as u8,
            7 => self.a,
            _ => unreachable!("register 6 is (HL)"),
        }
    }
    pub(crate) fn set_reg(&mut self, idx: Idx, n: u8, v: u8) {
        match n & 7 {
            0 => self.b = v,
            1 => self.c = v,
            2 => self.d = v,
            3 => self.e = v,
            4 => {
                let w = self.idx(idx);
                self.set_idx(idx, (w & 0x00FF) | (v as u16) << 8);
            }
            5 => {
                let w = self.idx(idx);
                self.set_idx(idx, (w & 0xFF00) | v as u16);
            }
            7 => self.a = v,
            _ => unreachable!("register 6 is (HL)"),
        }
    }

    /// Pair numbered by opcode bits 4-5: BC DE HL(IX/IY) SP.
    pub(crate) fn rp(&self, idx: Idx, p: u8) -> u16 {
        match p & 3 {
            0 => self.bc(),
            1 => self.de(),
            2 => self.idx(idx),
            _ => self.sp,
        }
    }
    pub(crate) fn set_rp(&mut self, idx: Idx, p: u8, v: u16) {
        match p & 3 {
            0 => self.set_bc(v),
            1 => self.set_de(v),
            2 => self.set_idx(idx, v),
            _ => self.sp = v,
        }
    }

    /// Condition numbered by opcode bits 3-5: NZ Z NC C PO PE P M.
    pub(crate) fn cond(&self, y: u8) -> bool {
        let f = self.f;
        match y & 7 {
            0 => f & flag::Z == 0,
            1 => f & flag::Z != 0,
            2 => f & flag::C == 0,
            3 => f & flag::C != 0,
            4 => f & flag::PV == 0,
            5 => f & flag::PV != 0,
            6 => f & flag::S == 0,
            _ => f & flag::S != 0,
        }
    }

    /// Address of the memory operand: HL, or IX/IY + d (reads d, sets `wz`).
    fn mem_addr(&mut self, bus: &mut impl Bus, idx: Idx) -> u16 {
        if idx == Idx::Hl {
            return self.hl();
        }
        let d = self.fetch(bus) as i8;
        let a = self.idx(idx).wrapping_add(d as u16);
        self.wz = a;
        a
    }

    /// One instruction or one interrupt acceptance; returns its length in T-states. While
    /// halted it runs NOPs of 4 T-states (R still counts). An endless run of DD / FD prefixes
    /// does not return, as the chip does not accept interrupts between them.
    pub fn step(&mut self, bus: &mut impl Bus) -> u32 {
        let prev_q = self.q;
        self.q = 0;
        let after_ei = self.ei;
        self.ei = false;
        let after_p = self.p;
        self.p = false;

        if self.nmi {
            self.nmi = false;
            self.halted = false;
            self.iff1 = false;
            self.inc_r();
            let pc = self.pc;
            self.push16(bus, pc);
            self.pc = 0x66;
            self.wz = 0x66;
            return 11;
        }
        if self.irq && self.iff1 && !after_ei {
            self.halted = false;
            self.iff1 = false;
            self.iff2 = false;
            self.inc_r();
            if after_p {
                self.f &= !flag::PV;
            }
            let byte = bus.int_ack();
            let pc = self.pc;
            self.push16(bus, pc);
            let t = match self.im {
                0 => {
                    self.pc = (byte & 0x38) as u16;
                    13
                }
                1 => {
                    self.pc = 0x38;
                    13
                }
                _ => {
                    self.pc = Cpu::read16(bus, (self.i as u16) << 8 | byte as u16);
                    19
                }
            };
            self.wz = self.pc;
            return t;
        }
        if self.halted {
            self.inc_r();
            return 4;
        }

        let mut t = 0;
        let mut idx = Idx::Hl;
        loop {
            let op = self.fetch_m1(bus);
            match op {
                0xDD => idx = Idx::Ix,
                0xFD => idx = Idx::Iy,
                0xED => return t + ed::exec(self, bus),
                0xCB if idx == Idx::Hl => return t + cb::exec(self, bus, None),
                0xCB => {
                    let d = self.fetch(bus) as i8;
                    let addr = self.idx(idx).wrapping_add(d as u16);
                    self.wz = addr;
                    return t + cb::exec(self, bus, Some(addr));
                }
                _ => return t + self.exec_main(bus, op, idx, prev_q),
            }
            t += 4;
        }
    }

    /// One unprefixed opcode (or DD/FD-prefixed, `idx` naming the pair); T-states without the prefix.
    fn exec_main(&mut self, bus: &mut impl Bus, op: u8, idx: Idx, prev_q: u8) -> u32 {
        let x = op >> 6;
        let y = (op >> 3) & 7;
        let z = op & 7;
        let p = y >> 1;
        let q = y & 1;
        // Extra T-states of an (IX+d) operand over (HL).
        let ext = if idx == Idx::Hl { 0 } else { 8 };
        match x {
            0 => match z {
                0 => match y {
                    0 => 4,
                    1 => {
                        let af = self.af();
                        self.set_af(self.af_);
                        self.af_ = af;
                        4
                    }
                    2 => {
                        self.b = self.b.wrapping_sub(1);
                        let d = self.fetch(bus) as i8;
                        if self.b != 0 {
                            self.pc = self.pc.wrapping_add(d as u16);
                            self.wz = self.pc;
                            13
                        } else {
                            8
                        }
                    }
                    _ => {
                        let d = self.fetch(bus) as i8;
                        if y == 3 || self.cond(y - 4) {
                            self.pc = self.pc.wrapping_add(d as u16);
                            self.wz = self.pc;
                            12
                        } else {
                            7
                        }
                    }
                },
                1 => {
                    if q == 0 {
                        let v = self.fetch16(bus);
                        self.set_rp(idx, p, v);
                        10
                    } else {
                        let r = self.add16(self.idx(idx), self.rp(idx, p));
                        self.set_idx(idx, r);
                        11
                    }
                }
                2 => match (q, p) {
                    (0, 0 | 1) => {
                        let addr = if p == 0 { self.bc() } else { self.de() };
                        bus.write(addr, self.a);
                        self.wz = (self.a as u16) << 8 | (addr.wrapping_add(1) & 0xFF);
                        7
                    }
                    (0, 2) => {
                        let nn = self.fetch16(bus);
                        Cpu::write16(bus, nn, self.idx(idx));
                        self.wz = nn.wrapping_add(1);
                        16
                    }
                    (0, _) => {
                        let nn = self.fetch16(bus);
                        bus.write(nn, self.a);
                        self.wz = (self.a as u16) << 8 | (nn.wrapping_add(1) & 0xFF);
                        13
                    }
                    (_, 0 | 1) => {
                        let addr = if p == 0 { self.bc() } else { self.de() };
                        self.a = bus.read(addr);
                        self.wz = addr.wrapping_add(1);
                        7
                    }
                    (_, 2) => {
                        let nn = self.fetch16(bus);
                        let v = Cpu::read16(bus, nn);
                        self.set_idx(idx, v);
                        self.wz = nn.wrapping_add(1);
                        16
                    }
                    _ => {
                        let nn = self.fetch16(bus);
                        self.a = bus.read(nn);
                        self.wz = nn.wrapping_add(1);
                        13
                    }
                },
                3 => {
                    let v = self.rp(idx, p);
                    let v = if q == 0 { v.wrapping_add(1) } else { v.wrapping_sub(1) };
                    self.set_rp(idx, p, v);
                    6
                }
                4 | 5 => {
                    if y == 6 {
                        let addr = self.mem_addr(bus, idx);
                        let v = bus.read(addr);
                        let r = if z == 4 { self.inc8(v) } else { self.dec8(v) };
                        bus.write(addr, r);
                        11 + ext
                    } else {
                        let v = self.reg(idx, y);
                        let r = if z == 4 { self.inc8(v) } else { self.dec8(v) };
                        self.set_reg(idx, y, r);
                        4
                    }
                }
                6 => {
                    if y == 6 {
                        let addr = self.mem_addr(bus, idx);
                        let n = self.fetch(bus);
                        bus.write(addr, n);
                        if idx == Idx::Hl { 10 } else { 15 }
                    } else {
                        let n = self.fetch(bus);
                        self.set_reg(idx, y, n);
                        7
                    }
                }
                _ => {
                    match y {
                        0 => self.rlca(),
                        1 => self.rrca(),
                        2 => self.rla(),
                        3 => self.rra(),
                        4 => self.daa(),
                        5 => self.cpl(),
                        6 => self.scf(prev_q),
                        _ => self.ccf(prev_q),
                    }
                    4
                }
            },
            1 => {
                if y == 6 && z == 6 {
                    self.halted = true;
                    4
                } else if y == 6 {
                    let addr = self.mem_addr(bus, idx);
                    bus.write(addr, self.reg(Idx::Hl, z));
                    7 + ext
                } else if z == 6 {
                    let addr = self.mem_addr(bus, idx);
                    let v = bus.read(addr);
                    self.set_reg(Idx::Hl, y, v);
                    7 + ext
                } else {
                    let v = self.reg(idx, z);
                    self.set_reg(idx, y, v);
                    4
                }
            }
            2 => {
                if z == 6 {
                    let addr = self.mem_addr(bus, idx);
                    let v = bus.read(addr);
                    self.alu8(y, v);
                    7 + ext
                } else {
                    let v = self.reg(idx, z);
                    self.alu8(y, v);
                    4
                }
            }
            _ => match z {
                0 => {
                    if self.cond(y) {
                        self.pc = self.pop16(bus);
                        self.wz = self.pc;
                        11
                    } else {
                        5
                    }
                }
                1 => {
                    if q == 0 {
                        let v = self.pop16(bus);
                        if p == 3 { self.set_af(v) } else { self.set_rp(idx, p, v) }
                        10
                    } else {
                        match p {
                            0 => {
                                self.pc = self.pop16(bus);
                                self.wz = self.pc;
                                10
                            }
                            1 => {
                                let (bc, de, hl) = (self.bc(), self.de(), self.hl());
                                self.set_bc(self.bc_);
                                self.set_de(self.de_);
                                self.set_hl(self.hl_);
                                self.bc_ = bc;
                                self.de_ = de;
                                self.hl_ = hl;
                                4
                            }
                            2 => {
                                self.pc = self.idx(idx);
                                4
                            }
                            _ => {
                                self.sp = self.idx(idx);
                                6
                            }
                        }
                    }
                }
                2 => {
                    let nn = self.fetch16(bus);
                    self.wz = nn;
                    if self.cond(y) {
                        self.pc = nn;
                    }
                    10
                }
                3 => match y {
                    0 => {
                        let nn = self.fetch16(bus);
                        self.wz = nn;
                        self.pc = nn;
                        10
                    }
                    2 => {
                        let n = self.fetch(bus);
                        bus.port_out((self.a as u16) << 8 | n as u16, self.a);
                        self.wz = (self.a as u16) << 8 | (n.wrapping_add(1) as u16);
                        11
                    }
                    3 => {
                        let n = self.fetch(bus);
                        let port = (self.a as u16) << 8 | n as u16;
                        self.a = bus.port_in(port);
                        self.wz = port.wrapping_add(1);
                        11
                    }
                    4 => {
                        let v = Cpu::read16(bus, self.sp);
                        Cpu::write16(bus, self.sp, self.idx(idx));
                        self.set_idx(idx, v);
                        self.wz = v;
                        19
                    }
                    5 => {
                        let de = self.de();
                        self.set_de(self.hl());
                        self.set_hl(de);
                        4
                    }
                    6 => {
                        self.iff1 = false;
                        self.iff2 = false;
                        4
                    }
                    7 => {
                        self.iff1 = true;
                        self.iff2 = true;
                        self.ei = true;
                        4
                    }
                    _ => unreachable!("CB is a prefix"),
                },
                4 => {
                    let nn = self.fetch16(bus);
                    self.wz = nn;
                    if self.cond(y) {
                        let pc = self.pc;
                        self.push16(bus, pc);
                        self.pc = nn;
                        17
                    } else {
                        10
                    }
                }
                5 => {
                    if q == 0 {
                        let v = if p == 3 { self.af() } else { self.rp(idx, p) };
                        self.push16(bus, v);
                        11
                    } else {
                        // p == 0: CALL nn (DD, ED, FD are prefixes).
                        let nn = self.fetch16(bus);
                        self.wz = nn;
                        let pc = self.pc;
                        self.push16(bus, pc);
                        self.pc = nn;
                        17
                    }
                }
                6 => {
                    let n = self.fetch(bus);
                    self.alu8(y, n);
                    7
                }
                _ => {
                    let pc = self.pc;
                    self.push16(bus, pc);
                    self.pc = (y as u16) * 8;
                    self.wz = self.pc;
                    11
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Ram {
        mem: Vec<u8>,
        ack: u8,
        out: Vec<(u16, u8)>,
        inp: Vec<u16>,
    }

    impl Bus for Ram {
        fn read(&mut self, addr: u16) -> u8 {
            self.mem[addr as usize]
        }
        fn write(&mut self, addr: u16, val: u8) {
            self.mem[addr as usize] = val;
        }
        fn port_in(&mut self, port: u16) -> u8 {
            self.inp.push(port);
            0x5A
        }
        fn port_out(&mut self, port: u16, val: u8) {
            self.out.push((port, val));
        }
        fn int_ack(&mut self) -> u8 {
            self.ack
        }
    }

    fn setup(prog: &[u8]) -> (Cpu, Ram) {
        let mut ram = Ram { mem: vec![0; 0x10000], ack: 0xFF, out: Vec::new(), inp: Vec::new() };
        ram.mem[..prog.len()].copy_from_slice(prog);
        let mut cpu = Cpu::new();
        cpu.f = 0;
        cpu.sp = 0xF000;
        (cpu, ram)
    }

    /// Runs `op v` on A = `a` with F = `f`; returns (A, F).
    fn alu(op: u8, a: u8, f: u8, v: u8) -> (u8, u8) {
        let (mut cpu, mut ram) = setup(&[op, v]);
        cpu.a = a;
        cpu.f = f;
        assert_eq!(cpu.step(&mut ram), 7);
        assert_eq!(cpu.q, cpu.f);
        (cpu.a, cpu.f)
    }

    #[test]
    fn eight_bit_flags() {
        assert_eq!(alu(0xC6, 0x44, 0, 0x11), (0x55, 0x00));
        assert_eq!(alu(0xC6, 0x7F, 0, 0x01), (0x80, 0x94));
        assert_eq!(alu(0xC6, 0xFF, 0, 0x01), (0x00, 0x51));
        assert_eq!(alu(0xCE, 0x16, flag::C, 0x10), (0x27, 0x20));
        assert_eq!(alu(0xD6, 0x29, 0, 0x11), (0x18, 0x0A));
        assert_eq!(alu(0xD6, 0x00, 0, 0x01), (0xFF, 0xBB));
        assert_eq!(alu(0xDE, 0x16, flag::C, 0x34), (0xE1, 0xA3));
        assert_eq!(alu(0xFE, 0x63, 0, 0x60), (0x63, 0x22));
        assert_eq!(alu(0xFE, 0x63, 0, 0x63), (0x63, 0x62));
        assert_eq!(alu(0xE6, 0xF0, 0, 0x3C), (0x30, 0x34));
        assert_eq!(alu(0xEE, 0x0F, 0, 0x0F), (0x00, 0x44));
    }

    #[test]
    fn daa_inc_dec() {
        // 0x15 + 0x27 = 0x3C, DAA gives 0x42.
        let (mut cpu, mut ram) = setup(&[0xC6, 0x27, 0x27, 0xD6, 0x15, 0x27]);
        cpu.a = 0x15;
        cpu.step(&mut ram);
        assert_eq!(cpu.step(&mut ram), 4);
        assert_eq!((cpu.a, cpu.f), (0x42, 0x14));
        // 0x42 - 0x15 = 0x2D, DAA gives 0x27.
        cpu.step(&mut ram);
        cpu.step(&mut ram);
        assert_eq!((cpu.a, cpu.f), (0x27, 0x26));

        let (mut cpu, mut ram) = setup(&[0x3C, 0x3D, 0x05]);
        cpu.a = 0x7F;
        cpu.f = flag::C;
        cpu.b = 0x01;
        assert_eq!(cpu.step(&mut ram), 4);
        assert_eq!((cpu.a, cpu.f), (0x80, 0x95));
        cpu.f = 0;
        cpu.step(&mut ram);
        assert_eq!((cpu.a, cpu.f), (0x7F, 0x3E));
        cpu.step(&mut ram);
        assert_eq!((cpu.b, cpu.f), (0x00, 0x42));
    }

    #[test]
    fn scf_uses_previous_q() {
        let (mut cpu, mut ram) = setup(&[0x37, 0x37]);
        cpu.a = 0;
        cpu.f = 0x28;
        cpu.q = 0x28;
        cpu.step(&mut ram);
        assert_eq!(cpu.f, 0x01);
        cpu.f = 0x28;
        cpu.q = 0;
        cpu.step(&mut ram);
        assert_eq!(cpu.f, 0x29);
        // An instruction without flag effects leaves q at 0.
        let (mut cpu, mut ram) = setup(&[0x00]);
        cpu.q = 0x55;
        cpu.step(&mut ram);
        assert_eq!(cpu.q, 0);
    }

    #[test]
    fn neg_and_16_bit_carry_forms() {
        let mut cpu = Cpu::new();
        cpu.a = 1;
        cpu.neg();
        assert_eq!((cpu.a, cpu.f), (0xFF, 0xBB));
        cpu.set_hl(0x7FFF);
        cpu.f = flag::C;
        cpu.adc16(0);
        assert_eq!((cpu.hl(), cpu.f, cpu.wz), (0x8000, 0x94, 0x8000));
        cpu.set_hl(0);
        cpu.f = 0;
        cpu.sbc16(1);
        assert_eq!((cpu.hl(), cpu.f), (0xFFFF, 0xBB));
        let (mut cpu, mut ram) = setup(&[0x09]);
        cpu.set_hl(0x0FFF);
        cpu.set_bc(0x0001);
        assert_eq!(cpu.step(&mut ram), 11);
        assert_eq!((cpu.hl(), cpu.f, cpu.wz), (0x1000, 0x10, 0x1000));
    }

    #[test]
    fn branch_timings() {
        // JR +2 taken, JR NZ with Z set not taken.
        let (mut cpu, mut ram) = setup(&[0x18, 0x02, 0, 0, 0x20, 0x10]);
        assert_eq!(cpu.step(&mut ram), 12);
        assert_eq!((cpu.pc, cpu.wz), (4, 4));
        cpu.f = flag::Z;
        assert_eq!(cpu.step(&mut ram), 7);
        assert_eq!(cpu.pc, 6);
        // DJNZ: taken while B != 0.
        let (mut cpu, mut ram) = setup(&[0x10, 0xFE]);
        cpu.b = 2;
        assert_eq!(cpu.step(&mut ram), 13);
        assert_eq!(cpu.pc, 0);
        assert_eq!(cpu.step(&mut ram), 8);
        assert_eq!((cpu.pc, cpu.b), (2, 0));
        // CALL NZ taken, RET Z not taken, RET NZ taken, CALL NZ not taken.
        let (mut cpu, mut ram) = setup(&[0xC4, 0x00, 0x10, 0xC4, 0x00, 0x10]);
        ram.mem[0x1000] = 0xC8;
        ram.mem[0x1001] = 0xC0;
        assert_eq!(cpu.step(&mut ram), 17);
        assert_eq!((cpu.pc, cpu.sp, ram.mem[0xEFFE], ram.mem[0xEFFF]), (0x1000, 0xEFFE, 0x03, 0x00));
        assert_eq!(cpu.step(&mut ram), 5);
        assert_eq!(cpu.step(&mut ram), 11);
        assert_eq!((cpu.pc, cpu.sp, cpu.wz), (3, 0xF000, 3));
        cpu.f = flag::Z;
        assert_eq!(cpu.step(&mut ram), 10);
        assert_eq!((cpu.pc, cpu.wz), (6, 0x1000));
    }

    #[test]
    fn index_forms() {
        let (mut cpu, mut ram) = setup(&[
            0xDD, 0x7E, 0x05, // LD A,(IX+5)
            0xDD, 0x36, 0xFE, 0x99, // LD (IX-2),0x99
            0xDD, 0x34, 0x02, // INC (IX+2)
            0xDD, 0x26, 0x12, // LD IXH,0x12
            0xDD, 0x66, 0x01, // LD H,(IX+1)
            0xDD, 0xFD, 0x21, 0x34, 0x12, // LD IY,0x1234 after a dropped DD
            0xDD, 0xEB, // EX DE,HL (not IX)
            0xFD, 0xE9, // JP (IY)
        ]);
        cpu.ix = 0x1000;
        ram.mem[0x1005] = 0x42;
        ram.mem[0x1002] = 0x7F;
        ram.mem[0x1001] = 0x77;
        assert_eq!(cpu.step(&mut ram), 19);
        assert_eq!((cpu.a, cpu.wz), (0x42, 0x1005));
        assert_eq!(cpu.step(&mut ram), 19);
        assert_eq!(ram.mem[0x0FFE], 0x99);
        assert_eq!(cpu.step(&mut ram), 23);
        assert_eq!((ram.mem[0x1002], cpu.f & flag::PV), (0x80, flag::PV));
        assert_eq!(cpu.step(&mut ram), 11);
        assert_eq!((cpu.ix, cpu.h), (0x1200, 0));
        cpu.ix = 0x1000;
        assert_eq!(cpu.step(&mut ram), 19);
        assert_eq!((cpu.h, cpu.ix), (0x77, 0x1000));
        assert_eq!(cpu.step(&mut ram), 18);
        assert_eq!((cpu.iy, cpu.ix), (0x1234, 0x1000));
        cpu.set_de(0xAAAA);
        assert_eq!(cpu.step(&mut ram), 8);
        assert_eq!((cpu.hl(), cpu.de(), cpu.ix), (0xAAAA, 0x7700, 0x1000));
        assert_eq!(cpu.step(&mut ram), 8);
        assert_eq!(cpu.pc, 0x1234);
        // Each opcode fetch counted R: 4 two-byte + 1 three-prefix... plus the rest.
        assert_eq!(cpu.r, 2 + 2 + 2 + 2 + 2 + 3 + 2 + 2);
    }

    #[test]
    fn ei_delay_and_mode_1() {
        let (mut cpu, mut ram) = setup(&[0xFB, 0x00, 0x00]);
        cpu.im = 1;
        cpu.irq = true;
        assert_eq!(cpu.step(&mut ram), 4);
        assert!(cpu.ei);
        assert_eq!(cpu.step(&mut ram), 4);
        assert_eq!(cpu.pc, 2);
        assert_eq!(cpu.step(&mut ram), 13);
        assert_eq!((cpu.pc, cpu.sp, ram.mem[0xEFFE], cpu.iff1, cpu.iff2), (0x38, 0xEFFE, 2, false, false));
    }

    #[test]
    fn modes_0_and_2() {
        let (mut cpu, mut ram) = setup(&[0x00]);
        cpu.iff1 = true;
        cpu.irq = true;
        ram.ack = 0xCF;
        assert_eq!(cpu.step(&mut ram), 13);
        assert_eq!(cpu.pc, 0x08);
        let (mut cpu, mut ram) = setup(&[0x00]);
        cpu.iff1 = true;
        cpu.irq = true;
        cpu.im = 2;
        cpu.i = 0x80;
        ram.ack = 0x10;
        ram.mem[0x8010] = 0x34;
        ram.mem[0x8011] = 0x12;
        assert_eq!(cpu.step(&mut ram), 19);
        assert_eq!((cpu.pc, cpu.wz), (0x1234, 0x1234));
    }

    #[test]
    fn nmi_and_halt() {
        let (mut cpu, mut ram) = setup(&[0x76, 0x00]);
        cpu.iff1 = true;
        cpu.iff2 = true;
        cpu.im = 1;
        assert_eq!(cpu.step(&mut ram), 4);
        assert!(cpu.halted);
        assert_eq!(cpu.step(&mut ram), 4);
        assert_eq!((cpu.pc, cpu.r), (1, 2));
        cpu.nmi = true;
        assert_eq!(cpu.step(&mut ram), 11);
        assert_eq!((cpu.pc, cpu.halted, cpu.iff1, cpu.iff2, cpu.nmi), (0x66, false, false, true, false));
        assert_eq!(ram.mem[0xEFFE], 1);
        // HALT ends on a maskable interrupt too.
        let (mut cpu, mut ram) = setup(&[0x76]);
        cpu.iff1 = true;
        cpu.im = 1;
        cpu.step(&mut ram);
        cpu.irq = true;
        assert_eq!(cpu.step(&mut ram), 13);
        assert_eq!((cpu.pc, cpu.halted, ram.mem[0xEFFE]), (0x38, false, 1));
    }

    #[test]
    fn r_keeps_bit_7_and_ports_use_a() {
        let (mut cpu, mut ram) = setup(&[0x00, 0xD3, 0x34, 0xDB, 0x56]);
        cpu.r = 0xFF;
        cpu.step(&mut ram);
        assert_eq!(cpu.r, 0x80);
        cpu.a = 0x12;
        assert_eq!(cpu.step(&mut ram), 11);
        assert_eq!((ram.out[0], cpu.wz), ((0x1234, 0x12), 0x1235));
        assert_eq!(cpu.step(&mut ram), 11);
        assert_eq!((ram.inp[0], cpu.a, cpu.wz), (0x1256, 0x5A, 0x1257));
    }
}
