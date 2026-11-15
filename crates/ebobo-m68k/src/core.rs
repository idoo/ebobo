//! The framework of the 68000: what the instruction files (`line0.rs`, `moves.rs`, `line4.rs`, `flow.rs`, `arith.rs`,
//! `logic.rs`, `shift.rs`) are written with. Each of them is one `exec(cpu, bus, op) -> Res<()>`, called by `Cpu::step`
//! with the opcode already fetched: `cpu.pc` is past it, `cpu.op_pc` is its address.
//!
//! The helpers, all `pub(crate)` (`cpu.` ones are methods of `Cpu`):
//!
//! Decoding
//! - `reg(op)`, `mode(op)`, `reg9(op)`: the register field (bits 2-0), the mode field (bits 5-3), the second register
//!   field (bits 11-9) of an opcode, as `u8`.
//! - `Size::{Byte, Word, Long}`; `Size::of(op)`: the usual size field in bits 7-6, `None` for the value 3.
//! - `size.bytes()` 1/2/4, `size.bits()` 8/16/32, `size.mask()` all its bits, `size.msb()` its sign bit.
//! - `size.sext(v)`: `v` of that size sign-extended to 32 bits.
//!
//! Registers and flags
//! - `cpu.d(n, size)`: Dn cut to `size`. `cpu.set_d(n, size, v)`: the low byte/word/long of Dn, the rest kept.
//! - `cpu.a(n)`, `cpu.set_a(n, v)`: An, whole; n = 7 is the active stack pointer (`ssp` in supervisor mode, else `usp`).
//! - `cpu.set_sr(v)`: the whole SR (bits that do not exist read 0); A7 follows the S bit by itself.
//! - `cpu.ccr()`, `cpu.set_ccr(v)`: the low five bits of SR (X N Z V C).
//! - `cpu.flag(bit)`, `cpu.set_flag(bit, on)`: one or more `sr::*` bits.
//! - `cpu.cond(cc)`: the condition `cc & 15` of Bcc/DBcc/Scc on the current flags.
//! - `cpu.nz(v, size)`: N and Z from `v`, V and C cleared, X kept (MOVE, AND, OR, EOR, NOT, TST, ...).
//! - `cpu.add_flags(src, dst, res, size)`: X N Z V C of `res = dst + src`.
//! - `cpu.sub_flags(src, dst, res, size)`: X N Z V C of `res = dst - src`.
//! - `cpu.cmp_flags(src, dst, res, size)`: N Z V C of `res = dst - src`, X kept.
//!
//! Memory (addresses are cut to 24 bits; a word or long at an odd address is `Err(Fault::Address)` before the bus)
//! - `cpu.fetch16(bus)`, `cpu.fetch32(bus)`: the word/long at `pc`, `pc` advanced.
//! - `cpu.read(bus, addr, size)`, `cpu.write(bus, addr, size, val)`: a long is two words, high word first.
//! - `cpu.push16/push32(bus, val)`, `cpu.pop16/pop32(bus)`: on the active stack.
//!
//! Operands
//! - `Ea::{Dn(n), An(n), Mem(addr), Imm(val)}`: a resolved operand; resolve once, then `load` and `store` it.
//! - `cpu.ea(bus, mode, reg, size)`: any of the 12 addressing modes: fetches the extension words, steps (An)+ / -(An)
//!   (a byte on A7 by 2), charges the 2 cycles of -(An) and of the indexed modes. Unknown mode: `Err(Fault::Illegal)`.
//! - `cpu.ea_plain(bus, mode, reg, size)`: the same without the 2 cycles of -(An): MOVE's destination and the second
//!   operand of ADDX/SUBX/ABCD/SBCD -(Ay),-(Ax).
//! - `cpu.ea_addr(bus, mode, reg)`: the address of a control mode (LEA, PEA, JMP, JSR, MOVEM); charges the 2 cycles of
//!   the indexed modes; any other mode is `Err(Fault::Illegal)`.
//! - `cpu.load(bus, ea, size)`: the operand's value, cut to `size`.
//! - `cpu.store(bus, ea, size, val)`: Dn keeps its upper bits; An takes a word sign-extended; `Imm` is illegal.
//!
//! Time and exceptions
//! - `cpu.cyc`: cycles of this step so far. Every byte, word or half of a long on the bus adds 4 by itself, in
//!   `fetch16`, `read` and `write` (the opcode fetch included: NOP idles nothing and costs 4).
//! - `cpu.idle(n)`: everything else. n = the total of the manual's timing table minus what the bus accesses and `ea`
//!   already charged (JMP (An) is 8 in the table and reads only its opcode: `idle(4)`).
//! - `cpu.exception(bus, vector)`: pushes `pc` and SR, S on, T off, `pc` from the vector table: 20 cycles of bus, idle
//!   the rest (TRAP, TRAPV, CHK, divide by zero: `pc` is already the next instruction, which is what they push).
//! - `Fault`, `Res<T>`: pass errors up with `?`; `step` turns them into the exception. An encoding that does not exist
//!   is `return Err(Fault::Illegal)`, a privileged instruction in user mode `return Err(Fault::Privilege)`, both before
//!   anything is changed: `step` pushes `op_pc` and makes the whole step 34 cycles.

use crate::{Bus, Cpu, sr};

/// The address bus is 24 bits wide.
const ADDR_MASK: u32 = 0x00FF_FFFF;
/// The bits of SR that exist on the 68000: T, S, the interrupt mask and the five flags.
const SR_MASK: u16 = 0xA71F;

/// Operand size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Size {
    Byte,
    Word,
    Long,
}

impl Size {
    /// The usual size field of an opcode, bits 7-6: 0 byte, 1 word, 2 long; 3 is `None` (another instruction).
    pub(crate) fn of(op: u16) -> Option<Size> {
        match (op >> 6) & 3 {
            0 => Some(Size::Byte),
            1 => Some(Size::Word),
            2 => Some(Size::Long),
            _ => None,
        }
    }

    /// Bytes in memory: 1, 2 or 4.
    pub(crate) fn bytes(self) -> u32 {
        match self {
            Size::Byte => 1,
            Size::Word => 2,
            Size::Long => 4,
        }
    }

    /// Width in bits: 8, 16 or 32.
    pub(crate) fn bits(self) -> u32 {
        self.bytes() * 8
    }

    /// All bits of the size set.
    pub(crate) fn mask(self) -> u32 {
        match self {
            Size::Byte => 0xFF,
            Size::Word => 0xFFFF,
            Size::Long => 0xFFFF_FFFF,
        }
    }

    /// The sign bit of the size.
    pub(crate) fn msb(self) -> u32 {
        match self {
            Size::Byte => 0x80,
            Size::Word => 0x8000,
            Size::Long => 0x8000_0000,
        }
    }

