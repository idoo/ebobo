//! Flag arithmetic shared by the main, CB and ED tables.
//!
//! Every helper that changes F writes it through [`Cpu::set_f`], which also latches `q`.
//! `Cpu::step` clears `q` before each instruction, so an instruction that never calls a
//! helper leaves `q` at 0, as the chip does. New flag code in the CB and ED tables must end
//! in `set_f` too.

use crate::{Cpu, flag};

/// True when `v` has an even number of set bits (P/V after logic, rotates, shifts, IN, DAA).
pub(crate) fn parity(v: u8) -> bool {
    v.count_ones() & 1 == 0
}

/// S, Y and X copied from `v`, plus Z when `v` is zero; every other bit clear.
pub(crate) fn sz53(v: u8) -> u8 {
    (v & (flag::S | flag::Y | flag::X)) | if v == 0 { flag::Z } else { 0 }
}

/// [`sz53`] plus P/V set to the parity of `v` (the whole F of OR/XOR, the base of shifts and IN).
pub(crate) fn sz53p(v: u8) -> u8 {
    sz53(v) | if parity(v) { flag::PV } else { 0 }
}

impl Cpu {
    /// Writes F and latches it into `q`: the only way an instruction may change the flags.
    pub(crate) fn set_f(&mut self, f: u8) {
        self.f = f;
        self.q = f;
    }

    /// ADD / ADC: A = A + `v` + `carry`; all flags, X/Y from the result.
    pub(crate) fn add8(&mut self, v: u8, carry: bool) {
        let a = self.a;
        let sum = a as u16 + v as u16 + carry as u16;
        let r = sum as u8;
        let mut f = sz53(r);
        if (a ^ v ^ r) & 0x10 != 0 {
            f |= flag::H;
        }
        if (a ^ r) & (v ^ r) & 0x80 != 0 {
            f |= flag::PV;
        }
        if sum > 0xFF {
            f |= flag::C;
        }
        self.a = r;
        self.set_f(f);
    }

    /// A - `v` - `carry` without storing: the result and its F (N set, X/Y from the result).
    fn sub_flags(&self, v: u8, carry: bool) -> (u8, u8) {
        let a = self.a;
        let diff = (a as u16).wrapping_sub(v as u16).wrapping_sub(carry as u16);
        let r = diff as u8;
        let mut f = sz53(r) | flag::N;
        if (a ^ v ^ r) & 0x10 != 0 {
            f |= flag::H;
        }
        if (a ^ v) & (a ^ r) & 0x80 != 0 {
            f |= flag::PV;
        }
        if diff > 0xFF {
            f |= flag::C;
        }
        (r, f)
    }

    /// SUB / SBC: A = A - `v` - `carry`; all flags, N set, X/Y from the result.
    pub(crate) fn sub8(&mut self, v: u8, carry: bool) {
        let (r, f) = self.sub_flags(v, carry);
        self.a = r;
        self.set_f(f);
    }

    /// CP: flags of A - `v`, A kept; X/Y come from the operand `v`, not the result.
    pub(crate) fn cp8(&mut self, v: u8) {
        let (_, f) = self.sub_flags(v, false);
        self.set_f((f & !(flag::X | flag::Y)) | (v & (flag::X | flag::Y)));
    }

    /// AND: A &= `v`; S Z X Y from the result, H set, P/V parity, N and C clear.
    pub(crate) fn and8(&mut self, v: u8) {
        self.a &= v;
        self.set_f(sz53p(self.a) | flag::H);
    }

    /// XOR: A ^= `v`; S Z X Y from the result, P/V parity, H N C clear.
    pub(crate) fn xor8(&mut self, v: u8) {
        self.a ^= v;
        self.set_f(sz53p(self.a));
    }

    /// OR: A |= `v`; S Z X Y from the result, P/V parity, H N C clear.
    pub(crate) fn or8(&mut self, v: u8) {
        self.a |= v;
        self.set_f(sz53p(self.a));
    }

