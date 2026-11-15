//! Line E: ASL, ASR, LSL, LSR, ROL, ROR, ROXL and ROXR, on a data register or on a word in memory.

use crate::core::{Fault, Res, Size, mode, reg, reg9};
use crate::{Bus, Cpu, sr};

/// The kind field of a line E opcode: 0 arithmetic shift, 1 logical shift, 2 rotate through X, 3 rotate.
const AS: u16 = 0;
const LS: u16 = 1;
const ROX: u16 = 2;

/// Shifts or rotates `v` (cut to `size`) by `n` places (0-63) and sets the flags; returns the result cut to `size`.
///
/// N and Z come from the result. C is the last bit shifted out, cleared for `n` = 0 except by ROXL/ROXR, where it
/// takes X. X follows C when `n` > 0, except for ROL/ROR, which never change it. V is set only by ASL, when the sign
/// bit changed at any step.
fn shift(cpu: &mut Cpu, kind: u16, left: bool, size: Size, v: u32, n: u32) -> u32 {
    let bits = size.bits();
    let mask = u64::from(size.mask());
    let v = v as u64 & mask;
    let mut overflow = false;
    // `carry`: the bit that goes to C; `None` leaves C clear and X alone.
    let (res, carry) = match kind {
        AS | LS if n == 0 => (v, None),
        AS | LS if left => {
            if kind == AS {
                overflow = if n >= bits {
                    v != 0
                } else {
                    // The sign changes at some step unless the top n + 1 bits are all alike.
                    let top = mask & !(mask >> (n + 1));
                    v & top != 0 && v & top != top
                };
            }
            if n > bits {
                (0, Some(false))
            } else {
                let wide = v << n;
                (wide & mask, Some(wide >> bits & 1 != 0))
            }
        }
        AS => {
            // The sign bit is shifted in at every step, so past the operand's width the result is all sign bits and
            // the last bit out, which C and X take, is the sign bit too.
            let s = i64::from(size.sext(v as u32) as i32);
            ((s >> n) as u64 & mask, Some(s >> (n - 1) & 1 != 0))
        }
        LS => (v >> n, Some(v >> (n - 1) & 1 != 0)),
        ROX => {
            // A rotation of bits + 1 places, X above the operand.
            let width = bits + 1;
            let r = n % width;
            let wide = v | (cpu.flag(sr::X) as u64) << bits;
            let rot = if left { wide << r | wide >> (width - r) } else { wide >> r | wide << (width - r) };
            (rot & mask, Some(rot >> bits & 1 != 0))
        }
        _ => {
            let r = n % bits;
            let res = (if left { v << r | v >> (bits - r) } else { v >> r | v << (bits - r) }) & mask;
            let out = if left { res & 1 != 0 } else { res >> (bits - 1) != 0 };
            (res, if n == 0 { None } else { Some(out) })
        }
    };
    cpu.set_flag(sr::N, res >> (bits - 1) != 0);
    cpu.set_flag(sr::Z, res == 0);
    cpu.set_flag(sr::V, overflow);
    cpu.set_flag(sr::C, carry == Some(true));
    if let Some(c) = carry
        && kind <= ROX
    {
        cpu.set_flag(sr::X, c);
    }
    res as u32
}

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let left = op & 0x0100 != 0;
    let Some(size) = Size::of(op) else {
        // A word in memory, by one place: 8 cycles plus the operand's, all of them bus accesses.
        let (m, r) = (mode(op), reg(op));
        if op & 0x0800 != 0 || !matches!((m, r), (2..=6, _) | (7, 0 | 1)) {
            return Err(Fault::Illegal);
        }
        let ea = cpu.ea(bus, m, r, Size::Word)?;
        let v = cpu.load(bus, ea, Size::Word)?;
        let res = shift(cpu, op >> 9 & 3, left, Size::Word, v, 1);
        return cpu.store(bus, ea, Size::Word, res);
    };
    // Dn by an immediate count (1-8) or by the low six bits of another register: 6 + 2n cycles, a long 8 + 2n.
    let n = if op & 0x0020 != 0 {
        cpu.d(reg9(op), Size::Long) & 63
    } else {
        ((reg9(op) as u32).wrapping_sub(1) & 7) + 1
    };
    let r = reg(op);
    let v = cpu.d(r, size);
    let res = shift(cpu, op >> 3 & 3, left, size, v, n);
    cpu.set_d(r, size, res);
    cpu.idle(if matches!(size, Size::Long) { 4 } else { 2 } + 2 * n);
    Ok(())
}