    /// `v` of this size sign-extended to 32 bits (upper bits of `v` are ignored).
    pub(crate) fn sext(self, v: u32) -> u32 {
        match self {
            Size::Byte => v as u8 as i8 as i32 as u32,
            Size::Word => v as u16 as i16 as i32 as u32,
            Size::Long => v,
        }
    }
}

/// Why an instruction stopped; `Cpu::step` turns it into the exception.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// Address error (vector 3): a word or long access at the odd address `addr` (all 32 bits, as the frame shows
    /// them). `write`: a write cycle. `program`: an opcode or extension word fetch (program space), else data.
    Address { addr: u32, write: bool, program: bool },
    /// The opcode is not an instruction (vector 4).
    Illegal,
    /// A privileged instruction in user mode (vector 8).
    Privilege,
    /// An opcode of line A (vector 10).
    LineA,
    /// An opcode of line F (vector 11).
    LineF,
}

pub(crate) type Res<T> = Result<T, Fault>;

/// A resolved operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ea {
    /// Data register 0-7.
    Dn(u8),
    /// Address register 0-7.
    An(u8),
    /// Memory at this address (32 bits as computed; the bus sees the low 24).
    Mem(u32),
    /// Immediate value, already fetched.
    Imm(u32),
}

/// The register field of an opcode, bits 2-0.
pub(crate) fn reg(op: u16) -> u8 {
    (op & 7) as u8
}

/// The addressing mode field of an opcode, bits 5-3.
pub(crate) fn mode(op: u16) -> u8 {
    ((op >> 3) & 7) as u8
}

/// The second register field of an opcode, bits 11-9.
pub(crate) fn reg9(op: u16) -> u8 {
    ((op >> 9) & 7) as u8
}

/// A word or long access needs an even address.
fn even(addr: u32, write: bool) -> Res<()> {
    if addr & 1 == 0 { Ok(()) } else { Err(Fault::Address { addr, write, program: false }) }
}

/// What (An)+ and -(An) step by: the operand size, but 2 for a byte on A7, which stays even.
fn an_step(reg: u8, size: Size) -> u32 {
    if reg == 7 && size == Size::Byte { 2 } else { size.bytes() }
}

impl Cpu {
    // ---- registers ----

    /// Dn cut to `size`.
    pub(crate) fn d(&self, n: u8, size: Size) -> u32 {
        self.d[usize::from(n)] & size.mask()
    }

    /// Writes the low byte, word or long of Dn; the bits above stay.
    pub(crate) fn set_d(&mut self, n: u8, size: Size, v: u32) {
        let d = &mut self.d[usize::from(n)];
        *d = (*d & !size.mask()) | (v & size.mask());
    }

    /// An, all 32 bits. A7 is `ssp` in supervisor mode and `usp` in user mode.
    pub(crate) fn a(&self, n: u8) -> u32 {
        match n {
            7 if self.sr & sr::S != 0 => self.ssp,
            7 => self.usp,
            _ => self.a[usize::from(n)],
        }
    }

    /// Writes An, all 32 bits; A7 is the active stack pointer.
    pub(crate) fn set_a(&mut self, n: u8, v: u32) {
        match n {
            7 if self.sr & sr::S != 0 => self.ssp = v,
            7 => self.usp = v,
            _ => self.a[usize::from(n)] = v,
        }
    }

    /// Writes the whole status register; bits that do not exist stay 0. A change of S changes which of `ssp` and
    /// `usp` is A7; both are kept apart, so nothing is copied.
    pub(crate) fn set_sr(&mut self, v: u16) {
        self.sr = v & SR_MASK;
    }

    /// The condition codes: X N Z V C in bits 4-0.
    pub(crate) fn ccr(&self) -> u16 {
        self.sr & 0x1F
    }

    /// Writes the condition codes from bits 4-0 of `v`; the system byte stays.
    pub(crate) fn set_ccr(&mut self, v: u16) {
        self.sr = (self.sr & 0xFF00) | (v & 0x1F);
    }

    /// Whether any of the `sr::*` bits in `bit` is set.
    pub(crate) fn flag(&self, bit: u16) -> bool {
        self.sr & bit != 0
    }

    /// Sets or clears the `sr::*` bits in `bit`.
    pub(crate) fn set_flag(&mut self, bit: u16, on: bool) {
        if on {
            self.sr |= bit;
        } else {
            self.sr &= !bit;
        }
    }

    /// The condition `cc & 15` of Bcc, DBcc and Scc: T F HI LS CC CS NE EQ VC VS PL MI GE LT GT LE.
    pub(crate) fn cond(&self, cc: u16) -> bool {
        let (c, v, z, n) = (self.flag(sr::C), self.flag(sr::V), self.flag(sr::Z), self.flag(sr::N));
        let even_case = match (cc >> 1) & 7 {
            0 => true,
            1 => !c && !z,
            2 => !c,
            3 => !z,
            4 => !v,
            5 => !n,
            6 => n == v,
            _ => !z && n == v,
        };
        // Each odd condition is the opposite of the even one before it.
        even_case != (cc & 1 != 0)
    }

    // ---- flags ----

    /// N and Z from `v` (of `size`), V and C cleared, X untouched: the flags of MOVE and the logic instructions.
    pub(crate) fn nz(&mut self, v: u32, size: Size) {
        self.result_flags(v, 0, 0, size, false);
    }

    /// X N Z V C of the addition `res = dst + src` (`res` may carry bits above the size; they are ignored).
    pub(crate) fn add_flags(&mut self, src: u32, dst: u32, res: u32, size: Size) {
        let carry = (src & dst) | (!res & (src | dst));
        let overflow = (src ^ res) & (dst ^ res);
        self.result_flags(res, carry, overflow, size, true);
    }

    /// X N Z V C of the subtraction `res = dst - src`.
    pub(crate) fn sub_flags(&mut self, src: u32, dst: u32, res: u32, size: Size) {
        self.borrow_flags(src, dst, res, size, true);
    }

    /// N Z V C of the comparison `res = dst - src`; X untouched.
    pub(crate) fn cmp_flags(&mut self, src: u32, dst: u32, res: u32, size: Size) {
        self.borrow_flags(src, dst, res, size, false);
    }

    fn borrow_flags(&mut self, src: u32, dst: u32, res: u32, size: Size, x: bool) {
        let borrow = (src & !dst) | (res & (src | !dst));
        let overflow = (src ^ dst) & (res ^ dst);
        self.result_flags(res, borrow, overflow, size, x);
    }

    /// N Z from `res`; V and C from the sign bit of `overflow` and `carry`; X = C when `x`.
    fn result_flags(&mut self, res: u32, carry: u32, overflow: u32, size: Size, x: bool) {
        let msb = size.msb();
        let mut f = self.sr & !(sr::N | sr::Z | sr::V | sr::C);
        if x {
            f &= !sr::X;
        }
        if res & msb != 0 {
            f |= sr::N;
        }
        if res & size.mask() == 0 {
            f |= sr::Z;
        }
        if overflow & msb != 0 {
            f |= sr::V;
        }
        if carry & msb != 0 {
            f |= if x { sr::C | sr::X } else { sr::C };
        }
        self.sr = f;
    }

