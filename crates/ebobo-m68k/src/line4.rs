//! Line 4 without its 0x4E00-0x4EFF block: NEGX, CLR, NEG, NOT, TST, TAS, NBCD, EXT, SWAP, PEA, LEA, CHK, MOVEM,
//! MOVE from SR, MOVE to CCR, MOVE to SR and ILLEGAL. The 0x4Exx block (TRAP, LINK, UNLK, MOVE USP, RESET, NOP,
//! STOP, RTE, RTS, TRAPV, RTR, JSR, JMP) is `sys.rs`.

use crate::core::{Fault, Res, Size, mode, reg, reg9};
use crate::{Bus, Cpu, sr, sys};

/// Whether the mode is data alterable: everything but An, the PC-relative modes and immediate.
fn data_alterable(m: u8, r: u8) -> bool {
    m != 1 && (m != 7 || r <= 1)
}

/// Whether the mode is a data mode: everything but An.
fn data(m: u8, r: u8) -> bool {
    m != 1 && (m != 7 || r <= 4)
}

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    if op & 0xFF00 == 0x4E00 {
        return sys::exec(cpu, bus, op);
    }
    // ILLEGAL: the one opcode that is promised to stay illegal.
    if op == 0x4AFC {
        return Err(Fault::Illegal);
    }
    let (m, r) = (mode(op), reg(op));
    if op & 0x0100 != 0 {
        return match op & 0x00C0 {
            0x0080 => chk(cpu, bus, op),
            0x00C0 => {
                // LEA: 4 + the extension words; the indexed modes take 12.
                let addr = cpu.ea_addr(bus, m, r)?;
                if m == 6 || (m == 7 && r == 3) {
                    cpu.idle(2);
                }
                cpu.set_a(reg9(op), addr);
                Ok(())
            }
            _ => Err(Fault::Illegal),
        };
    }
    let group = (op >> 9) & 7;
    match (group, Size::of(op)) {
        (0..=3, Some(size)) => unary(cpu, bus, group, size, m, r),
        // MOVE from SR: reads the operand, then writes SR; 6 on a register, 8 + ea in memory.
        (0, None) => {
            if !data_alterable(m, r) {
                return Err(Fault::Illegal);
            }
            let ea = cpu.ea(bus, m, r, Size::Word)?;
            cpu.load(bus, ea, Size::Word)?;
            if m == 0 {
                cpu.idle(2);
            }
            cpu.store(bus, ea, Size::Word, u32::from(cpu.sr))
        }
        // MOVE to CCR (2) and MOVE to SR (3): 12 + ea.
        (2 | 3, None) => {
            if !data(m, r) {
                return Err(Fault::Illegal);
            }
            if group == 3 && !cpu.flag(sr::S) {
                return Err(Fault::Privilege);
            }
            let ea = cpu.ea(bus, m, r, Size::Word)?;
            let v = cpu.load(bus, ea, Size::Word)? as u16;
            cpu.idle(8);
            if group == 3 {
                cpu.set_sr(v);
            } else {
                cpu.set_ccr(v);
            }
            Ok(())
        }
        (4, Some(Size::Byte)) => nbcd(cpu, bus, m, r),
        (4, Some(Size::Word)) if m == 0 => {
            // SWAP
            let v = cpu.d(r, Size::Long).rotate_left(16);
            cpu.set_d(r, Size::Long, v);
            cpu.nz(v, Size::Long);
            Ok(())
        }
        (4, Some(Size::Word)) => {
            // PEA: 12 for (An), 4 more for each extension word, 20 for the indexed modes.
            let addr = cpu.ea_addr(bus, m, r)?;
            if m == 6 || (m == 7 && r == 3) {
                cpu.idle(2);
            }
            cpu.push32(bus, addr)
        }
        (4, Some(Size::Long) | None) if m == 0 => {
            // EXT: byte to word (0x4880), word to long (0x48C0).
            if op & 0x0040 == 0 {
                let v = Size::Byte.sext(cpu.d(r, Size::Byte));
                cpu.set_d(r, Size::Word, v);
                cpu.nz(v, Size::Word);
            } else {
                let v = Size::Word.sext(cpu.d(r, Size::Word));
                cpu.set_d(r, Size::Long, v);
                cpu.nz(v, Size::Long);
            }
            Ok(())
        }
        (4, Some(Size::Long) | None) => movem_to_mem(cpu, bus, op),
        (5, Some(size)) => {
            // TST: 4 + ea.
            if !data_alterable(m, r) {
                return Err(Fault::Illegal);
            }
            let ea = cpu.ea(bus, m, r, size)?;
            let v = cpu.load(bus, ea, size)?;
            cpu.nz(v, size);
            Ok(())
        }
        (5, None) => {
            // TAS: 4 on a register, 10 + ea in memory (one read-modify-write cycle).
            if !data_alterable(m, r) {
                return Err(Fault::Illegal);
            }
            let ea = cpu.ea(bus, m, r, Size::Byte)?;
            let v = cpu.load(bus, ea, Size::Byte)?;
            cpu.nz(v, Size::Byte);
            if m == 0 {
                cpu.store(bus, ea, Size::Byte, v | 0x80)
            } else {
                cpu.idle(2);
                if cpu.tas_no_write {
                    cpu.idle(4);
                    Ok(())
                } else {
                    cpu.store(bus, ea, Size::Byte, v | 0x80)
                }
            }
        }
        (6, Some(Size::Long) | None) => movem_to_reg(cpu, bus, op),
        _ => Err(Fault::Illegal),
    }
}

