//! Lines 5 and 6: ADDQ, SUBQ, Scc and DBcc; Bcc, BRA and BSR.

use crate::core::{Ea, Fault, Res, Size, mode, reg, reg9};
use crate::{Bus, Cpu};

/// Loads `pc` with `target` after `idle` internal cycles. An odd target is the address error of the prefetch that
/// follows: the frame holds the target as the access address and target - 4 as the PC (`address_error` stacks
/// `pc - 2`), and the step has spent `spent` cycles before it: the opcode word, 2 internal cycles and any push, the
/// displacement word being already in the prefetch queue.
fn jump(cpu: &mut Cpu, target: u32, idle: u32, spent: u32) -> Res<()> {
    if target & 1 != 0 {
        cpu.cyc = spent;
        cpu.pc = target.wrapping_sub(2);
        return Err(Fault::Address { addr: target, write: false, program: true });
    }
    cpu.idle(idle);
    cpu.pc = target;
    Ok(())
}

/// ADDQ and SUBQ #1-8,<alterable>. Dn: 4 cycles (long 8). An: the whole register, no flags; a byte is illegal; word 8
/// cycles, long 6 (the manual says 8 for both; SingleStepTests, recorded from the chip, has 6 for every long case).
/// Memory: 8 + ea (long 12 + ea), all of it bus accesses.
fn quick(cpu: &mut Cpu, bus: &mut impl Bus, op: u16, size: Size) -> Res<()> {
    let (m, r) = (mode(op), reg(op));
    let n = match reg9(op) {
        0 => 8,
        v => u32::from(v),
    };
    let sub = op & 0x0100 != 0;
    if (m == 7 && r >= 2) || (m == 1 && size == Size::Byte) {
        return Err(Fault::Illegal);
    }
    if m == 1 {
        let a = cpu.a(r);
        cpu.set_a(r, if sub { a.wrapping_sub(n) } else { a.wrapping_add(n) });
        cpu.idle(if size == Size::Long { 2 } else { 4 });
        return Ok(());
    }
    let ea = cpu.ea(bus, m, r, size)?;
    let dst = cpu.load(bus, ea, size)?;
    let res = if sub { dst.wrapping_sub(n) } else { dst.wrapping_add(n) } & size.mask();
    if sub {
        cpu.sub_flags(n, dst, res, size);
    } else {
        cpu.add_flags(n, dst, res, size);
    }
    cpu.store(bus, ea, size, res)?;
    if m == 0 && size == Size::Long {
        cpu.idle(4);
    }
    Ok(())
}

/// DBcc Dn,d16. Condition true: 12 cycles, nothing changes. Else the low word of Dn is decremented: not yet -1, the
/// branch is taken (10 cycles); -1, execution falls through (14 cycles).
fn dbcc(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let base = cpu.pc;
    if cpu.cond(op >> 8) {
        cpu.fetch16(bus)?;
        cpu.idle(4);
        return Ok(());
    }
    let disp = Size::Word.sext(u32::from(cpu.fetch16(bus)?));
    let n = reg(op);
    let count = cpu.d(n, Size::Word).wrapping_sub(1) & 0xFFFF;
    cpu.set_d(n, Size::Word, count);
    if count != 0xFFFF {
        jump(cpu, base.wrapping_add(disp), 2, 6)
    } else {
        cpu.idle(6);
        Ok(())
    }
}

/// Scc <alterable>: the byte becomes 0xFF when the condition holds, else 0x00. Dn: 6 cycles when set, 4 when not.
/// Memory: 8 + ea, the byte is read before it is written.
fn scc(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let (m, r) = (mode(op), reg(op));
    if m == 7 && r >= 2 {
        return Err(Fault::Illegal);
    }
    let on = cpu.cond(op >> 8);
    let ea = cpu.ea(bus, m, r, Size::Byte)?;
    if !matches!(ea, Ea::Dn(_)) {
        cpu.load(bus, ea, Size::Byte)?;
    } else if on {
        cpu.idle(2);
    }
    cpu.store(bus, ea, Size::Byte, if on { 0xFF } else { 0 })
}

/// Bcc, BRA (condition 0) and BSR (condition 1). The displacement is the low byte of the opcode, or the next word
/// when that byte is 0, and counts from the address after the opcode word. Taken: 10 cycles; not taken: 8 (byte
/// form) or 12 (word form); BSR pushes the address of the next instruction and costs 18.
fn branch(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let base = cpu.pc;
    let cc = (op >> 8) & 15;
    let short = op & 0xFF != 0;
    let disp = if short { Size::Byte.sext(u32::from(op & 0xFF)) } else { Size::Word.sext(u32::from(cpu.fetch16(bus)?)) };
    let target = base.wrapping_add(disp);
    let idle = if short { 6 } else { 2 };
    if cc == 1 {
        cpu.idle(idle);
        cpu.push32(bus, cpu.pc)?;
        return jump(cpu, target, 0, 14);
    }
    if cc == 0 || cpu.cond(cc) {
        return jump(cpu, target, idle, 6);
    }
    cpu.idle(4);
    Ok(())
}

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    if op & 0xF000 == 0x6000 {
        return branch(cpu, bus, op);
    }
    match Size::of(op) {
        Some(size) => quick(cpu, bus, op, size),
        None if mode(op) == 1 => dbcc(cpu, bus, op),
        None => scc(cpu, bus, op),
    }
}