    // ---- time ----

    /// `cycles` clock cycles without a bus access.
    pub(crate) fn idle(&mut self, cycles: u32) {
        self.cyc += cycles;
    }

    // ---- memory ----

    /// The word at `pc`, `pc` advanced by 2. 4 cycles. An odd `pc` is an address error in program space.
    pub(crate) fn fetch16(&mut self, bus: &mut impl Bus) -> Res<u16> {
        if self.pc & 1 != 0 {
            return Err(Fault::Address { addr: self.pc, write: false, program: true });
        }
        self.cyc += 4;
        let word = bus.read16(self.pc & ADDR_MASK);
        self.pc = self.pc.wrapping_add(2);
        Ok(word)
    }

    /// The long at `pc`, high word first, `pc` advanced by 4. 8 cycles.
    pub(crate) fn fetch32(&mut self, bus: &mut impl Bus) -> Res<u32> {
        let hi = self.fetch16(bus)?;
        let lo = self.fetch16(bus)?;
        Ok((u32::from(hi) << 16) | u32::from(lo))
    }

    /// Reads memory. 4 cycles for a byte or a word, 8 for a long (two words, high first). A word or long at an odd
    /// address is an address error and the bus is not touched.
    pub(crate) fn read(&mut self, bus: &mut impl Bus, addr: u32, size: Size) -> Res<u32> {
        match size {
            Size::Byte => {
                self.cyc += 4;
                Ok(u32::from(bus.read8(addr & ADDR_MASK)))
            }
            Size::Word => {
                even(addr, false)?;
                self.cyc += 4;
                Ok(u32::from(bus.read16(addr & ADDR_MASK)))
            }
            Size::Long => {
                even(addr, false)?;
                self.cyc += 8;
                let hi = bus.read16(addr & ADDR_MASK);
                let lo = bus.read16(addr.wrapping_add(2) & ADDR_MASK);
                Ok((u32::from(hi) << 16) | u32::from(lo))
            }
        }
    }

    /// Writes the low `size` of `val` to memory, with the cycles and the address error of [`Cpu::read`].
    pub(crate) fn write(&mut self, bus: &mut impl Bus, addr: u32, size: Size, val: u32) -> Res<()> {
        match size {
            Size::Byte => {
                self.cyc += 4;
                bus.write8(addr & ADDR_MASK, val as u8);
            }
            Size::Word => {
                even(addr, true)?;
                self.cyc += 4;
                bus.write16(addr & ADDR_MASK, val as u16);
            }
            Size::Long => {
                even(addr, true)?;
                self.cyc += 8;
                bus.write16(addr & ADDR_MASK, (val >> 16) as u16);
                bus.write16(addr.wrapping_add(2) & ADDR_MASK, val as u16);
            }
        }
        Ok(())
    }

    /// Pushes a word on the active stack: A7 down by 2, then the write.
    pub(crate) fn push16(&mut self, bus: &mut impl Bus, val: u16) -> Res<()> {
        let sp = self.a(7).wrapping_sub(2);
        self.set_a(7, sp);
        self.write(bus, sp, Size::Word, u32::from(val))
    }

    /// Pushes a long on the active stack: A7 down by 4, then the write.
    pub(crate) fn push32(&mut self, bus: &mut impl Bus, val: u32) -> Res<()> {
        let sp = self.a(7).wrapping_sub(4);
        self.set_a(7, sp);
        self.write(bus, sp, Size::Long, val)
    }

    /// Pops a word from the active stack.
    pub(crate) fn pop16(&mut self, bus: &mut impl Bus) -> Res<u16> {
        let sp = self.a(7);
        let val = self.read(bus, sp, Size::Word)?;
        self.set_a(7, sp.wrapping_add(2));
        Ok(val as u16)
    }

    /// Pops a long from the active stack.
    pub(crate) fn pop32(&mut self, bus: &mut impl Bus) -> Res<u32> {
        let sp = self.a(7);
        let val = self.read(bus, sp, Size::Long)?;
        self.set_a(7, sp.wrapping_add(4));
        Ok(val)
    }

    // ---- operands ----

    /// Resolves the operand `mode`/`reg` (the two 3-bit fields of an opcode) for an access of `size`:
    /// 0 Dn, 1 An, 2 (An), 3 (An)+, 4 -(An), 5 d16(An), 6 d8(An,Xn), 7/0 abs.W, 7/1 abs.L, 7/2 d16(PC),
    /// 7/3 d8(PC,Xn), 7/4 #imm. Fetches the extension words (4 cycles each), steps An for (An)+ and -(An) by the
    /// size (a byte on A7 by 2) and adds the internal cycles of the effective address table: 2 for -(An), 2 for the
    /// two indexed modes. Reads no operand. Any other mode is `Err(Fault::Illegal)`.
    pub(crate) fn ea(&mut self, bus: &mut impl Bus, mode: u8, reg: u8, size: Size) -> Res<Ea> {
        self.operand(bus, mode, reg, size, true)
    }

    /// [`Cpu::ea`] without the 2 cycles of -(An): the destination of MOVE and the second operand of ADDX, SUBX, ABCD
    /// and SBCD -(Ay),-(Ax), whose tables do not have them.
    pub(crate) fn ea_plain(&mut self, bus: &mut impl Bus, mode: u8, reg: u8, size: Size) -> Res<Ea> {
        self.operand(bus, mode, reg, size, false)
    }

    fn operand(&mut self, bus: &mut impl Bus, mode: u8, reg: u8, size: Size, predecrement_cost: bool) -> Res<Ea> {
        // What an address error of this operand's access needs: see `Cpu::address_error`.
        (self.ae_pc, self.ae_program, self.ae_restore) = (0, false, None);
        Ok(match (mode, reg) {
            (0, _) => Ea::Dn(reg),
            (1, _) => Ea::An(reg),
            (3, _) => {
                let addr = self.a(reg);
                self.set_a(reg, addr.wrapping_add(an_step(reg, size)));
                if size == Size::Long {
                    self.ae_restore = Some((reg, addr));
                }
                Ea::Mem(addr)
            }
            (4, _) => {
                let addr = self.a(reg).wrapping_sub(an_step(reg, size));
                self.set_a(reg, addr);
                if predecrement_cost {
                    self.idle(2);
                }
                if size == Size::Word {
                    self.ae_pc = 2;
                }
                Ea::Mem(addr)
            }
            (7, 4) => Ea::Imm(match size {
                // A byte immediate is the low byte of its extension word.
                Size::Byte => u32::from(self.fetch16(bus)?) & 0xFF,
                Size::Word => u32::from(self.fetch16(bus)?),
                Size::Long => self.fetch32(bus)?,
            }),
            _ => {
                let addr = self.ea_addr(bus, mode, reg)?;
                if matches!((mode, reg), (5 | 6, _) | (7, 2 | 3)) {
                    self.ae_pc = -2;
                }
                self.ae_program = mode == 7 && (reg == 2 || reg == 3);
                Ea::Mem(addr)
            }
        })
    }