/// NEGX (0), CLR (1), NEG (2), NOT (3): the operand is read, then written. 4 on a register (6 for a long), in memory
/// nothing but the bus accesses.
fn unary(cpu: &mut Cpu, bus: &mut impl Bus, group: u16, size: Size, m: u8, r: u8) -> Res<()> {
    if !data_alterable(m, r) {
        return Err(Fault::Illegal);
    }
    let ea = cpu.ea(bus, m, r, size)?;
    let d = cpu.load(bus, ea, size)?;
    let res = match group {
        0 => {
            let res = 0u32.wrapping_sub(d).wrapping_sub(u32::from(cpu.flag(sr::X))) & size.mask();
            let borrow = (d | res) & size.msb() != 0;
            cpu.set_flag(sr::X | sr::C, borrow);
            cpu.set_flag(sr::V, d & res & size.msb() != 0);
            cpu.set_flag(sr::N, res & size.msb() != 0);
            if res != 0 {
                cpu.set_flag(sr::Z, false);
            }
            res
        }
        1 => {
            cpu.nz(0, size);
            0
        }
        2 => {
            let res = 0u32.wrapping_sub(d) & size.mask();
            cpu.sub_flags(d, 0, res, size);
            res
        }
        _ => {
            let res = !d & size.mask();
            cpu.nz(res, size);
            res
        }
    };
    if m == 0 && size == Size::Long {
        cpu.idle(2);
    }
    cpu.store(bus, ea, size, res)
}

/// NBCD: 0 - operand - X in decimal, as the chip's adder does it: a binary subtraction, then 6 taken from every digit
/// that borrowed. 6 on a register, 8 + ea in memory.
fn nbcd(cpu: &mut Cpu, bus: &mut impl Bus, m: u8, r: u8) -> Res<()> {
    if !data_alterable(m, r) {
        return Err(Fault::Illegal);
    }
    let ea = cpu.ea(bus, m, r, Size::Byte)?;
    let s = cpu.load(bus, ea, Size::Byte)?;
    let x = u32::from(cpu.flag(sr::X));
    let dd = 0u32.wrapping_sub(s).wrapping_sub(x);
    // The borrows out of bit 3 and bit 7 of 0 - s - x.
    let bc = (s | dd) & 0x88;
    let corf = bc - (bc >> 2);
    let rr = dd.wrapping_sub(corf);
    let carry = (bc | (!dd & rr)) & 0x80 != 0;
    cpu.set_flag(sr::X | sr::C, carry);
    cpu.set_flag(sr::V, dd & !rr & 0x80 != 0);
    cpu.set_flag(sr::N, rr & 0x80 != 0);
    if rr & 0xFF != 0 {
        cpu.set_flag(sr::Z, false);
    }
    if m == 0 {
        cpu.idle(2);
    }
    cpu.store(bus, ea, Size::Byte, rr & 0xFF)
}

