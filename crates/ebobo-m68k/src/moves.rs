//! Lines 1, 2, 3 and 7: MOVE and MOVEA (byte, long, word) and MOVEQ.

use crate::core::{Ea, Fault, Res, Size, mode, reg, reg9};
use crate::{Bus, Cpu};

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    match op >> 12 {
        1 => mov(cpu, bus, op, Size::Byte),
        2 => mov(cpu, bus, op, Size::Long),
        3 => mov(cpu, bus, op, Size::Word),
        // Bit 8 of a line 7 opcode is always 0.
        7 if op & 0x0100 == 0 => moveq(cpu, op),
        _ => Err(Fault::Illegal),
    }
}

/// MOVE <ea>,<ea> and MOVEA <ea>,An. The source is read, then the destination address is worked out, then the value
/// is written. MOVE: N and Z from the value, V and C cleared, X kept. MOVEA (destination mode 1, word or long): the
/// word is sign-extended, the whole register is written, no flag changes.
/// Cycles: only the bus accesses and the 2 of an indexed mode or of a -(An) source, which the helpers charge.
///
/// What an odd destination (address error on the write) shows of the chip's order (MAME microcode suite):
/// - (An), (An)+: the frame's PC is `pc + 2`; (An)+ is stepped only after the write, so a faulting write leaves An.
///   A long has set no flags yet from a register or immediate source; from a memory source N and Z come from the low
///   word (V, C cleared).
/// - -(An): the prefetch of the next opcode (4 cycles) comes before the write; the frame's PC is `pc + 2`. A long goes
///   out low word first, and a fault there leaves An unstepped. A word stacks the next opcode as instruction register.
/// - d16(An), d8(An,Xn): the frame's PC is `pc`; a long from a register or immediate keeps V and C.
/// - (xxx).l after a memory source: the low address word is taken from the prefetch queue, and its bus read (and
///   `pc` passing it) comes only after the write; a long faulting there has N and Z from the low word.
fn mov(cpu: &mut Cpu, bus: &mut impl Bus, op: u16, size: Size) -> Res<()> {
    let (smode, sreg) = (mode(op), reg(op));
    let dmode = ((op >> 6) & 7) as u8;
    let dreg = reg9(op);
    let byte = matches!(size, Size::Byte);
    // No byte from or to an address register; the destination is never PC-relative or immediate.
    if (byte && (smode == 1 || dmode == 1)) || (dmode == 7 && dreg > 1) || (smode == 7 && sreg > 4) {
        return Err(Fault::Illegal);
    }
    let src = cpu.ea(bus, smode, sreg, size)?;
    let val = cpu.load(bus, src, size)?;
    if dmode == 1 {
        cpu.set_a(dreg, size.sext(val));
        return Ok(());
    }
    let late_ext = dmode == 7 && dreg == 1 && matches!(src, Ea::Mem(_));
    let dst = if late_ext {
        // No `operand` call for this destination: drop the source's fault notes.
        (cpu.ae_pc, cpu.ae_program, cpu.ae_restore) = (0, false, None);
        let hi = u32::from(cpu.fetch16(bus)?);
        // Already in the prefetch queue: no bus cycle until the queue is refilled after the write.
        let lo = u32::from(bus.read16(cpu.pc & 0x00FF_FFFF));
        Ea::Mem(hi << 16 | lo)
    } else {
        let before = cpu.a(dreg);
        let dst = cpu.ea_plain(bus, dmode, dreg, size)?;
        if dmode == 3 {
            cpu.set_a(dreg, before);
        }
        dst
    };
    // The flags before MOVE touches them: some faulting writes come before the flags are (all) set.
    let old_ccr = cpu.ccr();
    let from_mem = matches!(src, Ea::Mem(_));
    let long = matches!(size, Size::Long);
    cpu.nz(val, size);
    match dst {
        Ea::Mem(addr) if dmode == 4 => {
            let written = if long {
                cpu.write(bus, addr.wrapping_add(2), Size::Word, val & 0xFFFF).and_then(|()| cpu.write(bus, addr, Size::Word, val >> 16))
            } else {
                cpu.store(bus, dst, size, val)
            };
            written.inspect_err(|_| {
                cpu.ae_pc = 2;
                cpu.cyc += 4;
                if long {
                    cpu.set_a(dreg, addr.wrapping_add(4));
                } else {
                    // The prefetched next opcode is in the instruction register by the time the word goes out.
                    cpu.ae_ir = Some(bus.read16(cpu.pc & 0x00FF_FFFF));
                }
            })
        }
        Ea::Mem(_) if dmode == 2 || dmode == 3 => {
            cpu.store(bus, dst, size, val).inspect_err(|_| {
                cpu.ae_pc = 2;
                if long {
                    cpu.set_ccr(old_ccr);
                    if from_mem {
                        cpu.nz(val & 0xFFFF, Size::Word);
                    }
                }
            })?;
            if dmode == 3 {
                // A byte on A7 steps by 2, which keeps the stack even.
                let by = if byte && dreg == 7 { 2 } else { size.bytes() };
                cpu.set_a(dreg, cpu.a(dreg).wrapping_add(by));
            }
            Ok(())
        }
        Ea::Mem(_) if dmode == 5 || dmode == 6 => cpu.store(bus, dst, size, val).inspect_err(|_| {
            cpu.ae_pc = 0;
            if long && !from_mem {
                cpu.set_ccr((cpu.ccr() & !3) | (old_ccr & 3));
            }
        }),
        _ => {
            cpu.store(bus, dst, size, val).inspect_err(|_| {
                if late_ext && long {
                    cpu.set_ccr(old_ccr);
                    cpu.nz(val & 0xFFFF, Size::Word);
                }
            })?;
            if late_ext {
                cpu.fetch16(bus)?;
            }
            Ok(())
        }
    }
}

/// MOVEQ #d8,Dn: the byte of the opcode, sign-extended, fills the register; N and Z from it, V and C cleared.
/// 4 cycles: the opcode fetch, which `step` already made.
fn moveq(cpu: &mut Cpu, op: u16) -> Res<()> {
    let val = Size::Byte.sext(u32::from(op));
    cpu.set_d(reg9(op), Size::Long, val);
    cpu.nz(val, Size::Long);
    Ok(())
}
