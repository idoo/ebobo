//! Lines 8 and C: OR, DIVU, DIVS and SBCD; AND, MULU, MULS, ABCD and EXG.

use crate::core::{Ea, Fault, Res, Size, mode, reg, reg9};
use crate::{Bus, Cpu, sr};

/// Cycles `cpu.exception` spends on the bus: three words pushed, two read.
const EXCEPTION_BUS: u32 = 20;

/// OR (line 8) or AND (line C) of two operands.
fn combine(and: bool, a: u32, b: u32) -> u32 {
    if and { a & b } else { a | b }
}

/// The word source operand of MULU, MULS, DIVU and DIVS: any data addressing mode.
fn word_source(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<u32> {
    if mode(op) == 1 {
        return Err(Fault::Illegal);
    }
    let ea = cpu.ea(bus, mode(op), reg(op), Size::Word)?;
    cpu.load(bus, ea, Size::Word)
}

/// MULU and MULS <ea>,Dn: 16 x 16 to 32. N and Z from the long result, V and C cleared. 38 cycles plus 2 for every
/// one-bit of the source (MULU), or for every 01 or 10 pair of the source with a zero appended below it (MULS).
fn multiply(cpu: &mut Cpu, bus: &mut impl Bus, op: u16, signed: bool) -> Res<()> {
    let src = word_source(cpu, bus, op)?;
    let n = reg9(op);
    let dst = cpu.d(n, Size::Word);
    let (res, steps) = if signed {
        ((src as i16 as i32).wrapping_mul(dst as i16 as i32) as u32, ((src ^ (src << 1)) & 0xFFFF).count_ones())
    } else {
        (src * dst, src.count_ones())
    };
    cpu.idle(34 + 2 * steps);
    cpu.set_d(n, Size::Long, res);
    cpu.nz(res, Size::Long);
    Ok(())
}

/// Division by zero: vector 5, 38 cycles plus the operand's. N, Z, V and C cleared before SR is stacked.
fn divide_by_zero(cpu: &mut Cpu, bus: &mut impl Bus) -> Res<()> {
    cpu.set_flag(sr::N | sr::Z | sr::V | sr::C, false);
    cpu.exception(bus, 5)?;
    cpu.idle(38 - 4 - EXCEPTION_BUS);
    Ok(())
}

/// DIVU <ea>,Dn: Dn (32 bits) / source (16 bits), remainder in the high word, quotient in the low word. A quotient
/// that does not fit leaves Dn, sets V (N and Z kept) and takes 10 cycles. Otherwise the time is that of the chip's
/// restoring division: 76 cycles, plus 4 for every quotient bit of the first 15 that is 0, or 2 when it is 1 and
/// no bit was shifted out of the dividend.
fn divu(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let divisor = word_source(cpu, bus, op)?;
    let n = reg9(op);
    let dividend = cpu.d(n, Size::Long);
    if divisor == 0 {
        return divide_by_zero(cpu, bus);
    }
    cpu.set_flag(sr::C, false);
    if dividend >> 16 >= divisor {
        cpu.set_flag(sr::V, true);
        cpu.idle(10 - 4);
        return Ok(());
    }
    let hdivisor = divisor << 16;
    let mut work = dividend;
    let mut units = 38u32;
    for _ in 0..15 {
        let carry = work & 0x8000_0000 != 0;
        work <<= 1;
        if carry {
            work = work.wrapping_sub(hdivisor);
        } else {
            units += 2;
            if work >= hdivisor {
                work -= hdivisor;
                units -= 1;
            }
        }
    }
    cpu.idle(units * 2 - 4);
    let quot = dividend / divisor;
    let rem = dividend % divisor;
    cpu.set_d(n, Size::Long, rem << 16 | quot);
    cpu.nz(quot, Size::Word);
    Ok(())
}

/// DIVS <ea>,Dn: signed, the remainder has the sign of the dividend. A quotient that does not fit a signed word leaves
/// Dn, sets V (N and Z kept) and takes 16 cycles, 18 for a negative dividend. The time of a whole division:
/// 122 cycles, 2 more for a negative dividend, 2 less for a positive divisor with a positive dividend, 2 more for a
/// positive divisor with a negative dividend, and 2 for every zero in the top 15 bits of the absolute quotient.
fn divs(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let divisor = word_source(cpu, bus, op)? as i16 as i32;
    let n = reg9(op);
    let dividend = cpu.d(n, Size::Long) as i32;
    if divisor == 0 {
        return divide_by_zero(cpu, bus);
    }
    cpu.set_flag(sr::C, false);
    let mut units = 6u32;
    if dividend < 0 {
        units += 1;
    }
    let aquot = dividend.unsigned_abs() / divisor.unsigned_abs();
    let negative = (dividend < 0) != (divisor < 0);
    if aquot > 0x7FFF && !(negative && aquot == 0x8000) {
        cpu.set_flag(sr::V, true);
        cpu.idle((units + 2) * 2 - 4);
        return Ok(());
    }
    units += 55;
    if divisor >= 0 {
        if dividend >= 0 { units -= 1 } else { units += 1 }
    }
    units += (!aquot & 0xFFFE).count_ones();
    cpu.idle(units * 2 - 4);
    let quot = dividend / divisor;
    let rem = dividend % divisor;
    cpu.set_d(n, Size::Long, (rem as u32 & 0xFFFF) << 16 | (quot as u32 & 0xFFFF));
    cpu.nz(quot as u32, Size::Word);
    Ok(())
}

/// ABCD (`sub` false) and SBCD (`sub` true), Dy,Dx (6 cycles) and -(Ay),-(Ax) (18 cycles): destination plus or
/// minus source and X, as two decimal digits. The binary result is corrected by 6 per digit that carried or (ABCD)
/// went past 9, which is also what the chip does to digits that are not decimal. C and X: the decimal carry or
/// borrow; Z only cleared, by a result that is not zero; N: bit 7 of the result; V: bit 7 changed by the correction
/// (0 to 1 for ABCD, 1 to 0 for SBCD).
fn bcd(cpu: &mut Cpu, bus: &mut impl Bus, op: u16, sub: bool) -> Res<()> {
    let (src_ea, dst_ea) = if mode(op) == 0 {
        cpu.idle(2);
        (Ea::Dn(reg(op)), Ea::Dn(reg9(op)))
    } else {
        let src_ea = cpu.ea(bus, 4, reg(op), Size::Byte)?;
        let src = cpu.load(bus, src_ea, Size::Byte)?;
        let dst_ea = cpu.ea_plain(bus, 4, reg9(op), Size::Byte)?;
        (Ea::Imm(src), dst_ea)
    };
    let xx = cpu.load(bus, src_ea, Size::Byte)?;
    let yy = cpu.load(bus, dst_ea, Size::Byte)?;
    let x = cpu.flag(sr::X) as u32;
    let (rr, carry, overflow) = if sub {
        let dd = yy.wrapping_sub(xx).wrapping_sub(x);
        let bc = ((!yy & xx) | (dd & !yy) | (dd & xx)) & 0x88;
        let rr = dd.wrapping_sub(bc - (bc >> 2));
        (rr, (bc | (!dd & rr)) & 0x80 != 0, dd & !rr & 0x80 != 0)
    } else {
        let ss = xx + yy + x;
        let bc = ((xx & yy) | (!ss & xx) | (!ss & yy)) & 0x88;
        let dc = (((ss + 0x66) ^ ss) & 0x110) >> 1;
        let rr = ss + ((bc | dc) - ((bc | dc) >> 2));
        (rr, (bc | (ss & !rr)) & 0x80 != 0, !ss & rr & 0x80 != 0)
    };
    let rr = rr & 0xFF;
    cpu.set_flag(sr::C | sr::X, carry);
    cpu.set_flag(sr::V, overflow);
    cpu.set_flag(sr::N, rr & 0x80 != 0);
    if rr != 0 {
        cpu.set_flag(sr::Z, false);
    }
    cpu.store(bus, dst_ea, Size::Byte, rr)
}

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let and = op & 0x4000 != 0;
    let (m, r, n) = (mode(op), reg(op), reg9(op));
    let opmode = (op >> 6) & 7;
    match opmode {
        // OR/AND <ea>,Dn: 4 cycles (byte, word), 6 (long from memory) or 8 (long from a register or an immediate),
        // plus the operand's.
        0..=2 => {
            if m == 1 {
                return Err(Fault::Illegal);
            }
            let size = Size::of(op).ok_or(Fault::Illegal)?;
            let ea = cpu.ea(bus, m, r, size)?;
            let src = cpu.load(bus, ea, size)?;
            let res = combine(and, src, cpu.d(n, size));
            cpu.nz(res, size);
            cpu.set_d(n, size, res);
            if size == Size::Long {
                cpu.idle(if matches!(ea, Ea::Mem(_)) { 2 } else { 4 });
            }
            Ok(())
        }
        3 if and => multiply(cpu, bus, op, false),
        7 if and => multiply(cpu, bus, op, true),
        3 => divu(cpu, bus, op),
        7 => divs(cpu, bus, op),
        4 if m < 2 => bcd(cpu, bus, op, !and),
        // EXG Dx,Dy; EXG Ax,Ay; EXG Dx,Ay: 6 cycles.
        5 if and && m == 0 => {
            let (a, b) = (cpu.d(n, Size::Long), cpu.d(r, Size::Long));
            cpu.set_d(n, Size::Long, b);
            cpu.set_d(r, Size::Long, a);
            cpu.idle(2);
            Ok(())
        }
        5 if and && m == 1 => {
            let (a, b) = (cpu.a(n), cpu.a(r));
            cpu.set_a(n, b);
            cpu.set_a(r, a);
            cpu.idle(2);
            Ok(())
        }
        6 if and && m == 1 => {
            let (a, b) = (cpu.d(n, Size::Long), cpu.a(r));
            cpu.set_d(n, Size::Long, b);
            cpu.set_a(r, a);
            cpu.idle(2);
            Ok(())
        }
        // OR/AND Dn,<ea>: memory alterable modes only. 8 cycles (byte, word) or 12 (long) plus the operand's.
        _ => {
            if m < 2 || (m == 7 && r > 1) {
                return Err(Fault::Illegal);
            }
            let size = Size::of(op & 0xC0).ok_or(Fault::Illegal)?;
            let ea = cpu.ea(bus, m, r, size)?;
            let dst = cpu.load(bus, ea, size)?;
            let res = combine(and, cpu.d(n, size), dst);
            cpu.nz(res, size);
            cpu.store(bus, ea, size, res)
        }
    }
}
