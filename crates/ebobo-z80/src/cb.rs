//! The CB table: rotates and shifts (RLC RRC RL RR SLA SRA SLL SRL), BIT, RES, SET.
//!
//! - `addr == None` (plain `CB op`): the operand is the register numbered by bits 0-2 of the
//!   opcode, or (HL) for 6. 8 T for registers, 15 T for (HL), 12 T for BIT n,(HL).
//! - `addr == Some(a)` (`DD CB d op` / `FD CB d op`): the operand is always the byte at `a`;
//!   every form except BIT also copies the result into the register of bits 0-2 (not for 6,
//!   and always the plain H / L, never the index halves). 19 T, 16 T for BIT (the caller adds
//!   the 4 T of the DD / FD prefix).
//! - Flags: rotates and shifts set S Z Y X P from the result, clear H and N, and put the bit
//!   shifted out in C. BIT sets Z and P/V when the bit is clear, S when bit 7 is tested and
//!   set, H always, keeps C, and takes Y/X from the register, from the high byte of `wz` for
//!   BIT n,(HL), or from the high byte of the address for the indexed form. RES and SET leave
//!   F (and so `q`) alone.

use crate::alu::sz53p;
use crate::{Bus, Cpu, Idx, flag};

/// Rotate or shift number `y` (RLC RRC RL RR SLA SRA SLL SRL) of `v`: sets F, returns the result.
fn rot(cpu: &mut Cpu, y: u8, v: u8) -> u8 {
    let cin = cpu.f & flag::C;
    let (r, cout) = match y {
        0 => (v.rotate_left(1), v >> 7),
        1 => (v.rotate_right(1), v & 1),
        2 => (v << 1 | cin, v >> 7),
        3 => (v >> 1 | cin << 7, v & 1),
        4 => (v << 1, v >> 7),
        5 => (v >> 1 | (v & 0x80), v & 1),
        6 => (v << 1 | 1, v >> 7),
        _ => (v >> 1, v & 1),
    };
    cpu.set_f(sz53p(r) | cout);
    r
}

/// Runs one CB-table instruction. On entry `pc` is at the opcode byte (after `CB`, or after
/// `DD CB d`), R already counts the CB byte, and for `Some(addr)` `wz` already equals `addr`.
/// Fetches the opcode itself: through `Cpu::fetch_m1` for `None` (R counts it), through
/// `Cpu::fetch` for `Some` (R does not). Returns the T-states from the CB byte on.
pub(crate) fn exec(cpu: &mut Cpu, bus: &mut impl Bus, addr: Option<u16>) -> u32 {
    let op = match addr {
        None => cpu.fetch_m1(bus),
        Some(_) => cpu.fetch(bus),
    };
    let (x, y, z) = (op >> 6, (op >> 3) & 7, op & 7);
    // Where the operand lives: memory (with the address) or a register.
    let mem = match addr {
        Some(a) => Some(a),
        None if z == 6 => Some(cpu.hl()),
        None => None,
    };
    let v = match mem {
        Some(a) => bus.read(a),
        None => cpu.reg(Idx::Hl, z),
    };

    if x == 1 {
        let xy_src = match (addr, mem) {
            (Some(a), _) => (a >> 8) as u8,
            (None, Some(_)) => (cpu.wz >> 8) as u8,
            (None, None) => v,
        };
        let bit = v & (1 << y);
        let mut f = (cpu.f & flag::C) | flag::H | (xy_src & (flag::X | flag::Y)) | (bit & flag::S);
        if bit == 0 {
            f |= flag::Z | flag::PV;
        }
        cpu.set_f(f);
        return match (addr, mem) {
            (Some(_), _) => 16,
            (None, Some(_)) => 12,
            (None, None) => 8,
        };
    }

    let r = match x {
        0 => rot(cpu, y, v),
        2 => v & !(1 << y),
        _ => v | (1 << y),
    };
    match mem {
        Some(a) => {
            bus.write(a, r);
            if addr.is_some() && z != 6 {
                cpu.set_reg(Idx::Hl, z, r);
            }
            if addr.is_some() { 19 } else { 15 }
        }
        None => {
            cpu.set_reg(Idx::Hl, z, r);
            8
        }
    }
}