    /// The accumulator operation numbered by opcode bits 3-5: 0 ADD, 1 ADC, 2 SUB, 3 SBC, 4 AND, 5 XOR, 6 OR, 7 CP.
    pub(crate) fn alu8(&mut self, op: u8, v: u8) {
        let carry = self.f & flag::C != 0;
        match op & 7 {
            0 => self.add8(v, false),
            1 => self.add8(v, carry),
            2 => self.sub8(v, false),
            3 => self.sub8(v, carry),
            4 => self.and8(v),
            5 => self.xor8(v),
            6 => self.or8(v),
            _ => self.cp8(v),
        }
    }

    /// INC: returns `v` + 1; C kept, N clear, P/V when the result is 0x80, H on carry from bit 3.
    pub(crate) fn inc8(&mut self, v: u8) -> u8 {
        let r = v.wrapping_add(1);
        let mut f = (self.f & flag::C) | sz53(r);
        if r & 0x0F == 0 {
            f |= flag::H;
        }
        if r == 0x80 {
            f |= flag::PV;
        }
        self.set_f(f);
        r
    }

    /// DEC: returns `v` - 1; C kept, N set, P/V when the result is 0x7F, H on borrow from bit 4.
    pub(crate) fn dec8(&mut self, v: u8) -> u8 {
        let r = v.wrapping_sub(1);
        let mut f = (self.f & flag::C) | sz53(r) | flag::N;
        if r & 0x0F == 0x0F {
            f |= flag::H;
        }
        if r == 0x7F {
            f |= flag::PV;
        }
        self.set_f(f);
        r
    }

    /// F after a rotate of A: S Z P/V kept, H N clear, X/Y from the new A, C = `carry` (0 or 1).
    fn rot_a_flags(&mut self, carry: u8) {
        self.set_f((self.f & (flag::S | flag::Z | flag::PV)) | (self.a & (flag::X | flag::Y)) | carry);
    }

    /// RLCA: A rotated left, bit 7 to bit 0 and C; S Z P/V kept.
    pub(crate) fn rlca(&mut self) {
        self.a = self.a.rotate_left(1);
        self.rot_a_flags(self.a & 1);
    }

    /// RRCA: A rotated right, bit 0 to bit 7 and C; S Z P/V kept.
    pub(crate) fn rrca(&mut self) {
        let carry = self.a & 1;
        self.a = self.a.rotate_right(1);
        self.rot_a_flags(carry);
    }

    /// RLA: A rotated left through C (old C to bit 0, bit 7 to C); S Z P/V kept.
    pub(crate) fn rla(&mut self) {
        let carry = self.a >> 7;
        self.a = (self.a << 1) | (self.f & flag::C);
        self.rot_a_flags(carry);
    }

    /// RRA: A rotated right through C (old C to bit 7, bit 0 to C); S Z P/V kept.
    pub(crate) fn rra(&mut self) {
        let carry = self.a & 1;
        self.a = (self.a >> 1) | ((self.f & flag::C) << 7);
        self.rot_a_flags(carry);
    }

    /// DAA: decimal-adjusts A after an addition (N clear) or subtraction (N set); N kept, P/V parity,
    /// H = bit 4 changed, C set when 0x60 was applied.
    pub(crate) fn daa(&mut self) {
        let a = self.a;
        let mut carry = self.f & flag::C;
        let mut corr = 0u8;
        if self.f & flag::H != 0 || a & 0x0F > 9 {
            corr |= 0x06;
        }
        if carry != 0 || a > 0x99 {
            corr |= 0x60;
            carry = flag::C;
        }
        let r = if self.f & flag::N != 0 { a.wrapping_sub(corr) } else { a.wrapping_add(corr) };
        self.a = r;
        self.set_f(sz53p(r) | (self.f & flag::N) | ((a ^ r) & flag::H) | carry);
    }

    /// CPL: A = !A; H and N set, X/Y from the new A, S Z P/V C kept.
    pub(crate) fn cpl(&mut self) {
        self.a = !self.a;
        self.set_f((self.f & (flag::S | flag::Z | flag::PV | flag::C)) | flag::H | flag::N | (self.a & (flag::X | flag::Y)));
    }

