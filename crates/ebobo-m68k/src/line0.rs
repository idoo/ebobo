//! Line 0: ORI, ANDI, SUBI, ADDI, EORI, CMPI (also to CCR and SR), BTST, BCHG, BCLR, BSET and MOVEP.

use crate::core::{Ea, Fault, Res, Size, mode, reg, reg9};
use crate::{Bus, Cpu, sr};

/// A data alterable mode: Dn, (An), (An)+, -(An), d16(An), d8(An,Xn), abs.W, abs.L.
fn data_alterable(mode: u8, reg: u8) -> bool {
    matches!(mode, 0 | 2..=6) || (mode == 7 && reg < 2)
}

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    if op & 0x0100 != 0 {
        if mode(op) == 1 {
            return movep(cpu, bus, op);
        }
        return bit(cpu, bus, op, false);
    }
    let kind = reg9(op);
    match kind {
        4 => bit(cpu, bus, op, true),
        7 => Err(Fault::Illegal),
        0 | 1 | 5 if op & 0xFF == 0x3C => to_status(cpu, bus, kind, false),
        0 | 1 | 5 if op & 0xFF == 0x7C => to_status(cpu, bus, kind, true),
        _ => immediate(cpu, bus, op, kind),
    }
}

/// ORI (0), ANDI (1), EORI (5) to CCR or, privileged, to SR: 20 cycles.
fn to_status(cpu: &mut Cpu, bus: &mut impl Bus, kind: u8, whole: bool) -> Res<()> {
    if whole && cpu.sr & sr::S == 0 {
        return Err(Fault::Privilege);
    }
    let imm = cpu.fetch16(bus)?;
    let old = if whole { cpu.sr } else { u16::from(cpu.ccr()) };
    let new = match kind {
        0 => old | imm,
        1 => old & imm,
        _ => old ^ imm,
    };
    if whole {
        cpu.set_sr(new);
    } else {
        cpu.set_ccr((new & 0x1F) as _);
    }
    cpu.idle(12);
    Ok(())
}

/// ORI (0), ANDI (1), SUBI (2), ADDI (3), EORI (5), CMPI (6) #imm,<ea>.
/// Register: 8 cycles, long 16 (CMPI 14). Memory: 12 + ea, long 20 + ea (CMPI 8 + ea, long 12 + ea).
fn immediate(cpu: &mut Cpu, bus: &mut impl Bus, op: u16, kind: u8) -> Res<()> {
    let Some(size) = Size::of(op) else { return Err(Fault::Illegal) };
    let (m, r) = (mode(op), reg(op));
    if !data_alterable(m, r) {
        return Err(Fault::Illegal);
    }
    let src = match size {
        Size::Byte => u32::from(cpu.fetch16(bus)?) & 0xFF,
        Size::Word => u32::from(cpu.fetch16(bus)?),
        Size::Long => cpu.fetch32(bus)?,
    };
    let ea = cpu.ea(bus, m, r, size)?;
    let dst = cpu.load(bus, ea, size)?;
    let res = match kind {
        0 => dst | src,
        1 => dst & src,
        2 | 6 => dst.wrapping_sub(src) & size.mask(),
        3 => dst.wrapping_add(src) & size.mask(),
        _ => dst ^ src,
    };
    match kind {
        2 => cpu.sub_flags(src, dst, res, size),
        3 => cpu.add_flags(src, dst, res, size),
        6 => cpu.cmp_flags(src, dst, res, size),
        _ => cpu.nz(res, size),
    }
    if size == Size::Long && m == 0 {
        cpu.idle(if kind == 6 { 2 } else { 4 });
    }
    if kind != 6 {
        cpu.store(bus, ea, size, res)?;
    }
    Ok(())
}

/// BTST (0), BCHG (1), BCLR (2), BSET (3), the bit number in a data register or, `fixed`, in an extension word.
/// On a data register: 32 bits; BTST 6, BCHG and BSET 6 or 8 (bit 16 and up), BCLR 8 or 10. On memory: a byte;
/// BTST 4 + ea, the others 8 + ea. The immediate forms cost the 4 of their extension word more.
fn bit(cpu: &mut Cpu, bus: &mut impl Bus, op: u16, fixed: bool) -> Res<()> {
    let kind = (op >> 6) & 3;
    let (m, r) = (mode(op), reg(op));
    let legal = match (kind, fixed) {
        (0, false) => m != 1 && (m < 7 || r <= 4),
        (0, true) => m != 1 && (m < 7 || r <= 3),
        _ => data_alterable(m, r),
    };
    if !legal {
        return Err(Fault::Illegal);
    }
    let number = if fixed { u32::from(cpu.fetch16(bus)?) } else { cpu.d(reg9(op), Size::Long) };
    let size = if m == 0 { Size::Long } else { Size::Byte };
    let ea = cpu.ea(bus, m, r, size)?;
    let old = cpu.load(bus, ea, size)?;
    let mask = 1u32 << (number & (size.bits() - 1));
    cpu.set_flag(sr::Z, old & mask == 0);
    let new = match kind {
        0 => old,
        1 => old ^ mask,
        2 => old & !mask,
        _ => old | mask,
    };
    if let Ea::Dn(_) = ea {
        let high = if mask > 0x8000 { 2 } else { 0 };
        cpu.idle(match kind {
            0 => 2,
            2 => 4 + high,
            _ => 2 + high,
        });
    } else if m == 7 && r == 4 {
        // BTST Dn,#imm: 10 cycles, two more than its two words.
        cpu.idle(2);
    }
    if kind != 0 {
        cpu.store(bus, ea, size, new)?;
    }
    Ok(())
}

/// MOVEP: a word or long between Dn and every other byte from d16(An), the high byte first. 16 or 24 cycles.
fn movep(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let disp = cpu.fetch16(bus)? as i16;
    let mut addr = cpu.a(reg(op)).wrapping_add(disp as u32);
    let long = op & 0x0040 != 0;
    let size = if long { Size::Long } else { Size::Word };
    let count = size.bytes();
    let dn = reg9(op);
    if op & 0x0080 != 0 {
        let v = cpu.d(dn, size);
        for i in (0..count).rev() {
            cpu.write(bus, addr, Size::Byte, (v >> (i * 8)) & 0xFF)?;
            addr = addr.wrapping_add(2);
        }
    } else {
        let mut v = 0;
        for _ in 0..count {
            v = (v << 8) | cpu.read(bus, addr, Size::Byte)?;
            addr = addr.wrapping_add(2);
        }
        cpu.set_d(dn, size, v);
    }
    Ok(())
}
