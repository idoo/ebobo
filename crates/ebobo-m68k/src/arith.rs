//! Lines 9, B and D: SUB, SUBA and SUBX; CMP, CMPA, CMPM and EOR; ADD, ADDA and ADDX.

use crate::core::{Ea, Fault, Res, Size, mode, reg, reg9};
use crate::{Bus, Cpu, sr};

/// The result of ADDX (`dst + src + X`) or SUBX (`dst - src - X`), cut to `size`, with the flags set: X and C the
/// carry or borrow, V the signed overflow, N the sign, Z cleared by a non-zero result and otherwise kept.
fn extend(cpu: &mut Cpu, src: u32, dst: u32, size: Size, sub: bool) -> u32 {
    let x = cpu.flag(sr::X) as u32;
    let msb = size.msb();
    let res = if sub { dst.wrapping_sub(src).wrapping_sub(x) } else { dst.wrapping_add(src).wrapping_add(x) } & size.mask();
    let (carry, over) = if sub {
        ((src & !dst) | (res & !dst) | (src & res), (src ^ dst) & (res ^ dst))
    } else {
        ((src & dst) | (!res & (src | dst)), (src ^ res) & (dst ^ res))
    };
    cpu.set_flag(sr::X | sr::C, carry & msb != 0);
    cpu.set_flag(sr::V, over & msb != 0);
    cpu.set_flag(sr::N, res & msb != 0);
    if res != 0 {
        cpu.set_flag(sr::Z, false);
    }
    res
}

/// The long at -(An) as ADDX.l and SUBX.l read it: low word first, at An - 2, then the high word at An - 4. An odd
/// address faults on the low word with An not stepped yet and the frame's PC 2 past `pc`.
fn predec_long(cpu: &mut Cpu, bus: &mut impl Bus, n: u8) -> Res<u32> {
    let addr = cpu.a(n).wrapping_sub(4);
    cpu.ae_pc = 2;
    let lo = cpu.read(bus, addr.wrapping_add(2), Size::Word)?;
    let hi = cpu.read(bus, addr, Size::Word)?;
    cpu.set_a(n, addr);
    Ok(hi << 16 | lo)
}

/// One operand of CMPM, (An)+. An odd address faults with the frame's PC 2 past `pc`; An is then a word further for
/// the source (`first`, whatever the size: a long has stepped half its way) and not stepped for the destination.
fn postinc(cpu: &mut Cpu, bus: &mut impl Bus, n: u8, size: Size, first: bool) -> Res<u32> {
    let addr = cpu.a(n);
    let ea = cpu.ea(bus, 3, n, size)?;
    cpu.ae_pc = 2;
    cpu.ae_restore = Some((n, if first { addr.wrapping_add(2) } else { addr }));
    cpu.load(bus, ea, size)
}

