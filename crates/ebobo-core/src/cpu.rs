//! Ricoh 2A03 CPU (6502 without decimal mode). Owner: CPU slice.
//!
//! Cycle model: every `CpuBus::read` / `CpuBus::write` call is exactly one CPU cycle, and the
//! bus advances PPU/APU on each call. Instructions must therefore issue exactly the bus
//! accesses real hardware does, dummy reads and dummy writes included.

pub trait CpuBus {
    /// One CPU read cycle.
    fn read(&mut self, addr: u16) -> u8;
    /// One CPU write cycle.
    fn write(&mut self, addr: u16, val: u8);
    /// NMI edge latched by the bus (PPU NMI output went low to high); stays set until
    /// `ack_nmi`.
    fn nmi_pending(&self) -> bool;
    fn ack_nmi(&mut self);
    /// Level-triggered IRQ line (APU frame counter, DMC, cartridge).
    fn irq_line(&self) -> bool;
}

const FLAG_C: u8 = 0x01;
const FLAG_Z: u8 = 0x02;
const FLAG_I: u8 = 0x04;
const FLAG_B: u8 = 0x10;
const FLAG_U: u8 = 0x20;
const FLAG_V: u8 = 0x40;
const FLAG_N: u8 = 0x80;

/// Magic constant of the unstable "A | magic" opcodes (XAA, LAX #imm) on the 2A03.
const MAGIC: u8 = 0xEE;

const VEC_NMI: u16 = 0xFFFA;
const VEC_RESET: u16 = 0xFFFC;
const VEC_IRQ: u16 = 0xFFFE;

/// Registers plus whatever interrupt-latch state the core needs. Must stay `Clone + Hash`
/// (it is part of the rollback snapshot and of the desync hash).
#[derive(Clone, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct Cpu {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub sp: u8,
    pub p: u8,
    pub pc: u16,
    /// Set by KIL/JAM opcodes; afterwards `step` only burns one idle cycle per call.
    pub jammed: bool,
    /// Interrupt lines as sampled at the start of the most recent CPU cycle, and the sample
    /// before that. The end-of-instruction poll uses the sample taken before the final cycle
    /// (the hardware's penultimate-cycle rule), which is `sig_*` here because every sample is
    /// taken *before* its bus access. A taken branch that does not cross a page uses
    /// `sig_*_prev`, delaying recognition by one instruction.
    sig_nmi: bool,
    sig_irq: bool,
    sig_nmi_prev: bool,
    sig_irq_prev: bool,
}