    /// X/Y of SCF and CCF: bits 3 and 5 of `(prev_q ^ F) | A`.
    fn scf_xy(&self, prev_q: u8) -> u8 {
        ((prev_q ^ self.f) | self.a) & (flag::X | flag::Y)
    }

    /// SCF: C set, H N clear, S Z P/V kept; `prev_q` is `q` as the previous instruction left it.
    pub(crate) fn scf(&mut self, prev_q: u8) {
        self.set_f((self.f & (flag::S | flag::Z | flag::PV)) | self.scf_xy(prev_q) | flag::C);
    }

    /// CCF: C inverted, H = old C, N clear, S Z P/V kept; `prev_q` as for [`Cpu::scf`].
    pub(crate) fn ccf(&mut self, prev_q: u8) {
        let half = if self.f & flag::C != 0 { flag::H } else { 0 };
        self.set_f(((self.f & (flag::S | flag::Z | flag::PV | flag::C)) ^ flag::C) | self.scf_xy(prev_q) | half);
    }

    /// NEG: A = 0 - A with the flags of SUB.
    pub(crate) fn neg(&mut self) {
        let v = self.a;
        self.a = 0;
        self.sub8(v, false);
    }

    /// ADD HL/IX/IY,rr: returns `a` + `b`; S Z P/V kept, N clear, H carry from bit 11, C from bit 15,
    /// X/Y from the high byte; `wz` = `a` + 1.
    pub(crate) fn add16(&mut self, a: u16, b: u16) -> u16 {
        let sum = a as u32 + b as u32;
        let r = sum as u16;
        let mut f = (self.f & (flag::S | flag::Z | flag::PV)) | ((r >> 8) as u8 & (flag::X | flag::Y));
        if (a ^ b ^ r) & 0x1000 != 0 {
            f |= flag::H;
        }
        if sum > 0xFFFF {
            f |= flag::C;
        }
        self.wz = a.wrapping_add(1);
        self.set_f(f);
        r
    }

    /// ADC HL,rr: HL = HL + `v` + C; all flags from the 16-bit result (S bit 15, Z whole word, H bit 11,
    /// P/V overflow, X/Y high byte); `wz` = old HL + 1.
    pub(crate) fn adc16(&mut self, v: u16) {
        let hl = self.hl();
        let sum = hl as u32 + v as u32 + (self.f & flag::C) as u32;
        let r = sum as u16;
        let mut f = (r >> 8) as u8 & (flag::S | flag::X | flag::Y);
        if r == 0 {
            f |= flag::Z;
        }
        if (hl ^ v ^ r) & 0x1000 != 0 {
            f |= flag::H;
        }
        if (hl ^ r) & (v ^ r) & 0x8000 != 0 {
            f |= flag::PV;
        }
        if sum > 0xFFFF {
            f |= flag::C;
        }
        self.wz = hl.wrapping_add(1);
        self.set_hl(r);
        self.set_f(f);
    }

    /// SBC HL,rr: HL = HL - `v` - C; all flags from the 16-bit result, N set; `wz` = old HL + 1.
    pub(crate) fn sbc16(&mut self, v: u16) {
        let hl = self.hl();
        let diff = (hl as u32).wrapping_sub(v as u32).wrapping_sub((self.f & flag::C) as u32);
        let r = diff as u16;
        let mut f = ((r >> 8) as u8 & (flag::S | flag::X | flag::Y)) | flag::N;
        if r == 0 {
            f |= flag::Z;
        }
        if (hl ^ v ^ r) & 0x1000 != 0 {
            f |= flag::H;
        }
        if (hl ^ v) & (hl ^ r) & 0x8000 != 0 {
            f |= flag::PV;
        }
        if diff > 0xFFFF {
            f |= flag::C;
        }
        self.wz = hl.wrapping_add(1);
        self.set_hl(r);
        self.set_f(f);
    }
}
