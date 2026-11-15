//! The ED table: IN r,(C) / OUT (C),r, SBC/ADC HL,rr, LD (nn),rr / LD rr,(nn), NEG, RETN/RETI,
//! IM 0/1/2, LD I,A / LD R,A / LD A,I / LD A,R, RRD / RLD, the block instructions
//! (LDI LDD LDIR LDDR, CPI CPD CPIR CPDR, INI IND INIR INDR, OUTI OUTD OTIR OTDR).
//!
//! - Every other ED opcode is a NOP of 8 T-states, as on the chip.
//! - Flags go through `Cpu::set_f` and the helpers of `alu.rs`.
//! - LD A,I and LD A,R set `cpu.p = true` (P/V = IFF2; an interrupt taken right after clears
//!   P/V, which `Cpu::step` does from `p`).
//! - A DD / FD prefix in front of ED has no effect: this table always works on HL.
//! - A repeating block instruction runs one iteration per call: while it repeats it moves PC
//!   back onto its ED byte and takes 21 T-states, the last iteration takes 16.

use crate::alu::{parity, sz53, sz53p};
use crate::{Bus, Cpu, Idx, flag};

const XY: u8 = flag::X | flag::Y;

/// Runs one ED-table instruction. On entry `pc` is at the opcode byte after `ED` and R
/// already counts the ED byte; fetches the opcode itself through `Cpu::fetch_m1` (R counts it
/// too). Returns the T-states from the ED byte on (8 for the shortest instructions).
pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus) -> u32 {
    let op = cpu.fetch_m1(bus);
    match op {
        0x40..=0x7F => quarter1(cpu, bus, op),
        0xA0..=0xBF if op & 0x04 == 0 => block(cpu, bus, op),
        _ => 8,
    }
}

/// Opcodes ED 40-7F.
fn quarter1(cpu: &mut Cpu, bus: &mut impl Bus, op: u8) -> u32 {
    let y = (op >> 3) & 7;
    let p = (op >> 4) & 3;
    match op & 7 {
        // IN r,(C); y = 6 only sets the flags.
        0 => {
            let bc = cpu.bc();
            let v = bus.port_in(bc);
            cpu.wz = bc.wrapping_add(1);
            if y != 6 {
                cpu.set_reg(Idx::Hl, y, v);
            }
            cpu.set_f(sz53p(v) | (cpu.f & flag::C));
            12
        }
        // OUT (C),r; y = 6 puts 0 on the bus.
        1 => {
            let bc = cpu.bc();
            let v = if y == 6 { 0 } else { cpu.reg(Idx::Hl, y) };
            bus.port_out(bc, v);
            cpu.wz = bc.wrapping_add(1);
            12
        }
        2 => {
            let v = cpu.rp(Idx::Hl, p);
            if op & 0x08 == 0 { cpu.sbc16(v) } else { cpu.adc16(v) }
            15
        }
        // LD (nn),rr / LD rr,(nn)
        3 => {
            let nn = cpu.fetch16(bus);
            if op & 0x08 == 0 {
                Cpu::write16(bus, nn, cpu.rp(Idx::Hl, p));
            } else {
                let v = Cpu::read16(bus, nn);
                cpu.set_rp(Idx::Hl, p, v);
            }
            cpu.wz = nn.wrapping_add(1);
            20
        }
        4 => {
            cpu.neg();
            8
        }
        // RETN / RETI: both copy IFF2 into IFF1.
        5 => {
            cpu.pc = cpu.pop16(bus);
            cpu.wz = cpu.pc;
            cpu.iff1 = cpu.iff2;
            14
        }
        6 => {
            cpu.im = [0, 0, 1, 2, 0, 0, 1, 2][y as usize];
            8
        }
        _ => match y {
            0 => {
                cpu.i = cpu.a;
                9
            }
            1 => {
                cpu.r = cpu.a;
                9
            }
            2 | 3 => {
                cpu.a = if y == 2 { cpu.i } else { cpu.r };
                let pv = if cpu.iff2 { flag::PV } else { 0 };
                cpu.set_f(sz53(cpu.a) | (cpu.f & flag::C) | pv);
                cpu.p = true;
                9
            }
            // RRD / RLD: a 12-bit nibble rotate through A's low nibble and (HL).
            4 | 5 => {
                let hl = cpu.hl();
                let m = bus.read(hl);
                let (a, m) = if y == 4 {
                    ((cpu.a & 0xF0) | (m & 0x0F), (cpu.a << 4) | (m >> 4))
                } else {
                    ((cpu.a & 0xF0) | (m >> 4), (m << 4) | (cpu.a & 0x0F))
                };
                bus.write(hl, m);
                cpu.a = a;
                cpu.wz = hl.wrapping_add(1);
                cpu.set_f(sz53p(a) | (cpu.f & flag::C));
                18
            }
            _ => 8,
        },
    }
}