impl Default for Cpu {
    fn default() -> Cpu {
        Cpu::new()
    }
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu {
            a: 0,
            x: 0,
            y: 0,
            sp: 0xFD,
            p: 0x34,
            pc: 0,
            jammed: false,
            sig_nmi: false,
            sig_irq: false,
            sig_nmi_prev: false,
            sig_irq_prev: false,
        }
    }

    /// Power-on: 7-cycle reset sequence, PC loaded from $FFFC/$FFFD.
    pub fn power_on<B: CpuBus>(&mut self, bus: &mut B) {
        self.a = 0;
        self.x = 0;
        self.y = 0;
        self.p = 0x34;
        self.sp = 0x00;
        self.jammed = false;
        self.reset(bus);
    }

    /// Reset button: SP -= 3 without writes, I flag set, PC from $FFFC/$FFFD, 7 cycles.
    pub fn reset<B: CpuBus>(&mut self, bus: &mut B) {
        self.jammed = false;
        let pc = self.pc;
        self.rd(bus, pc);
        self.rd(bus, pc);
        // Three "pushes" that read instead of writing; SP still decrements.
        for _ in 0..3 {
            let sp = self.sp;
            self.rd(bus, 0x0100 | sp as u16);
            self.sp = sp.wrapping_sub(1);
        }
        self.p |= FLAG_I;
        let lo = self.rd(bus, VEC_RESET);
        let hi = self.rd(bus, VEC_RESET + 1);
        self.pc = u16::from(lo) | (u16::from(hi) << 8);
        self.sig_nmi = false;
        self.sig_irq = false;
        self.sig_nmi_prev = false;
        self.sig_irq_prev = false;
    }

    // ---------------------------------------------------------------- cycles

    /// Samples the interrupt inputs as they stand at the start of the cycle that is about to
    /// run. Called from every bus access, so after an instruction's last access `sig_*` holds
    /// exactly the state the real CPU latched during its penultimate cycle.
    #[inline(always)]
    fn sample<B: CpuBus>(&mut self, bus: &B) {
        self.sig_nmi_prev = self.sig_nmi;
        self.sig_irq_prev = self.sig_irq;
        self.sig_nmi = bus.nmi_pending();
        self.sig_irq = bus.irq_line() && (self.p & FLAG_I) == 0;
    }

    #[inline(always)]
    fn rd<B: CpuBus>(&mut self, bus: &mut B, addr: u16) -> u8 {
        self.sample(bus);
        bus.read(addr)
    }

    #[inline(always)]
    fn wr<B: CpuBus>(&mut self, bus: &mut B, addr: u16, val: u8) {
        self.sample(bus);
        bus.write(addr, val);
    }

    #[inline(always)]
    fn fetch<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let pc = self.pc;
        self.pc = pc.wrapping_add(1);
        self.rd(bus, pc)
    }

    #[inline(always)]
    fn fetch16<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let lo = self.fetch(bus);
        let hi = self.fetch(bus);
        u16::from(lo) | (u16::from(hi) << 8)
    }

    #[inline(always)]
    fn push<B: CpuBus>(&mut self, bus: &mut B, val: u8) {
        let sp = self.sp;
        self.wr(bus, 0x0100 | u16::from(sp), val);
        self.sp = sp.wrapping_sub(1);
    }

    #[inline(always)]
    fn pull<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let sp = self.sp.wrapping_add(1);
        self.sp = sp;
        self.rd(bus, 0x0100 | u16::from(sp))
    }

    /// Dummy read of the current stack slot (PLA/PLP/RTS/RTI internal cycle).
    #[inline(always)]
    fn peek_stack<B: CpuBus>(&mut self, bus: &mut B) {
        let sp = self.sp;
        self.rd(bus, 0x0100 | u16::from(sp));
    }

    /// Dummy read of PC (implied / accumulator addressing).
    #[inline(always)]
    fn idle<B: CpuBus>(&mut self, bus: &mut B) {
        let pc = self.pc;
        self.rd(bus, pc);
    }

    // --------------------------------------------------------------- flags

    #[inline(always)]
    fn set_flag(&mut self, mask: u8, on: bool) {
        if on {
            self.p |= mask;
        } else {
            self.p &= !mask;
        }
    }

    #[inline(always)]
    fn set_zn(&mut self, v: u8) {
        self.p = (self.p & !(FLAG_Z | FLAG_N)) | (if v == 0 { FLAG_Z } else { 0 }) | (v & FLAG_N);
    }

    #[inline(always)]
    fn carry(&self) -> u8 {
        self.p & FLAG_C
    }

    // ------------------------------------------------------- address modes

    #[inline(always)]
    fn am_zp<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        u16::from(self.fetch(bus))
    }

    #[inline(always)]
    fn am_zpx<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let b = self.fetch(bus);
        self.rd(bus, u16::from(b));
        u16::from(b.wrapping_add(self.x))
    }

    #[inline(always)]
    fn am_zpy<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let b = self.fetch(bus);
        self.rd(bus, u16::from(b));
        u16::from(b.wrapping_add(self.y))
    }

    /// `abs,X` / `abs,Y`. `always` forces the un-carried dummy read (writes and RMW).
    #[inline(always)]
    fn am_abi<B: CpuBus>(&mut self, bus: &mut B, idx: u8, always: bool) -> u16 {
        let base = self.fetch16(bus);
        let eff = base.wrapping_add(u16::from(idx));
        if always || (eff & 0xFF00) != (base & 0xFF00) {
            self.rd(bus, (base & 0xFF00) | (eff & 0x00FF));
        }
        eff
    }

    #[inline(always)]
    fn am_izx<B: CpuBus>(&mut self, bus: &mut B) -> u16 {
        let b = self.fetch(bus);
        self.rd(bus, u16::from(b));
        let p = b.wrapping_add(self.x);
        let lo = self.rd(bus, u16::from(p));
        let hi = self.rd(bus, u16::from(p.wrapping_add(1)));
        u16::from(lo) | (u16::from(hi) << 8)
    }

    #[inline(always)]
    fn am_izy<B: CpuBus>(&mut self, bus: &mut B, always: bool) -> u16 {
        let b = self.fetch(bus);
        let lo = self.rd(bus, u16::from(b));
        let hi = self.rd(bus, u16::from(b.wrapping_add(1)));
        let base = u16::from(lo) | (u16::from(hi) << 8);
        let eff = base.wrapping_add(u16::from(self.y));
        if always || (eff & 0xFF00) != (base & 0xFF00) {
            self.rd(bus, (base & 0xFF00) | (eff & 0x00FF));
        }
        eff
    }

    // Read-operand shorthands.
    #[inline(always)]
    fn r_zp<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.am_zp(bus);
        self.rd(bus, a)
    }
    #[inline(always)]
    fn r_zpx<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.am_zpx(bus);
        self.rd(bus, a)
    }
    #[inline(always)]
    fn r_zpy<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.am_zpy(bus);
        self.rd(bus, a)
    }
    #[inline(always)]
    fn r_abs<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.fetch16(bus);
        self.rd(bus, a)
    }
    #[inline(always)]
    fn r_abx<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.am_abi(bus, self.x, false);
        self.rd(bus, a)
    }
    #[inline(always)]
    fn r_aby<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.am_abi(bus, self.y, false);
        self.rd(bus, a)
    }
    #[inline(always)]
    fn r_izx<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.am_izx(bus);
        self.rd(bus, a)
    }
    #[inline(always)]
    fn r_izy<B: CpuBus>(&mut self, bus: &mut B) -> u8 {
        let a = self.am_izy(bus, false);
        self.rd(bus, a)
    }

    /// Read-modify-write prologue: read the cell, write the old value back (the dummy write),
    /// and hand the old value to the caller.
    #[inline(always)]
    fn rmw_begin<B: CpuBus>(&mut self, bus: &mut B, addr: u16) -> u8 {
        let v = self.rd(bus, addr);
        self.wr(bus, addr, v);
        v
    }

    // ---------------------------------------------------------- operations

    #[inline(always)]
    fn lda(&mut self, v: u8) {
        self.a = v;
        self.set_zn(v);
    }
    #[inline(always)]
    fn ldx(&mut self, v: u8) {
        self.x = v;
        self.set_zn(v);
    }
    #[inline(always)]
    fn ldy(&mut self, v: u8) {
        self.y = v;
        self.set_zn(v);
    }
    #[inline(always)]
    fn lax(&mut self, v: u8) {
        self.a = v;
        self.x = v;
        self.set_zn(v);
    }
    #[inline(always)]
    fn ora(&mut self, v: u8) {
        let r = self.a | v;
        self.lda(r);
    }
    #[inline(always)]
    fn and(&mut self, v: u8) {
        let r = self.a & v;
        self.lda(r);
    }
    #[inline(always)]
    fn eor(&mut self, v: u8) {
        let r = self.a ^ v;
        self.lda(r);
    }

    /// Binary ADC (no decimal mode on the 2A03).
    #[inline(always)]
    fn adc(&mut self, v: u8) {
        let a = self.a;
        let sum = u16::from(a) + u16::from(v) + u16::from(self.carry());
        let r = sum as u8;
        self.set_flag(FLAG_C, sum > 0xFF);
        self.set_flag(FLAG_V, ((a ^ r) & (v ^ r) & 0x80) != 0);
        self.lda(r);
    }

    #[inline(always)]
    fn sbc(&mut self, v: u8) {
        self.adc(!v);
    }

    #[inline(always)]
    fn compare(&mut self, reg: u8, v: u8) {
        let r = reg.wrapping_sub(v);
        self.set_flag(FLAG_C, reg >= v);
        self.set_zn(r);
    }

    #[inline(always)]
    fn bit(&mut self, v: u8) {
        self.set_flag(FLAG_Z, (self.a & v) == 0);
        self.p = (self.p & !(FLAG_N | FLAG_V)) | (v & (FLAG_N | FLAG_V));
    }

    #[inline(always)]
    fn asl(&mut self, v: u8) -> u8 {
        self.set_flag(FLAG_C, (v & 0x80) != 0);
        let r = v << 1;
        self.set_zn(r);
        r
    }
    #[inline(always)]
    fn lsr(&mut self, v: u8) -> u8 {
        self.set_flag(FLAG_C, (v & 0x01) != 0);
        let r = v >> 1;
        self.set_zn(r);
        r
    }
    #[inline(always)]
    fn rol(&mut self, v: u8) -> u8 {
        let c = self.carry();
        self.set_flag(FLAG_C, (v & 0x80) != 0);
        let r = (v << 1) | c;
        self.set_zn(r);
        r
    }
    #[inline(always)]
    fn ror(&mut self, v: u8) -> u8 {
        let c = self.carry();
        self.set_flag(FLAG_C, (v & 0x01) != 0);
        let r = (v >> 1) | (c << 7);
        self.set_zn(r);
        r
    }
    #[inline(always)]
    fn inc(&mut self, v: u8) -> u8 {
        let r = v.wrapping_add(1);
        self.set_zn(r);
        r
    }
    #[inline(always)]
    fn dec(&mut self, v: u8) -> u8 {
        let r = v.wrapping_sub(1);
        self.set_zn(r);
        r
    }

    /// Returns `true` when the end-of-instruction interrupt poll must use the older sample
    /// (taken branch that stayed inside the page).
    #[inline(always)]
    fn branch<B: CpuBus>(&mut self, bus: &mut B, cond: bool) -> bool {
        let off = self.fetch(bus) as i8 as u16;
        if !cond {
            return false;
        }
        self.idle(bus);
        let pc = self.pc;
        let new = pc.wrapping_add(off);
        if (new & 0xFF00) != (pc & 0xFF00) {
            self.rd(bus, (pc & 0xFF00) | (new & 0x00FF));
            self.pc = new;
            false
        } else {
            self.pc = new;
            true
        }
    }

    /// Interrupt entry (7 cycles). `nmi` picks the vector up front; an IRQ entry still checks
    /// for a pending NMI at the vector-fetch point and hijacks.
    fn interrupt<B: CpuBus>(&mut self, bus: &mut B, nmi: bool) {
        let pc = self.pc;
        self.rd(bus, pc);
        self.rd(bus, pc);
        self.push(bus, (pc >> 8) as u8);
        self.push(bus, pc as u8);
        let pushed = (self.p & !FLAG_B) | FLAG_U;
        self.push(bus, pushed);
        let mut vec = if nmi { VEC_NMI } else { VEC_IRQ };
        if !nmi && self.sig_nmi {
            vec = VEC_NMI;
        }
        if vec == VEC_NMI {
            bus.ack_nmi();
        }
        self.p |= FLAG_I;
        let lo = self.rd(bus, vec);
        let hi = self.rd(bus, vec + 1);
        self.pc = u16::from(lo) | (u16::from(hi) << 8);
        self.sig_nmi = false;
        self.sig_irq = false;
        self.sig_nmi_prev = false;
        self.sig_irq_prev = false;
    }

    /// Executes one instruction, or one interrupt entry sequence when an interrupt was polled.
    pub fn step<B: CpuBus>(&mut self, bus: &mut B) {
        if self.jammed {
            self.idle(bus);
            return;
        }
        let op = self.fetch(bus);
        let mut delay_poll = false;

        match op {
            // ------------------------------------------------ control flow
            0x00 => {
                // BRK: pushes PC+2 and P with B set.
                let pc = self.pc;
                self.rd(bus, pc);
                self.pc = pc.wrapping_add(1);
                let ret = self.pc;
                self.push(bus, (ret >> 8) as u8);
                self.push(bus, ret as u8);
                self.push(bus, self.p | FLAG_B | FLAG_U);
                let mut vec = VEC_IRQ;
                if self.sig_nmi {
                    vec = VEC_NMI;
                    bus.ack_nmi();
                }
                self.p |= FLAG_I;
                let lo = self.rd(bus, vec);
                let hi = self.rd(bus, vec + 1);
                self.pc = u16::from(lo) | (u16::from(hi) << 8);
                self.sig_nmi = false;
                self.sig_irq = false;
                self.sig_nmi_prev = false;
                self.sig_irq_prev = false;
                return;
            }
            0x20 => {
                // JSR
                let lo = self.fetch(bus);
                self.peek_stack(bus);
                let pc = self.pc;
                self.push(bus, (pc >> 8) as u8);
                self.push(bus, pc as u8);
                let hi = self.rd(bus, pc);
                self.pc = u16::from(lo) | (u16::from(hi) << 8);
            }
            0x40 => {
                // RTI. The status byte is pulled on cycle 4 of 6, so the restored I flag is
                // already in effect for the end-of-instruction poll: an IRQ pending while RTI
                // clears I is serviced immediately afterwards (unlike CLI/SEI/PLP).
                self.idle(bus);
                self.peek_stack(bus);
                let p = self.pull(bus);
                self.p = (p & !FLAG_B) | FLAG_U;
                let lo = self.pull(bus);
                let hi = self.pull(bus);
                self.pc = u16::from(lo) | (u16::from(hi) << 8);
            }
            0x60 => {
                // RTS
                self.idle(bus);
                self.pc = self.pc.wrapping_add(1);
                self.peek_stack(bus);
                let lo = self.pull(bus);
                let hi = self.pull(bus);
                let addr = u16::from(lo) | (u16::from(hi) << 8);
                self.rd(bus, addr);
                self.pc = addr.wrapping_add(1);
            }
            0x4C => {
                let a = self.fetch16(bus);
                self.pc = a;
            }
            0x6C => {
                let ptr = self.fetch16(bus);
                let lo = self.rd(bus, ptr);
                let hi = self.rd(bus, (ptr & 0xFF00) | u16::from((ptr as u8).wrapping_add(1)));
                self.pc = u16::from(lo) | (u16::from(hi) << 8);
            }

            // ---------------------------------------------------- branches
            0x10 => delay_poll = self.branch(bus, (self.p & FLAG_N) == 0),
            0x30 => delay_poll = self.branch(bus, (self.p & FLAG_N) != 0),
            0x50 => delay_poll = self.branch(bus, (self.p & FLAG_V) == 0),
            0x70 => delay_poll = self.branch(bus, (self.p & FLAG_V) != 0),
            0x90 => delay_poll = self.branch(bus, (self.p & FLAG_C) == 0),
            0xB0 => delay_poll = self.branch(bus, (self.p & FLAG_C) != 0),
            0xD0 => delay_poll = self.branch(bus, (self.p & FLAG_Z) == 0),
            0xF0 => delay_poll = self.branch(bus, (self.p & FLAG_Z) != 0),

            // ------------------------------------------------------- stack
            0x08 => {
                self.idle(bus);
                self.push(bus, self.p | FLAG_B | FLAG_U);
            }
            0x28 => {
                self.idle(bus);
                self.peek_stack(bus);
                let v = self.pull(bus);
                self.p = (v & !FLAG_B) | FLAG_U;
            }
            0x48 => {
                self.idle(bus);
                self.push(bus, self.a);
            }
            0x68 => {
                self.idle(bus);
                self.peek_stack(bus);
                let v = self.pull(bus);
                self.lda(v);
            }

            // ---------------------------------------------------- implied
            0x18 => {
                self.idle(bus);
                self.set_flag(FLAG_C, false);
            }
            0x38 => {
                self.idle(bus);
                self.set_flag(FLAG_C, true);
            }
            0x58 => {
                self.idle(bus);
                self.set_flag(FLAG_I, false);
            }
            0x78 => {
                self.idle(bus);
                self.set_flag(FLAG_I, true);
            }
            0xB8 => {
                self.idle(bus);
                self.set_flag(FLAG_V, false);
            }
            0xD8 => {
                self.idle(bus);
                self.p &= !0x08;
            }
            0xF8 => {
                self.idle(bus);
                self.p |= 0x08;
            }
            0xAA => {
                self.idle(bus);
                let v = self.a;
                self.ldx(v);
            }
            0xA8 => {
                self.idle(bus);
                let v = self.a;
                self.ldy(v);
            }
            0xBA => {
                self.idle(bus);
                let v = self.sp;
                self.ldx(v);
            }
            0x8A => {
                self.idle(bus);
                let v = self.x;
                self.lda(v);
            }
            0x9A => {
                self.idle(bus);
                self.sp = self.x;
            }
            0x98 => {
                self.idle(bus);
                let v = self.y;
                self.lda(v);
            }
            0xCA => {
                self.idle(bus);
                let v = self.x.wrapping_sub(1);
                self.ldx(v);
            }
            0xE8 => {
                self.idle(bus);
                let v = self.x.wrapping_add(1);
                self.ldx(v);
            }
            0x88 => {
                self.idle(bus);
                let v = self.y.wrapping_sub(1);
                self.ldy(v);
            }
            0xC8 => {
                self.idle(bus);
                let v = self.y.wrapping_add(1);
                self.ldy(v);
            }
            0xEA | 0x1A | 0x3A | 0x5A | 0x7A | 0xDA | 0xFA => self.idle(bus),

            // --------------------------------------------------------- ORA
            0x09 => {
                let v = self.fetch(bus);
                self.ora(v);
            }
            0x05 => {
                let v = self.r_zp(bus);
                self.ora(v);
            }
            0x15 => {
                let v = self.r_zpx(bus);
                self.ora(v);
            }
            0x0D => {
                let v = self.r_abs(bus);
                self.ora(v);
            }
            0x1D => {
                let v = self.r_abx(bus);
                self.ora(v);
            }
            0x19 => {
                let v = self.r_aby(bus);
                self.ora(v);
            }
            0x01 => {
                let v = self.r_izx(bus);
                self.ora(v);
            }
            0x11 => {
                let v = self.r_izy(bus);
                self.ora(v);
            }

            // --------------------------------------------------------- AND
            0x29 => {
                let v = self.fetch(bus);
                self.and(v);
            }
            0x25 => {
                let v = self.r_zp(bus);
                self.and(v);
            }
            0x35 => {
                let v = self.r_zpx(bus);
                self.and(v);
            }
            0x2D => {
                let v = self.r_abs(bus);
                self.and(v);
            }
            0x3D => {
                let v = self.r_abx(bus);
                self.and(v);
            }
            0x39 => {
                let v = self.r_aby(bus);
                self.and(v);
            }
            0x21 => {
                let v = self.r_izx(bus);
                self.and(v);
            }
            0x31 => {
                let v = self.r_izy(bus);
                self.and(v);
            }

            // --------------------------------------------------------- EOR
            0x49 => {
                let v = self.fetch(bus);
                self.eor(v);
            }
            0x45 => {
                let v = self.r_zp(bus);
                self.eor(v);
            }
            0x55 => {
                let v = self.r_zpx(bus);
                self.eor(v);
            }
            0x4D => {
                let v = self.r_abs(bus);
                self.eor(v);
            }
            0x5D => {
                let v = self.r_abx(bus);
                self.eor(v);
            }
            0x59 => {
                let v = self.r_aby(bus);
                self.eor(v);
            }
            0x41 => {
                let v = self.r_izx(bus);
                self.eor(v);
            }
            0x51 => {
                let v = self.r_izy(bus);
                self.eor(v);
            }

            // --------------------------------------------------------- ADC
            0x69 => {
                let v = self.fetch(bus);
                self.adc(v);
            }
            0x65 => {
                let v = self.r_zp(bus);
                self.adc(v);
            }
            0x75 => {
                let v = self.r_zpx(bus);
                self.adc(v);
            }
            0x6D => {
                let v = self.r_abs(bus);
                self.adc(v);
            }
            0x7D => {
                let v = self.r_abx(bus);
                self.adc(v);
            }
            0x79 => {
                let v = self.r_aby(bus);
                self.adc(v);
            }
            0x61 => {
                let v = self.r_izx(bus);
                self.adc(v);
            }
            0x71 => {
                let v = self.r_izy(bus);
                self.adc(v);
            }

            // --------------------------------------------------------- SBC
            0xE9 | 0xEB => {
                let v = self.fetch(bus);
                self.sbc(v);
            }
            0xE5 => {
                let v = self.r_zp(bus);
                self.sbc(v);
            }
            0xF5 => {
                let v = self.r_zpx(bus);
                self.sbc(v);
            }
            0xED => {
                let v = self.r_abs(bus);
                self.sbc(v);
            }
            0xFD => {
                let v = self.r_abx(bus);
                self.sbc(v);
            }
            0xF9 => {
                let v = self.r_aby(bus);
                self.sbc(v);
            }
            0xE1 => {
                let v = self.r_izx(bus);
                self.sbc(v);
            }
            0xF1 => {
                let v = self.r_izy(bus);
                self.sbc(v);
            }

            // ---------------------------------------------------- compares
            0xC9 => {
                let v = self.fetch(bus);
                self.compare(self.a, v);
            }
            0xC5 => {
                let v = self.r_zp(bus);
                self.compare(self.a, v);
            }
            0xD5 => {
                let v = self.r_zpx(bus);
                self.compare(self.a, v);
            }
            0xCD => {
                let v = self.r_abs(bus);
                self.compare(self.a, v);
            }
            0xDD => {
                let v = self.r_abx(bus);
                self.compare(self.a, v);
            }
            0xD9 => {
                let v = self.r_aby(bus);
                self.compare(self.a, v);
            }
            0xC1 => {
                let v = self.r_izx(bus);
                self.compare(self.a, v);
            }
            0xD1 => {
                let v = self.r_izy(bus);
                self.compare(self.a, v);
            }
            0xE0 => {
                let v = self.fetch(bus);
                self.compare(self.x, v);
            }
            0xE4 => {
                let v = self.r_zp(bus);
                self.compare(self.x, v);
            }
            0xEC => {
                let v = self.r_abs(bus);
                self.compare(self.x, v);
            }
            0xC0 => {
                let v = self.fetch(bus);
                self.compare(self.y, v);
            }
            0xC4 => {
                let v = self.r_zp(bus);
                self.compare(self.y, v);
            }
            0xCC => {
                let v = self.r_abs(bus);
                self.compare(self.y, v);
            }

            // --------------------------------------------------------- BIT
            0x24 => {
                let v = self.r_zp(bus);
                self.bit(v);
            }
            0x2C => {
                let v = self.r_abs(bus);
                self.bit(v);
            }

            // --------------------------------------------------------- LDA
            0xA9 => {
                let v = self.fetch(bus);
                self.lda(v);
            }
            0xA5 => {
                let v = self.r_zp(bus);
                self.lda(v);
            }
            0xB5 => {
                let v = self.r_zpx(bus);
                self.lda(v);
            }
            0xAD => {
                let v = self.r_abs(bus);
                self.lda(v);
            }
            0xBD => {
                let v = self.r_abx(bus);
                self.lda(v);
            }
            0xB9 => {
                let v = self.r_aby(bus);
                self.lda(v);
            }
            0xA1 => {
                let v = self.r_izx(bus);
                self.lda(v);
            }
            0xB1 => {
                let v = self.r_izy(bus);
                self.lda(v);
            }

            // --------------------------------------------------------- LDX
            0xA2 => {
                let v = self.fetch(bus);
                self.ldx(v);
            }
            0xA6 => {
                let v = self.r_zp(bus);
                self.ldx(v);
            }
            0xB6 => {
                let v = self.r_zpy(bus);
                self.ldx(v);
            }
            0xAE => {
                let v = self.r_abs(bus);
                self.ldx(v);
            }
            0xBE => {
                let v = self.r_aby(bus);
                self.ldx(v);
            }

            // --------------------------------------------------------- LDY
            0xA0 => {
                let v = self.fetch(bus);
                self.ldy(v);
            }
            0xA4 => {
                let v = self.r_zp(bus);
                self.ldy(v);
            }
            0xB4 => {
                let v = self.r_zpx(bus);
                self.ldy(v);
            }
            0xAC => {
                let v = self.r_abs(bus);
                self.ldy(v);
            }
            0xBC => {
                let v = self.r_abx(bus);
                self.ldy(v);
            }

            // ------------------------------------------------------- stores
            0x85 => {
                let a = self.am_zp(bus);
                self.wr(bus, a, self.a);
            }
            0x95 => {
                let a = self.am_zpx(bus);
                self.wr(bus, a, self.a);
            }
            0x8D => {
                let a = self.fetch16(bus);
                self.wr(bus, a, self.a);
            }
            0x9D => {
                let a = self.am_abi(bus, self.x, true);
                self.wr(bus, a, self.a);
            }
            0x99 => {
                let a = self.am_abi(bus, self.y, true);
                self.wr(bus, a, self.a);
            }
            0x81 => {
                let a = self.am_izx(bus);
                self.wr(bus, a, self.a);
            }
            0x91 => {
                let a = self.am_izy(bus, true);
                self.wr(bus, a, self.a);
            }
            0x86 => {
                let a = self.am_zp(bus);
                self.wr(bus, a, self.x);
            }
            0x96 => {
                let a = self.am_zpy(bus);
                self.wr(bus, a, self.x);
            }
            0x8E => {
                let a = self.fetch16(bus);
                self.wr(bus, a, self.x);
            }
            0x84 => {
                let a = self.am_zp(bus);
                self.wr(bus, a, self.y);
            }
            0x94 => {
                let a = self.am_zpx(bus);
                self.wr(bus, a, self.y);
            }
            0x8C => {
                let a = self.fetch16(bus);
                self.wr(bus, a, self.y);
            }

            // ----------------------------------------------- shifts on A
            0x0A => {
                self.idle(bus);
                let v = self.a;
                let r = self.asl(v);
                self.a = r;
            }
            0x4A => {
                self.idle(bus);
                let v = self.a;
                let r = self.lsr(v);
                self.a = r;
            }
            0x2A => {
                self.idle(bus);
                let v = self.a;
                let r = self.rol(v);
                self.a = r;
            }
            0x6A => {
                self.idle(bus);
                let v = self.a;
                let r = self.ror(v);
                self.a = r;
            }

            // ------------------------------------------------------- ASL m
            0x06 => {
                let a = self.am_zp(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.asl(v);
                self.wr(bus, a, r);
            }
            0x16 => {
                let a = self.am_zpx(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.asl(v);
                self.wr(bus, a, r);
            }
            0x0E => {
                let a = self.fetch16(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.asl(v);
                self.wr(bus, a, r);
            }
            0x1E => {
                let a = self.am_abi(bus, self.x, true);
                let v = self.rmw_begin(bus, a);
                let r = self.asl(v);
                self.wr(bus, a, r);
            }

            // ------------------------------------------------------- LSR m
            0x46 => {
                let a = self.am_zp(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.lsr(v);
                self.wr(bus, a, r);
            }
            0x56 => {
                let a = self.am_zpx(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.lsr(v);
                self.wr(bus, a, r);
            }
            0x4E => {
                let a = self.fetch16(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.lsr(v);
                self.wr(bus, a, r);
            }
            0x5E => {
                let a = self.am_abi(bus, self.x, true);
                let v = self.rmw_begin(bus, a);
                let r = self.lsr(v);
                self.wr(bus, a, r);
            }

            // ------------------------------------------------------- ROL m
            0x26 => {
                let a = self.am_zp(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.rol(v);
                self.wr(bus, a, r);
            }
            0x36 => {
                let a = self.am_zpx(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.rol(v);
                self.wr(bus, a, r);
            }
            0x2E => {
                let a = self.fetch16(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.rol(v);
                self.wr(bus, a, r);
            }
            0x3E => {
                let a = self.am_abi(bus, self.x, true);
                let v = self.rmw_begin(bus, a);
                let r = self.rol(v);
                self.wr(bus, a, r);
            }

            // ------------------------------------------------------- ROR m
            0x66 => {
                let a = self.am_zp(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.ror(v);
                self.wr(bus, a, r);
            }
            0x76 => {
                let a = self.am_zpx(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.ror(v);
                self.wr(bus, a, r);
            }
            0x6E => {
                let a = self.fetch16(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.ror(v);
                self.wr(bus, a, r);
            }
            0x7E => {
                let a = self.am_abi(bus, self.x, true);
                let v = self.rmw_begin(bus, a);
                let r = self.ror(v);
                self.wr(bus, a, r);
            }

            // ------------------------------------------------------- INC m
            0xE6 => {
                let a = self.am_zp(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.inc(v);
                self.wr(bus, a, r);
            }
            0xF6 => {
                let a = self.am_zpx(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.inc(v);
                self.wr(bus, a, r);
            }
            0xEE => {
                let a = self.fetch16(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.inc(v);
                self.wr(bus, a, r);
            }
            0xFE => {
                let a = self.am_abi(bus, self.x, true);
                let v = self.rmw_begin(bus, a);
                let r = self.inc(v);
                self.wr(bus, a, r);
            }

            // ------------------------------------------------------- DEC m
            0xC6 => {
                let a = self.am_zp(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.dec(v);
                self.wr(bus, a, r);
            }
            0xD6 => {
                let a = self.am_zpx(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.dec(v);
                self.wr(bus, a, r);
            }
            0xCE => {
                let a = self.fetch16(bus);
                let v = self.rmw_begin(bus, a);
                let r = self.dec(v);
                self.wr(bus, a, r);
            }
            0xDE => {
                let a = self.am_abi(bus, self.x, true);
                let v = self.rmw_begin(bus, a);
                let r = self.dec(v);
                self.wr(bus, a, r);
            }

            // ------------------------------------------------ unofficial NOPs
            0x80 | 0x82 | 0x89 | 0xC2 | 0xE2 => {
                self.fetch(bus);
            }
            0x04 | 0x44 | 0x64 => {
                self.r_zp(bus);
            }
            0x14 | 0x34 | 0x54 | 0x74 | 0xD4 | 0xF4 => {
                self.r_zpx(bus);
            }
            0x0C => {
                self.r_abs(bus);
            }
            0x1C | 0x3C | 0x5C | 0x7C | 0xDC | 0xFC => {
                self.r_abx(bus);
            }

            // --------------------------------------------------------- LAX
            0xA7 => {
                let v = self.r_zp(bus);
                self.lax(v);
            }
            0xB7 => {
                let v = self.r_zpy(bus);
                self.lax(v);
            }
            0xAF => {
                let v = self.r_abs(bus);
                self.lax(v);
            }
            0xBF => {
                let v = self.r_aby(bus);
                self.lax(v);
            }
            0xA3 => {
                let v = self.r_izx(bus);
                self.lax(v);
            }
            0xB3 => {
                let v = self.r_izy(bus);
                self.lax(v);
            }
            0xAB => {
                // Unstable LAX #imm: A = X = (A | magic) & imm.
                let imm = self.fetch(bus);
                let v = (self.a | MAGIC) & imm;
                self.lax(v);
            }

            // --------------------------------------------------- SAX (AXS)
            0x87 => {
                let a = self.am_zp(bus);
                self.wr(bus, a, self.a & self.x);
            }
            0x97 => {
                let a = self.am_zpy(bus);
                self.wr(bus, a, self.a & self.x);
            }
            0x8F => {
                let a = self.fetch16(bus);
                self.wr(bus, a, self.a & self.x);
            }
            0x83 => {
                let a = self.am_izx(bus);
                self.wr(bus, a, self.a & self.x);
            }

            // --------------------------------------------------------- DCP
            0xC7 | 0xD7 | 0xCF | 0xDF | 0xDB | 0xC3 | 0xD3 => {
                let a = self.rmw_addr(bus, op);
                let v = self.rmw_begin(bus, a);
                let r = v.wrapping_sub(1);
                self.wr(bus, a, r);
                self.compare(self.a, r);
            }

            // --------------------------------------------------------- ISC
            0xE7 | 0xF7 | 0xEF | 0xFF | 0xFB | 0xE3 | 0xF3 => {
                let a = self.rmw_addr(bus, op);
                let v = self.rmw_begin(bus, a);
                let r = v.wrapping_add(1);
                self.wr(bus, a, r);
                self.sbc(r);
            }

            // --------------------------------------------------------- SLO
            0x07 | 0x17 | 0x0F | 0x1F | 0x1B | 0x03 | 0x13 => {
                let a = self.rmw_addr(bus, op);
                let v = self.rmw_begin(bus, a);
                let r = self.asl(v);
                self.wr(bus, a, r);
                self.ora(r);
            }

            // --------------------------------------------------------- RLA
            0x27 | 0x37 | 0x2F | 0x3F | 0x3B | 0x23 | 0x33 => {
                let a = self.rmw_addr(bus, op);
                let v = self.rmw_begin(bus, a);
                let r = self.rol(v);
                self.wr(bus, a, r);
                self.and(r);
            }

            // --------------------------------------------------------- SRE
            0x47 | 0x57 | 0x4F | 0x5F | 0x5B | 0x43 | 0x53 => {
                let a = self.rmw_addr(bus, op);
                let v = self.rmw_begin(bus, a);
                let r = self.lsr(v);
                self.wr(bus, a, r);
                self.eor(r);
            }

            // --------------------------------------------------------- RRA
            0x67 | 0x77 | 0x6F | 0x7F | 0x7B | 0x63 | 0x73 => {
                let a = self.rmw_addr(bus, op);
                let v = self.rmw_begin(bus, a);
                let r = self.ror(v);
                self.wr(bus, a, r);
                self.adc(r);
            }

            // ------------------------------------------- immediate oddballs
            0x0B | 0x2B => {
                // ANC: AND #imm, then copy N into C.
                let imm = self.fetch(bus);
                self.and(imm);
                let n = (self.p & FLAG_N) != 0;
                self.set_flag(FLAG_C, n);
            }
            0x4B => {
                // ALR: AND #imm, then LSR A.
                let imm = self.fetch(bus);
                let v = self.a & imm;
                let r = self.lsr(v);
                self.a = r;
            }
            0x6B => {
                // ARR: AND #imm, ROR A, then the odd C/V rules.
                let imm = self.fetch(bus);
                let v = self.a & imm;
                let r = (v >> 1) | (self.carry() << 7);
                self.a = r;
                self.set_zn(r);
                self.set_flag(FLAG_C, (r & 0x40) != 0);
                self.set_flag(FLAG_V, ((r >> 6) ^ (r >> 5)) & 1 != 0);
            }
            0xCB => {
                // AXS / SBX: X = (A & X) - imm, C as in CMP.
                let imm = self.fetch(bus);
                let v = self.a & self.x;
                let r = v.wrapping_sub(imm);
                self.set_flag(FLAG_C, v >= imm);
                self.x = r;
                self.set_zn(r);
            }
            0x8B => {
                // XAA: A = (A | magic) & X & imm.
                let imm = self.fetch(bus);
                let r = (self.a | MAGIC) & self.x & imm;
                self.lda(r);
            }

            // ---------------------------------------- unstable high-byte stores
            0x9C => {
                // SHY abs,X
                let base = self.fetch16(bus);
                let eff = base.wrapping_add(u16::from(self.x));
                self.rd(bus, (base & 0xFF00) | (eff & 0x00FF));
                let v = self.y & ((base >> 8) as u8).wrapping_add(1);
                self.wr(bus, eff, v);
            }
            0x9E => {
                // SHX abs,Y
                let base = self.fetch16(bus);
                let eff = base.wrapping_add(u16::from(self.y));
                self.rd(bus, (base & 0xFF00) | (eff & 0x00FF));
                let v = self.x & ((base >> 8) as u8).wrapping_add(1);
                self.wr(bus, eff, v);
            }
            0x9F => {
                // SHA abs,Y
                let base = self.fetch16(bus);
                let eff = base.wrapping_add(u16::from(self.y));
                self.rd(bus, (base & 0xFF00) | (eff & 0x00FF));
                let v = self.a & self.x & ((base >> 8) as u8).wrapping_add(1);
                self.wr(bus, eff, v);
            }
            0x93 => {
                // SHA (zp),Y
                let b = self.fetch(bus);
                let lo = self.rd(bus, u16::from(b));
                let hi = self.rd(bus, u16::from(b.wrapping_add(1)));
                let base = u16::from(lo) | (u16::from(hi) << 8);
                let eff = base.wrapping_add(u16::from(self.y));
                self.rd(bus, (base & 0xFF00) | (eff & 0x00FF));
                let v = self.a & self.x & ((base >> 8) as u8).wrapping_add(1);
                self.wr(bus, eff, v);
            }
            0x9B => {
                // TAS abs,Y: SP = A & X, store SP & (high byte + 1).
                let base = self.fetch16(bus);
                let eff = base.wrapping_add(u16::from(self.y));
                self.rd(bus, (base & 0xFF00) | (eff & 0x00FF));
                self.sp = self.a & self.x;
                let v = self.sp & ((base >> 8) as u8).wrapping_add(1);
                self.wr(bus, eff, v);
            }
            0xBB => {
                // LAS abs,Y: A = X = SP = mem & SP.
                let v = self.r_aby(bus);
                let r = v & self.sp;
                self.sp = r;
                self.lax(r);
            }

            // -------------------------------------------------------- KIL
            0x02 | 0x12 | 0x22 | 0x32 | 0x42 | 0x52 | 0x62 | 0x72 | 0x92 | 0xB2 | 0xD2
            | 0xF2 => {
                self.jammed = true;
                self.pc = self.pc.wrapping_sub(1);
                self.idle(bus);
                return;
            }
        }

        let (nmi, irq) =
            if delay_poll { (self.sig_nmi_prev, self.sig_irq_prev) } else { (self.sig_nmi, self.sig_irq) };
        if nmi {
            self.interrupt(bus, true);
        } else if irq {
            self.interrupt(bus, false);
        }
    }

    /// Effective address for the read-modify-write unofficial opcodes, which share the low
    /// nibble pattern of their official RMW counterparts.
    #[inline(always)]
    fn rmw_addr<B: CpuBus>(&mut self, bus: &mut B, op: u8) -> u16 {
        match op & 0x1F {
            0x07 => self.am_zp(bus),
            0x17 => self.am_zpx(bus),
            0x0F => self.fetch16(bus),
            0x1F => self.am_abi(bus, self.x, true),
            0x1B => self.am_abi(bus, self.y, true),
            0x03 => self.am_izx(bus),
            0x13 => self.am_izy(bus, true),
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
#[path = "cpu_tests.rs"]
mod tests;