    /// The address of a control addressing mode (the operand of LEA, PEA, JMP, JSR, MOVEM): 2 (An), 5 d16(An),
    /// 6 d8(An,Xn), 7/0 abs.W, 7/1 abs.L, 7/2 d16(PC), 7/3 d8(PC,Xn). Fetches the extension words and adds the 2
    /// internal cycles of the indexed modes. Any other mode is `Err(Fault::Illegal)`.
    pub(crate) fn ea_addr(&mut self, bus: &mut impl Bus, mode: u8, reg: u8) -> Res<u32> {
        match (mode, reg) {
            (2, _) => Ok(self.a(reg)),
            (5, _) => {
                let disp = Size::Word.sext(u32::from(self.fetch16(bus)?));
                Ok(self.a(reg).wrapping_add(disp))
            }
            (6, _) => {
                let base = self.a(reg);
                self.indexed(bus, base)
            }
            (7, 0) => Ok(Size::Word.sext(u32::from(self.fetch16(bus)?))),
            (7, 1) => self.fetch32(bus),
            // The base of the PC-relative modes is the address of the extension word.
            (7, 2) => {
                let base = self.pc;
                let disp = Size::Word.sext(u32::from(self.fetch16(bus)?));
                Ok(base.wrapping_add(disp))
            }
            (7, 3) => {
                let base = self.pc;
                self.indexed(bus, base)
            }
            _ => Err(Fault::Illegal),
        }
    }

    /// `base` + d8 + Xn from a brief extension word: bit 15 picks An over Dn, bits 14-12 the register, bit 11 the
    /// whole register over its sign-extended low word, bits 7-0 the signed displacement. 4 cycles of fetch + 2.
    fn indexed(&mut self, bus: &mut impl Bus, base: u32) -> Res<u32> {
        let ext = self.fetch16(bus)?;
        let n = ((ext >> 12) & 7) as u8;
        let mut index = if ext & 0x8000 != 0 { self.a(n) } else { self.d[usize::from(n)] };
        if ext & 0x0800 == 0 {
            index = Size::Word.sext(index);
        }
        self.idle(2);
        Ok(base.wrapping_add(Size::Byte.sext(u32::from(ext))).wrapping_add(index))
    }

    /// The value of a resolved operand, cut to `size` (An gives its low word for `Word`).
    pub(crate) fn load(&mut self, bus: &mut impl Bus, ea: Ea, size: Size) -> Res<u32> {
        match ea {
            Ea::Dn(n) => Ok(self.d(n, size)),
            Ea::An(n) => Ok(self.a(n) & size.mask()),
            Ea::Mem(addr) => self.read(bus, addr, size),
            Ea::Imm(val) => Ok(val & size.mask()),
        }
    }

    /// Writes `val` to a resolved operand. Dn keeps the bits above `size`. An is always written whole: a word is
    /// sign-extended (MOVEA.W, ADDA.W); a byte to An and any write to `Imm` are `Err(Fault::Illegal)`.
    pub(crate) fn store(&mut self, bus: &mut impl Bus, ea: Ea, size: Size, val: u32) -> Res<()> {
        match ea {
            Ea::Dn(n) => self.set_d(n, size, val),
            Ea::An(_) if size == Size::Byte => return Err(Fault::Illegal),
            Ea::An(n) => self.set_a(n, size.sext(val)),
            Ea::Mem(addr) => return self.write(bus, addr, size, val),
            Ea::Imm(_) => return Err(Fault::Illegal),
        }
        Ok(())
    }

    // ---- exceptions ----