/// CHK <ea>,Dn: the low word of Dn against 0 and the word operand; outside, exception 6. 10 + ea when inside. V and C
/// cleared, Z kept, N set when it is below 0 (and, above the bound, from its sign).
fn chk(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let (m, r) = (mode(op), reg(op));
    if !data(m, r) {
        return Err(Fault::Illegal);
    }
    let ea = cpu.ea(bus, m, r, Size::Word)?;
    let bound = cpu.load(bus, ea, Size::Word)? as u16 as i16;
    let v = cpu.d(reg9(op), Size::Word) as u16 as i16;
    cpu.set_flag(sr::N | sr::V | sr::C, false);
    if v > bound {
        cpu.idle(14);
        cpu.set_flag(sr::N, v < 0);
        cpu.exception(bus, 6)
    } else if v < 0 {
        cpu.idle(16);
        cpu.set_flag(sr::N, true);
        cpu.exception(bus, 6)
    } else {
        cpu.idle(6);
        Ok(())
    }
}

/// MOVEM registers to memory. Control modes: D0 first, addresses going up. -(An): A7 first (bit 0 of the list),
/// addresses going down; An in the list stores its value from before the instruction, An is left at the last address.
/// 8 + 4 (word) or 8 (long) for each register, plus the extension.
fn movem_to_mem(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let (m, r) = (mode(op), reg(op));
    let long = op & 0x0040 != 0;
    if !matches!((m, r), (2 | 4 | 5 | 6, _) | (7, 0 | 1)) {
        return Err(Fault::Illegal);
    }
    let list = cpu.fetch16(bus)?;
    if m == 4 {
        let mut addr = cpu.a(r);
        for i in 0..16u8 {
            if list & (1 << i) == 0 {
                continue;
            }
            let n = 15 - i;
            let v = if n < 8 { cpu.d(n, Size::Long) } else { cpu.a(n - 8) };
            addr = addr.wrapping_sub(2);
            cpu.write(bus, addr, Size::Word, v & 0xFFFF)?;
            if long {
                addr = addr.wrapping_sub(2);
                cpu.write(bus, addr, Size::Word, v >> 16)?;
            }
        }
        cpu.set_a(r, addr);
        return Ok(());
    }
    let mut addr = cpu.ea_addr(bus, m, r)?;
    let size = if long { Size::Long } else { Size::Word };
    for n in 0..16u8 {
        if list & (1 << n) == 0 {
            continue;
        }
        let v = if n < 8 { cpu.d(n, Size::Long) } else { cpu.a(n - 8) };
        cpu.write(bus, addr, size, v)?;
        addr = addr.wrapping_add(size.bytes());
    }
    Ok(())
}

/// MOVEM memory to registers: D0 first, addresses going up, words sign-extended into the whole register, then one more
/// word read past the last one. (An)+ leaves An past the last one. 12 + 4 (word) or 8 (long) for each register, plus
/// the extension.
fn movem_to_reg(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let (m, r) = (mode(op), reg(op));
    let size = if op & 0x0040 != 0 { Size::Long } else { Size::Word };
    if !matches!((m, r), (2 | 3 | 5 | 6, _) | (7, 0..=3)) {
        return Err(Fault::Illegal);
    }
    let list = cpu.fetch16(bus)?;
    let mut addr = if m == 3 { cpu.a(r) } else { cpu.ea_addr(bus, m, r)? };
    for n in 0..16u8 {
        if list & (1 << n) == 0 {
            continue;
        }
        let v = size.sext(cpu.read(bus, addr, size)?);
        if n < 8 {
            cpu.set_d(n, Size::Long, v);
        } else {
            cpu.set_a(n - 8, v);
        }
        addr = addr.wrapping_add(size.bytes());
    }
    cpu.read(bus, addr, Size::Word)?;
    if m == 3 {
        cpu.set_a(r, addr);
    }
    Ok(())
}