/// What a long operation into a register idles: 2 cycles after a memory source (6 + ea in the manual), 4 after a
/// register or an immediate (8).
fn long_idle(ea: Ea) -> u32 {
    if matches!(ea, Ea::Mem(_)) { 2 } else { 4 }
}

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let line = op >> 12;
    let sub = line == 0x9;
    let cmp = line == 0xB;
    let (m, r, rn) = (mode(op), reg(op), reg9(op));
    let opmode = (op >> 6) & 7;
    match opmode {
        // ADDA, SUBA, CMPA <ea>,An: a word source is sign-extended, the whole register takes part. ADDA and SUBA
        // leave the flags alone and cost 8 + ea (long from memory: 6 + ea); CMPA costs 6 + ea.
        3 | 7 => {
            let size = if opmode == 3 { Size::Word } else { Size::Long };
            let ea = cpu.ea(bus, m, r, size)?;
            let src = size.sext(cpu.load(bus, ea, size)?);
            let dst = cpu.a(rn);
            if cmp {
                cpu.cmp_flags(src, dst, dst.wrapping_sub(src), Size::Long);
                cpu.idle(2);
            } else {
                cpu.idle(if opmode == 3 { 4 } else { long_idle(ea) });
                cpu.set_a(rn, if sub { dst.wrapping_sub(src) } else { dst.wrapping_add(src) });
            }
            Ok(())
        }
        // ADD, SUB, CMP <ea>,Dn: 4 + ea; long 6 + ea (CMP, and ADD/SUB from memory) or 8 (ADD/SUB from a register or
        // an immediate).
        0..=2 => {
            let size = Size::of(op).ok_or(Fault::Illegal)?;
            let long = opmode == 2;
            if m == 1 && opmode == 0 {
                return Err(Fault::Illegal);
            }
            let ea = cpu.ea(bus, m, r, size)?;
            let src = cpu.load(bus, ea, size)?;
            let dst = cpu.d(rn, size);
            if cmp {
                cpu.cmp_flags(src, dst, dst.wrapping_sub(src) & size.mask(), size);
                if long {
                    cpu.idle(2);
                }
                return Ok(());
            }
            let res = if sub { dst.wrapping_sub(src) } else { dst.wrapping_add(src) } & size.mask();
            if sub {
                cpu.sub_flags(src, dst, res, size);
            } else {
                cpu.add_flags(src, dst, res, size);
            }
            cpu.set_d(rn, size, res);
            if long {
                cpu.idle(long_idle(ea));
            }
            Ok(())
        }
        _ => {
            let size = Size::of(op).ok_or(Fault::Illegal)?;
            let long = opmode == 6;
            match (m, cmp) {
                // EOR Dn,Dm: 4, long 8.
                (0, true) => {
                    let res = cpu.d(r, size) ^ cpu.d(rn, size);
                    cpu.nz(res, size);
                    cpu.set_d(r, size, res);
                    if long {
                        cpu.idle(4);
                    }
                    Ok(())
                }
                // ADDX, SUBX Dy,Dx: 4, long 8.
                (0, false) => {
                    let (src, dst) = (cpu.d(r, size), cpu.d(rn, size));
                    let res = extend(cpu, src, dst, size, sub);
                    cpu.set_d(rn, size, res);
                    if long {
                        cpu.idle(4);
                    }
                    Ok(())
                }
                // CMPM (Ay)+,(Ax)+: 12, long 20.
                (1, true) => {
                    let src = postinc(cpu, bus, r, size, true)?;
                    let dst = postinc(cpu, bus, rn, size, false)?;
                    cpu.cmp_flags(src, dst, dst.wrapping_sub(src) & size.mask(), size);
                    Ok(())
                }
                // ADDX, SUBX -(Ay),-(Ax): 18, long 30.
                (1, false) if long => {
                    cpu.idle(2);
                    let src = predec_long(cpu, bus, r)?;
                    let dst = predec_long(cpu, bus, rn)?;
                    let res = extend(cpu, src, dst, size, sub);
                    let addr = cpu.a(rn);
                    cpu.write(bus, addr.wrapping_add(2), Size::Word, res & 0xFFFF)?;
                    cpu.write(bus, addr, Size::Word, res >> 16)
                }
                (1, false) => {
                    let ea = cpu.ea(bus, 4, r, size)?;
                    let src = cpu.load(bus, ea, size)?;
                    let ea = cpu.ea_plain(bus, 4, rn, size)?;
                    let dst = cpu.load(bus, ea, size)?;
                    let res = extend(cpu, src, dst, size, sub);
                    cpu.store(bus, ea, size, res)
                }
                // ADD, SUB, EOR Dn,<memory alterable>: 8 + ea, long 12 + ea.
                _ => {
                    if !(matches!(m, 2..=6) || (m == 7 && r < 2)) {
                        return Err(Fault::Illegal);
                    }
                    let ea = cpu.ea(bus, m, r, size)?;
                    let dst = cpu.load(bus, ea, size)?;
                    let src = cpu.d(rn, size);
                    let res = if cmp {
                        let res = dst ^ src;
                        cpu.nz(res, size);
                        res
                    } else if sub {
                        let res = dst.wrapping_sub(src) & size.mask();
                        cpu.sub_flags(src, dst, res, size);
                        res
                    } else {
                        let res = dst.wrapping_add(src) & size.mask();
                        cpu.add_flags(src, dst, res, size);
                        res
                    };
                    cpu.store(bus, ea, size, res)
                }
            }
        }
    }
}