    /// Exception processing with the 6-byte frame: SR is copied, S goes on and T off, `pc` and the copied SR are
    /// pushed on the supervisor stack and `pc` is loaded from vector `vector` (address `vector * 4`). 20 cycles of
    /// bus (3 words written, 2 read); the caller idles up to the total of the manual: 34 for most, 40 for CHK, 38 for
    /// a division by zero. `pc` is pushed as it is: set it first if the frame needs another address.
    pub(crate) fn exception(&mut self, bus: &mut impl Bus, vector: u8) -> Res<()> {
        let old = self.sr;
        self.sr = (old | sr::S) & !sr::T;
        self.push32(bus, self.pc)?;
        self.push16(bus, old)?;
        self.pc = self.read(bus, u32::from(vector) * 4, Size::Long)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64 KB of RAM mirrored over the whole address space; counts the accesses and logs the acknowledged levels.
    struct Ram {
        mem: Vec<u8>,
        hits: u32,
        last: u32,
        acks: Vec<u8>,
    }

    impl Ram {
        fn new() -> Ram {
            Ram { mem: vec![0; 0x1_0000], hits: 0, last: 0, acks: Vec::new() }
        }

        fn r16(&self, addr: u32) -> u16 {
            let a = addr as usize & 0xFFFF;
            u16::from_be_bytes([self.mem[a], self.mem[a + 1]])
        }

        fn r32(&self, addr: u32) -> u32 {
            (u32::from(self.r16(addr)) << 16) | u32::from(self.r16(addr + 2))
        }

        fn w16(&mut self, addr: u32, val: u16) {
            let a = addr as usize & 0xFFFF;
            self.mem[a..a + 2].copy_from_slice(&val.to_be_bytes());
        }

        fn w32(&mut self, addr: u32, val: u32) {
            self.w16(addr, (val >> 16) as u16);
            self.w16(addr + 2, val as u16);
        }
    }

    impl Bus for Ram {
        fn read8(&mut self, addr: u32) -> u8 {
            self.hits += 1;
            self.last = addr;
            self.mem[addr as usize & 0xFFFF]
        }

        fn read16(&mut self, addr: u32) -> u16 {
            self.hits += 1;
            self.last = addr;
            self.r16(addr)
        }

        fn write8(&mut self, addr: u32, val: u8) {
            self.hits += 1;
            self.last = addr;
            self.mem[addr as usize & 0xFFFF] = val;
        }

        fn write16(&mut self, addr: u32, val: u16) {
            self.hits += 1;
            self.last = addr;
            self.w16(addr, val);
        }

        fn int_ack(&mut self, level: u8) -> u8 {
            self.acks.push(level);
            24 + level
        }
    }

    /// After reset: supervisor mode, mask 7, PC 0x400, SSP 0x8000, USP 0x7000; vector n points at 0x1000 + 0x10 * n.
    fn machine() -> (Cpu, Ram) {
        let mut bus = Ram::new();
        bus.w32(0, 0x8000);
        bus.w32(4, 0x400);
        for n in 2..64 {
            bus.w32(n * 4, 0x1000 + n * 0x10);
        }
        let mut cpu = Cpu::new();
        cpu.reset(&mut bus);
        cpu.usp = 0x7000;
        (cpu, bus)
    }

    #[test]
    fn sizes_and_fields() {
        let all = [Size::Byte, Size::Word, Size::Long];
        assert_eq!(all.map(Size::bytes), [1, 2, 4]);
        assert_eq!(all.map(Size::bits), [8, 16, 32]);
        assert_eq!(all.map(Size::mask), [0xFF, 0xFFFF, 0xFFFF_FFFF]);
        assert_eq!(all.map(Size::msb), [0x80, 0x8000, 0x8000_0000]);
        assert_eq!(Size::Byte.sext(0x180), 0xFFFF_FF80);
        assert_eq!(Size::Word.sext(0x1_7FFF), 0x7FFF);
        assert_eq!(Size::Long.sext(0x8000_0005), 0x8000_0005);
        assert_eq!([0x0000, 0x0040, 0x0080, 0x00C0].map(Size::of), [Some(Size::Byte), Some(Size::Word), Some(Size::Long), None]);
        // 0000 101 000 011 110
        assert_eq!((reg9(0x0A1E), mode(0x0A1E), reg(0x0A1E)), (5, 3, 6));
    }

    #[test]
    fn reset_state() {
        let mut bus = Ram::new();
        bus.w32(0, 0x00FF_FE00);
        bus.w32(4, 0x0000_0200);
        let mut cpu = Cpu::new();
        cpu.stopped = true;
        cpu.halted = true;
        cpu.sr = 0x8015;
        cpu.reset(&mut bus);
        assert_eq!((cpu.sr, cpu.ssp, cpu.pc, cpu.a(7)), (0x2700, 0x00FF_FE00, 0x200, 0x00FF_FE00));
        assert!(!cpu.stopped && !cpu.halted);
    }

    #[test]
    fn registers_and_status() {
        let (mut cpu, _) = machine();
        cpu.set_a(7, 0x8010);
        assert_eq!(cpu.ssp, 0x8010);
        cpu.set_sr(0xFFFF & !sr::S);
        assert_eq!(cpu.sr, 0x871F);
        assert_eq!(cpu.a(7), 0x7000);
        cpu.set_a(7, 0x7004);
        assert_eq!((cpu.usp, cpu.ssp), (0x7004, 0x8010));
        cpu.set_a(3, 9);
        assert_eq!((cpu.a(3), cpu.a[3]), (9, 9));

        cpu.set_ccr(0xFFE5);
        assert_eq!((cpu.sr, cpu.ccr()), (0x8705, 0x05));
        cpu.set_flag(sr::X, true);
        cpu.set_flag(sr::C, false);
        assert!(cpu.flag(sr::X) && !cpu.flag(sr::C));
        assert_eq!(cpu.ccr(), 0x14);

        cpu.d[2] = 0x1234_5678;
        assert_eq!((cpu.d(2, Size::Byte), cpu.d(2, Size::Word), cpu.d(2, Size::Long)), (0x78, 0x5678, 0x1234_5678));
        cpu.set_d(2, Size::Word, 0xABCD_EF01);
        assert_eq!(cpu.d[2], 0x1234_EF01);
        cpu.set_d(2, Size::Byte, 0xFF);
        assert_eq!(cpu.d[2], 0x1234_EFFF);
        cpu.set_d(2, Size::Long, 7);
        assert_eq!(cpu.d[2], 7);
    }

    #[test]
    fn bus_access_sizes_mask_and_cycles() {
        let (mut cpu, mut bus) = machine();
        cpu.write(&mut bus, 0xFF00_2000, Size::Long, 0x1234_5678).unwrap();
        assert_eq!(bus.mem[0x2000..0x2004], [0x12, 0x34, 0x56, 0x78]);
        // Cut to 24 bits, high word first.
        assert_eq!(bus.last, 0x00_2002);
        assert_eq!(cpu.cyc, 8);
        assert_eq!(cpu.read(&mut bus, 0x2000, Size::Long), Ok(0x1234_5678));
        assert_eq!(cpu.read(&mut bus, 0x2001, Size::Byte), Ok(0x34));
        assert_eq!(cpu.read(&mut bus, 0x2002, Size::Word), Ok(0x5678));
        cpu.write(&mut bus, 0x2003, Size::Byte, 0x1FF).unwrap();
        cpu.write(&mut bus, 0x2004, Size::Word, 0xF_ABCD).unwrap();
        assert_eq!(bus.mem[0x2003..0x2006], [0xFF, 0xAB, 0xCD]);
        assert_eq!(cpu.cyc, 8 + 8 + 4 + 4 + 4 + 4);
    }

    #[test]
    fn odd_word_access_faults_before_the_bus() {
        let (mut cpu, mut bus) = machine();
        let hits = bus.hits;
        assert_eq!(cpu.read(&mut bus, 0x2001, Size::Word), Err(Fault::Address { addr: 0x2001, write: false, program: false }));
        assert_eq!(cpu.read(&mut bus, 0x2001, Size::Long), Err(Fault::Address { addr: 0x2001, write: false, program: false }));
        assert_eq!(cpu.write(&mut bus, 0x2003, Size::Word, 0), Err(Fault::Address { addr: 0x2003, write: true, program: false }));
        assert_eq!(cpu.write(&mut bus, 0x2003, Size::Long, 0), Err(Fault::Address { addr: 0x2003, write: true, program: false }));
        cpu.pc = 0x401;
        assert_eq!(cpu.fetch16(&mut bus), Err(Fault::Address { addr: 0x401, write: false, program: true }));
        assert_eq!((bus.hits, cpu.cyc, cpu.pc), (hits, 0, 0x401));
    }

    #[test]
    fn fetch_advances_pc() {
        let (mut cpu, mut bus) = machine();
        bus.w16(0x400, 0x1234);
        bus.w32(0x402, 0xDEAD_BEEF);
        assert_eq!(cpu.fetch16(&mut bus), Ok(0x1234));
        assert_eq!(cpu.pc, 0x402);
        assert_eq!(cpu.fetch32(&mut bus), Ok(0xDEAD_BEEF));
        assert_eq!((cpu.pc, cpu.cyc), (0x406, 12));
    }

    #[test]
    fn register_and_indirect_modes() {
        let (mut cpu, mut bus) = machine();
        let hits = bus.hits;
        cpu.a[1] = 0x2000;
        assert_eq!(cpu.ea(&mut bus, 0, 3, Size::Word), Ok(Ea::Dn(3)));
        assert_eq!(cpu.ea(&mut bus, 1, 2, Size::Long), Ok(Ea::An(2)));
        assert_eq!(cpu.ea(&mut bus, 2, 1, Size::Long), Ok(Ea::Mem(0x2000)));
        assert_eq!(cpu.ea_addr(&mut bus, 2, 1), Ok(0x2000));
        assert_eq!((cpu.a[1], cpu.pc, cpu.cyc, bus.hits), (0x2000, 0x400, 0, hits));
    }

    #[test]
    fn postincrement_steps_by_size() {
        let (mut cpu, mut bus) = machine();
        cpu.a[1] = 0x2000;
        cpu.a[2] = 0x3000;
        cpu.a[3] = 0x4000;
        assert_eq!(cpu.ea(&mut bus, 3, 1, Size::Byte), Ok(Ea::Mem(0x2000)));
        assert_eq!(cpu.ea(&mut bus, 3, 2, Size::Word), Ok(Ea::Mem(0x3000)));
        assert_eq!(cpu.ea(&mut bus, 3, 3, Size::Long), Ok(Ea::Mem(0x4000)));
        assert_eq!((cpu.a[1], cpu.a[2], cpu.a[3]), (0x2001, 0x3002, 0x4004));
        // A byte on the stack pointer keeps it even.
        assert_eq!(cpu.ea(&mut bus, 3, 7, Size::Byte), Ok(Ea::Mem(0x8000)));
        assert_eq!(cpu.ssp, 0x8002);
        assert_eq!((cpu.cyc, cpu.pc), (0, 0x400));
    }

    #[test]
    fn predecrement_steps_by_size_and_costs_two() {
        let (mut cpu, mut bus) = machine();
        cpu.a[1] = 0x2000;
        cpu.a[2] = 0x3000;
        cpu.a[3] = 0x4000;
        assert_eq!(cpu.ea(&mut bus, 4, 1, Size::Byte), Ok(Ea::Mem(0x1FFF)));
        assert_eq!(cpu.ea(&mut bus, 4, 2, Size::Word), Ok(Ea::Mem(0x2FFE)));
        assert_eq!(cpu.ea(&mut bus, 4, 3, Size::Long), Ok(Ea::Mem(0x3FFC)));
        assert_eq!((cpu.a[1], cpu.a[2], cpu.a[3]), (0x1FFF, 0x2FFE, 0x3FFC));
        assert_eq!(cpu.ea(&mut bus, 4, 7, Size::Byte), Ok(Ea::Mem(0x7FFE)));
        assert_eq!(cpu.ssp, 0x7FFE);
        assert_eq!(cpu.cyc, 8);
        // MOVE's destination: the same address, no internal cycles.
        assert_eq!(cpu.ea_plain(&mut bus, 4, 2, Size::Word), Ok(Ea::Mem(0x2FFC)));
        assert_eq!((cpu.a[2], cpu.cyc), (0x2FFC, 8));
    }

    #[test]
    fn displacement_and_absolute_modes() {
        let (mut cpu, mut bus) = machine();
        cpu.a[4] = 0x2000;
        bus.w16(0x400, 0xFFFE);
        assert_eq!(cpu.ea(&mut bus, 5, 4, Size::Word), Ok(Ea::Mem(0x1FFE)));
        assert_eq!((cpu.pc, cpu.cyc, cpu.a[4]), (0x402, 4, 0x2000));
        // abs.W is sign-extended.
        bus.w16(0x402, 0x8000);
        assert_eq!(cpu.ea(&mut bus, 7, 0, Size::Word), Ok(Ea::Mem(0xFFFF_8000)));
        bus.w32(0x404, 0x0012_3456);
        assert_eq!(cpu.ea(&mut bus, 7, 1, Size::Byte), Ok(Ea::Mem(0x0012_3456)));
        // d16(PC) counts from its own extension word.
        bus.w16(0x408, 0x0010);
        assert_eq!(cpu.ea(&mut bus, 7, 2, Size::Long), Ok(Ea::Mem(0x418)));
        assert_eq!((cpu.pc, cpu.cyc), (0x40A, 4 + 4 + 8 + 4));
    }

    #[test]
    fn indexed_modes() {
        let (mut cpu, mut bus) = machine();
        cpu.a[0] = 0x2000;
        cpu.a[3] = 0x100;
        cpu.d[2] = 0x0001_FFF0;
        // D2.W (-16), displacement -4.
        bus.w16(0x400, 0x20FC);
        assert_eq!(cpu.ea(&mut bus, 6, 0, Size::Word), Ok(Ea::Mem(0x1FEC)));
        assert_eq!((cpu.pc, cpu.cyc), (0x402, 6));
        // D2.L, displacement -4.
        bus.w16(0x402, 0x28FC);
        assert_eq!(cpu.ea(&mut bus, 6, 0, Size::Word), Ok(Ea::Mem(0x0002_1FEC)));
        // A3.L, displacement +0x10.
        bus.w16(0x404, 0xB810);
        assert_eq!(cpu.ea(&mut bus, 6, 0, Size::Long), Ok(Ea::Mem(0x2110)));
        // d8(PC,D2.W): 0x406 - 16 + 2.
        bus.w16(0x406, 0x2002);
        assert_eq!(cpu.ea(&mut bus, 7, 3, Size::Byte), Ok(Ea::Mem(0x3F8)));
        assert_eq!((cpu.pc, cpu.cyc), (0x408, 24));
        bus.w16(0x408, 0x20FC);
        assert_eq!(cpu.ea_addr(&mut bus, 6, 0), Ok(0x1FEC));
        bus.w16(0x40A, 0x2002);
        assert_eq!(cpu.ea_addr(&mut bus, 7, 3), Ok(0x3FC));
        assert_eq!((cpu.pc, cpu.cyc, cpu.a[0], cpu.d[2]), (0x40C, 36, 0x2000, 0x0001_FFF0));
    }

    #[test]
    fn immediate_and_invalid_modes() {
        let (mut cpu, mut bus) = machine();
        bus.w16(0x400, 0x12FF);
        bus.w16(0x402, 0xBEEF);
        bus.w32(0x404, 0xCAFE_F00D);
        assert_eq!(cpu.ea(&mut bus, 7, 4, Size::Byte), Ok(Ea::Imm(0xFF)));
        assert_eq!(cpu.ea(&mut bus, 7, 4, Size::Word), Ok(Ea::Imm(0xBEEF)));
        assert_eq!(cpu.ea(&mut bus, 7, 4, Size::Long), Ok(Ea::Imm(0xCAFE_F00D)));
        assert_eq!((cpu.pc, cpu.cyc), (0x408, 16));
        for reg in 5..8 {
            assert_eq!(cpu.ea(&mut bus, 7, reg, Size::Word), Err(Fault::Illegal));
        }
        for (mode, reg) in [(0, 0), (1, 0), (3, 0), (4, 0), (7, 4), (7, 5)] {
            assert_eq!(cpu.ea_addr(&mut bus, mode, reg), Err(Fault::Illegal));
        }
        assert_eq!((cpu.pc, cpu.cyc), (0x408, 16));
    }

    #[test]
    fn load_and_store() {
        let (mut cpu, mut bus) = machine();
        cpu.d[1] = 0x1122_3344;
        assert_eq!(cpu.load(&mut bus, Ea::Dn(1), Size::Byte), Ok(0x44));
        cpu.store(&mut bus, Ea::Dn(1), Size::Word, 0xAAAA_BBBB).unwrap();
        assert_eq!(cpu.d[1], 0x1122_BBBB);
        cpu.store(&mut bus, Ea::An(2), Size::Word, 0x8000).unwrap();
        assert_eq!(cpu.a[2], 0xFFFF_8000);
        assert_eq!(cpu.load(&mut bus, Ea::An(2), Size::Word), Ok(0x8000));
        cpu.store(&mut bus, Ea::An(2), Size::Long, 0x1234_5678).unwrap();
        assert_eq!(cpu.load(&mut bus, Ea::An(2), Size::Long), Ok(0x1234_5678));
        assert_eq!(cpu.store(&mut bus, Ea::An(2), Size::Byte, 0), Err(Fault::Illegal));
        assert_eq!(cpu.cyc, 0);

        cpu.store(&mut bus, Ea::Mem(0x2000), Size::Long, 0x0102_0304).unwrap();
        assert_eq!(cpu.load(&mut bus, Ea::Mem(0x2002), Size::Word), Ok(0x0304));
        assert_eq!(cpu.load(&mut bus, Ea::Mem(0x2001), Size::Byte), Ok(0x02));
        assert_eq!(cpu.cyc, 16);
        assert_eq!(cpu.load(&mut bus, Ea::Mem(0x2001), Size::Word), Err(Fault::Address { addr: 0x2001, write: false, program: false }));
        assert_eq!(cpu.load(&mut bus, Ea::Imm(0x1234), Size::Byte), Ok(0x34));
        assert_eq!(cpu.store(&mut bus, Ea::Imm(0), Size::Word, 0), Err(Fault::Illegal));
    }

    #[test]
    fn stack_follows_the_mode() {
        let (mut cpu, mut bus) = machine();
        cpu.push32(&mut bus, 0x1234_5678).unwrap();
        cpu.push16(&mut bus, 0xABCD).unwrap();
        assert_eq!((cpu.ssp, bus.r16(0x7FFA), bus.r32(0x7FFC)), (0x7FFA, 0xABCD, 0x1234_5678));
        assert_eq!(cpu.pop16(&mut bus), Ok(0xABCD));
        assert_eq!(cpu.pop32(&mut bus), Ok(0x1234_5678));
        assert_eq!((cpu.ssp, cpu.cyc), (0x8000, 24));
        cpu.set_sr(0);
        cpu.push16(&mut bus, 1).unwrap();
        assert_eq!((cpu.usp, cpu.ssp, bus.r16(0x6FFE)), (0x6FFE, 0x8000, 1));
    }

    #[test]
    fn conditions() {
        // Bit n of the result: condition n holds.
        let truth = |cpu: &mut Cpu, ccr: u16| {
            cpu.set_ccr(ccr);
            (0..16u16).filter(|&cc| cpu.cond(cc)).fold(0u16, |set, cc| set | 1 << cc)
        };
        let (mut cpu, _) = machine();
        assert_eq!(truth(&mut cpu, 0), 0x5555);
        assert_eq!(truth(&mut cpu, sr::C), 0x5569);
        assert_eq!(truth(&mut cpu, sr::Z), 0x9599);
        assert_eq!(truth(&mut cpu, sr::N), 0xA955);
        assert_eq!(truth(&mut cpu, sr::N | sr::V), 0x5A55);
        // Only the low four bits count: the condition field of 0x67xx is EQ.
        cpu.set_ccr(sr::Z);
        assert!(cpu.cond(0x67));
    }

    #[test]
    fn flag_helpers() {
        let (mut cpu, _) = machine();
        cpu.set_ccr(sr::X | sr::C | sr::V);
        cpu.nz(0x80, Size::Byte);
        assert_eq!(cpu.ccr(), sr::X | sr::N);
        cpu.nz(0x1_0000, Size::Word);
        assert_eq!(cpu.ccr(), sr::X | sr::Z);
        // 0x7F + 1 overflows into the sign.
        cpu.add_flags(1, 0x7F, 0x80, Size::Byte);
        assert_eq!(cpu.ccr(), sr::N | sr::V);
        // 0xFFFF + 1 carries out.
        cpu.add_flags(1, 0xFFFF, 0x1_0000, Size::Word);
        assert_eq!(cpu.ccr(), sr::X | sr::Z | sr::C);
        // 0 - 1 borrows.
        cpu.sub_flags(1, 0, 0xFFFF_FFFF, Size::Long);
        assert_eq!(cpu.ccr(), sr::X | sr::N | sr::C);
        // 0x80 - 1 overflows.
        cpu.sub_flags(1, 0x80, 0x7F, Size::Byte);
        assert_eq!(cpu.ccr(), sr::V);
        cpu.set_flag(sr::X, true);
        cpu.cmp_flags(5, 5, 0, Size::Word);
        assert_eq!(cpu.ccr(), sr::X | sr::Z);
        cpu.cmp_flags(6, 5, 0xFFFF, Size::Word);
        assert_eq!(cpu.ccr(), sr::X | sr::N | sr::C);
        assert_eq!(cpu.sr & 0xFF00, 0x2700);
    }

    #[test]
    fn moveq_and_nop_cost_four() {
        let (mut cpu, mut bus) = machine();
        bus.w16(0x400, 0x76FF); // MOVEQ #-1,D3
        bus.w16(0x402, 0x4E71); // NOP
        bus.w16(0x404, 0x7000); // MOVEQ #0,D0
        cpu.set_ccr(0x1F);
        cpu.d[0] = 5;
        assert_eq!(cpu.step(&mut bus), 4);
        assert_eq!((cpu.d[3], cpu.ccr(), cpu.pc), (0xFFFF_FFFF, sr::X | sr::N, 0x402));
        assert_eq!(cpu.step(&mut bus), 4);
        assert_eq!((cpu.sr, cpu.pc), (0x2718, 0x404));
        assert_eq!(cpu.step(&mut bus), 4);
        assert_eq!((cpu.d[0], cpu.ccr(), cpu.pc, cpu.ssp), (0, sr::X | sr::Z, 0x406, 0x8000));
    }

    #[test]
    fn line_a_f_and_illegal_trap_with_the_opcode_address() {
        // (opcode, vector)
        for (op, vector) in [(0xA123, 10), (0xF000, 11), (0x4AFC, 4)] {
            let (mut cpu, mut bus) = machine();
            bus.w16(0x400, op);
            cpu.set_sr(0x0015);
            assert_eq!(cpu.step(&mut bus), 34);
            assert_eq!((cpu.pc, cpu.sr, cpu.ssp, cpu.usp), (0x1000 + vector * 0x10, 0x2015, 0x7FFA, 0x7000));
            assert_eq!((bus.r16(0x7FFA), bus.r32(0x7FFC)), (0x0015, 0x400));
        }
    }

    #[test]
    fn privilege_violation_frame() {
        let (mut cpu, mut bus) = machine();
        cpu.set_sr(0);
        cpu.op_pc = 0x400;
        cpu.pc = 0x404;
        cpu.raise(&mut bus, Fault::Privilege, 0x4E72, false);
        assert_eq!((cpu.pc, cpu.sr, cpu.ssp, cpu.cyc), (0x1080, 0x2000, 0x7FFA, 34));
        assert_eq!((bus.r16(0x7FFA), bus.r32(0x7FFC)), (0, 0x400));
    }

    #[test]
    fn odd_pc_builds_the_address_error_frame() {
        let (mut cpu, mut bus) = machine();
        cpu.pc = 0x401;
        assert_eq!(cpu.step(&mut bus), 50);
        assert_eq!((cpu.pc, cpu.ssp, cpu.sr), (0x1030, 0x8000 - 14, 0x2700));
        let sp = cpu.ssp;
        assert_eq!(bus.r16(sp), 0x0016); // read, supervisor program
        assert_eq!(bus.r32(sp + 2), 0x401); // access address
        assert_eq!(bus.r16(sp + 6), 0); // instruction register: nothing was fetched
        assert_eq!(bus.r16(sp + 8), 0x2700); // SR
        assert_eq!(bus.r32(sp + 10), 0x401 - 2); // PC, one prefetched word behind ours
        assert!(!cpu.halted);
    }

    #[test]
    fn odd_data_write_builds_the_address_error_frame() {
        let (mut cpu, mut bus) = machine();
        cpu.set_sr(0x8000);
        cpu.pc = 0x402;
        let fault = cpu.write(&mut bus, 0x0012_2001, Size::Word, 0).unwrap_err();
        cpu.raise(&mut bus, fault, 0x3080, false);
        assert_eq!((cpu.pc, cpu.ssp, cpu.usp, cpu.sr, cpu.cyc), (0x1030, 0x8000 - 14, 0x7000, 0x2000, 54));
        let sp = cpu.ssp;
        assert_eq!(bus.r16(sp), 0x3081); // write, instruction, user data; the rest is the opcode's
        assert_eq!(bus.r32(sp + 2), 0x0012_2001);
        assert_eq!(bus.r16(sp + 6), 0x3080);
        assert_eq!(bus.r16(sp + 8), 0x8000);
        assert_eq!(bus.r32(sp + 10), 0x402);
    }

    #[test]
    fn long_postincrement_fault_leaves_the_register() {
        let (mut cpu, mut bus) = machine();
        bus.w16(0x400, 0x4A98); // TST.l (A0)+
        cpu.a[0] = 0x1001;
        assert_eq!(cpu.step(&mut bus), 58);
        assert_eq!(cpu.a[0], 0x1001);
        let sp = cpu.ssp;
        assert_eq!(bus.r16(sp), 0x4A95); // read, instruction, supervisor data
        assert_eq!(bus.r32(sp + 10), 0x402);
    }

    #[test]
    fn fault_while_taking_an_address_error_halts() {
        let (mut cpu, mut bus) = machine();
        bus.w16(0x400, 0xA000);
        cpu.ssp = 0x7FFF;
        assert!(cpu.step(&mut bus) >= 4);
        assert!(cpu.halted);
        let hits = bus.hits;
        assert_eq!(cpu.step(&mut bus), 4);
        assert_eq!(bus.hits, hits);
        cpu.reset(&mut bus);
        assert!(!cpu.halted);
    }

    #[test]
    fn interrupt_respects_the_mask_and_costs_44() {
        let (mut cpu, mut bus) = machine();
        bus.w16(0x400, 0x4E71);
        bus.w16(0x11C0, 0x4E71);
        cpu.irq = 4;
        // The mask is 7 after reset.
        assert_eq!(cpu.step(&mut bus), 4);
        assert!(bus.acks.is_empty());
        cpu.set_sr(0x2300);
        assert_eq!(cpu.step(&mut bus), 44);
        assert_eq!(bus.acks, [4]);
        assert_eq!((cpu.pc, cpu.sr, cpu.ssp), (0x11C0, 0x2400, 0x7FFA));
        assert_eq!((bus.r16(0x7FFA), bus.r32(0x7FFC)), (0x2300, 0x402));
        // A level equal to the mask waits.
        assert_eq!(cpu.step(&mut bus), 4);
        assert_eq!((bus.acks.len(), cpu.pc), (1, 0x11C2));
    }

    #[test]
    fn level_7_is_taken_once_per_change() {
        let (mut cpu, mut bus) = machine();
        for addr in [0x11F0, 0x11F2] {
            bus.w16(addr, 0x4E71);
        }
        cpu.irq = 7;
        assert_eq!(cpu.step(&mut bus), 44);
        assert_eq!((cpu.pc, cpu.sr), (0x11F0, 0x2700));
        assert_eq!(cpu.step(&mut bus), 4);
        cpu.irq = 0;
        assert_eq!(cpu.step(&mut bus), 4);
        cpu.irq = 7;
        assert_eq!(cpu.step(&mut bus), 44);
        assert_eq!(bus.acks, [7, 7]);
    }

    #[test]
    fn stopped_waits_for_an_interrupt() {
        let (mut cpu, mut bus) = machine();
        cpu.stopped = true;
        cpu.set_sr(0x2000);
        let hits = bus.hits;
        assert_eq!(cpu.step(&mut bus), 4);
        assert_eq!((bus.hits, cpu.pc), (hits, 0x400));
        cpu.irq = 2;
        assert_eq!(cpu.step(&mut bus), 44);
        assert!(!cpu.stopped);
        assert_eq!((cpu.pc, cpu.sr, bus.r32(0x7FFC)), (0x11A0, 0x2200, 0x400));
    }

    #[test]
    fn trace_follows_an_instruction_but_not_an_illegal_one() {
        let (mut cpu, mut bus) = machine();
        bus.w16(0x400, 0x4E71);
        cpu.set_sr(0xA700);
        assert_eq!(cpu.step(&mut bus), 4);
        assert!(cpu.trace_pending);
        assert_eq!((cpu.pc, cpu.sr, cpu.ssp), (0x402, 0xA700, 0x8000));
        // The trace exception is the next step, ahead of an interrupt.
        cpu.irq = 7;
        assert_eq!(cpu.step(&mut bus), 34);
        cpu.irq = 0;
        cpu.irq_prev = 0;
        assert!(!cpu.trace_pending);
        assert_eq!((cpu.pc, cpu.sr, cpu.ssp), (0x1090, 0x2700, 0x7FFA));
        assert_eq!((bus.r16(0x7FFA), bus.r32(0x7FFC)), (0xA700, 0x402));

        bus.w16(0x500, 0x4AFC);
        cpu.pc = 0x500;
        cpu.set_sr(0xA700);
        assert_eq!(cpu.step(&mut bus), 34);
        assert!(!cpu.trace_pending);
        assert_eq!((cpu.pc, cpu.sr, bus.r32(0x7FF6)), (0x1040, 0x2700, 0x500));
    }
}
