//! The 0x4E00-0x4EFF block of line 4: TRAP, LINK, UNLK, MOVE USP, RESET, NOP, STOP, RTE, RTS, TRAPV, RTR, JSR, JMP.

use crate::core::{Fault, Res, mode, reg};
use crate::{Bus, Cpu, sr};

/// Cycles `cpu.exception` spends on the bus: three words pushed, two read.
const EXCEPTION_BUS: u32 = 20;

/// `Err(Fault::Privilege)` in user mode.
fn supervisor(cpu: &Cpu) -> Res<()> {
    if cpu.flag(sr::S) { Ok(()) } else { Err(Fault::Privilege) }
}

/// Loads `pc` with `target` after `idle` internal cycles. An odd target is the address error of the prefetch that
/// follows, taken in this step: the frame holds the target as the access address and target - 4 as the PC
/// (`address_error` stacks `pc - 2`), and the step costs the instruction's own total plus 42, which is `idle` - 4
/// on top of what the bus accesses so far and `address_error` charge.
fn jump(cpu: &mut Cpu, target: u32, idle: u32) -> Res<()> {
    if target & 1 != 0 {
        cpu.cyc = cpu.cyc + idle - 4;
        cpu.pc = target.wrapping_sub(2);
        return Err(Fault::Address { addr: target, write: false, program: true });
    }
    cpu.idle(idle);
    cpu.pc = target;
    Ok(())
}

/// What JMP and JSR idle on top of their bus accesses and the 2 cycles `ea_addr` charges for an index, so that the
/// totals are the manual's: JMP 8 (An), 10 d16, 14 indexed, 10 abs.w, 12 abs.l; JSR 8 more each.
fn jump_idle(mode: u8, reg: u8) -> u32 {
    match (mode, reg) {
        (2, _) | (6, _) | (7, 3) => 4,
        (7, 1) => 0,
        _ => 2,
    }
}

pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, op: u16) -> Res<()> {
    let n = reg(op);
    match op {
        // TRAP #n: vectors 32-47, 34 cycles.
        0x4E40..=0x4E4F => {
            cpu.exception(bus, 32 + (op & 15) as u8)?;
            cpu.idle(34 - 4 - EXCEPTION_BUS);
            Ok(())
        }
        // LINK An,#d16: pushes An (A7: its value before the instruction), An = SP, SP += d16. 16 cycles.
        0x4E50..=0x4E57 => {
            let disp = cpu.fetch16(bus)? as i16 as u32;
            let val = cpu.a(n);
            cpu.push32(bus, val)?;
            let sp = cpu.a(7);
            cpu.set_a(n, sp);
            cpu.set_a(7, sp.wrapping_add(disp));
            Ok(())
        }
        // UNLK An: SP = An, An popped. 12 cycles.
        0x4E58..=0x4E5F => {
            let frame = cpu.a(n);
            cpu.set_a(7, frame);
            let val = cpu.pop32(bus)?;
            cpu.set_a(n, val);
            Ok(())
        }
        // MOVE An,USP: 4 cycles.
        0x4E60..=0x4E67 => {
            supervisor(cpu)?;
            cpu.usp = cpu.a(n);
            Ok(())
        }
        // MOVE USP,An: 4 cycles.
        0x4E68..=0x4E6F => {
            supervisor(cpu)?;
            let usp = cpu.usp;
            cpu.set_a(n, usp);
            Ok(())
        }
        // RESET: the reset line of the devices is held for 124 cycles; 132 in all.
        0x4E70 => {
            supervisor(cpu)?;
            bus.reset_devices();
            cpu.idle(128);
            Ok(())
        }
        // NOP: 4 cycles, all of them the opcode fetch `step` already made.
        0x4E71 => Ok(()),
        // STOP #imm: SR from the immediate word, then waits for an interrupt. 4 cycles: the immediate word is already
        // in the chip's prefetch queue, so its fetch is not charged.
        0x4E72 => {
            supervisor(cpu)?;
            let imm = cpu.fetch16(bus)?;
            cpu.cyc -= 4;
            cpu.set_sr(imm);
            cpu.stopped = true;
            Ok(())
        }
        // RTE: SR and PC popped from the supervisor stack. 20 cycles.
        0x4E73 => {
            supervisor(cpu)?;
            let status = cpu.pop16(bus)?;
            let target = cpu.pop32(bus)?;
            cpu.set_sr(status);
            jump(cpu, target, 4)
        }
        // RTS: 16 cycles.
        0x4E75 => {
            let target = cpu.pop32(bus)?;
            jump(cpu, target, 4)
        }
        // TRAPV: vector 7 when V is set (34 cycles), else 4.
        0x4E76 => {
            if cpu.flag(sr::V) {
                cpu.exception(bus, 7)?;
                cpu.idle(34 - 4 - EXCEPTION_BUS);
            }
            Ok(())
        }
        // RTR: CCR and PC popped. 20 cycles.
        0x4E77 => {
            let ccr = cpu.pop16(bus)?;
            let target = cpu.pop32(bus)?;
            cpu.set_ccr(ccr);
            jump(cpu, target, 4)
        }
        // JSR <control>: pushes the address of the next instruction.
        0x4E80..=0x4EBF => {
            let target = cpu.ea_addr(bus, mode(op), n)?;
            let ret = cpu.pc;
            jump(cpu, target, jump_idle(mode(op), n))?;
            cpu.push32(bus, ret)
        }
        // JMP <control>.
        0x4EC0..=0x4EFF => {
            let target = cpu.ea_addr(bus, mode(op), n)?;
            jump(cpu, target, jump_idle(mode(op), n))
        }
        _ => Err(Fault::Illegal),
    }
}