/// The block instructions (ED A0-A3, A8-AB, B0-B3, B8-BB): bit 3 set counts down, bit 4 set repeats.
fn block(cpu: &mut Cpu, bus: &mut impl Bus, op: u8) -> u32 {
    let step: u16 = if op & 0x08 == 0 { 1 } else { 0xFFFF };
    let hl = cpu.hl();
    // The I/O forms: (value moved, its carry sum k) for the extra adjustment when they repeat.
    let mut io = None;
    let again = match op & 3 {
        // LDI / LDD
        0 => {
            let v = bus.read(hl);
            let de = cpu.de();
            bus.write(de, v);
            cpu.set_hl(hl.wrapping_add(step));
            cpu.set_de(de.wrapping_add(step));
            let bc = cpu.bc().wrapping_sub(1);
            cpu.set_bc(bc);
            let n = v.wrapping_add(cpu.a);
            let mut f = (cpu.f & (flag::S | flag::Z | flag::C)) | (n & flag::X) | ((n << 4) & flag::Y);
            if bc != 0 {
                f |= flag::PV;
            }
            cpu.set_f(f);
            bc != 0
        }
        // CPI / CPD
        1 => {
            let v = bus.read(hl);
            let r = cpu.a.wrapping_sub(v);
            let half = (cpu.a ^ v ^ r) & flag::H;
            cpu.set_hl(hl.wrapping_add(step));
            let bc = cpu.bc().wrapping_sub(1);
            cpu.set_bc(bc);
            let n = r.wrapping_sub(half >> 4);
            let mut f = (cpu.f & flag::C) | (sz53(r) & !XY) | half | flag::N | (n & flag::X) | ((n << 4) & flag::Y);
            if bc != 0 {
                f |= flag::PV;
            }
            cpu.wz = cpu.wz.wrapping_add(step);
            cpu.set_f(f);
            bc != 0 && r != 0
        }
        // INI / IND
        2 => {
            let bc = cpu.bc();
            cpu.wz = bc.wrapping_add(step);
            let v = bus.port_in(bc);
            bus.write(hl, v);
            cpu.b = cpu.b.wrapping_sub(1);
            cpu.set_hl(hl.wrapping_add(step));
            let k = v as u16 + cpu.c.wrapping_add(step as u8) as u16;
            io = Some((v, k));
            cpu.b != 0
        }
        // OUTI / OUTD
        _ => {
            let v = bus.read(hl);
            cpu.b = cpu.b.wrapping_sub(1);
            bus.port_out(cpu.bc(), v);
            cpu.set_hl(hl.wrapping_add(step));
            cpu.wz = cpu.bc().wrapping_add(step);
            let k = v as u16 + cpu.l as u16;
            io = Some((v, k));
            cpu.b != 0
        }
    };
    if let Some((v, k)) = io {
        let mut f = sz53(cpu.b);
        if v & 0x80 != 0 {
            f |= flag::N;
        }
        if k > 0xFF {
            f |= flag::H | flag::C;
        }
        if parity((k as u8 & 7) ^ cpu.b) {
            f |= flag::PV;
        }
        cpu.set_f(f);
    }
    if op & 0x10 == 0 || !again {
        return 16;
    }
    // Repeating: back onto the ED byte; X/Y come from PC's high byte.
    cpu.pc = cpu.pc.wrapping_sub(2);
    cpu.wz = cpu.pc.wrapping_add(1);
    let mut f = (cpu.f & !XY) | ((cpu.pc >> 8) as u8 & XY);
    if let Some((v, _)) = io {
        // INxR / OTxR: P/V and H are adjusted once more by the pending B decrement.
        let b = cpu.b;
        let t = if f & flag::C != 0 {
            f &= !flag::H;
            if v & 0x80 != 0 {
                if b & 0x0F == 0 {
                    f |= flag::H;
                }
                b.wrapping_sub(1)
            } else {
                if b & 0x0F == 0x0F {
                    f |= flag::H;
                }
                b.wrapping_add(1)
            }
        } else {
            b
        };
        if !parity(t & 7) {
            f ^= flag::PV;
        }
    }
    cpu.set_f(f);
    21
}
