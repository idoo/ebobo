//! CPU tests: the nestest golden log plus hand-assembled interrupt timing cases.

use super::{Cpu, CpuBus, FLAG_B, FLAG_I, FLAG_U};

struct TestBus {
    ram: Vec<u8>,
    cycles: u64,
    nmi: bool,
    irq: bool,
    /// Raise the NMI line at the end of this absolute cycle.
    nmi_at: Option<u64>,
    /// Vector fetches observed, for "did we enter a handler" assertions.
    vectors: Vec<u16>,
    /// When set, every bus access is appended to `trace` as ('R'|'W', addr, value).
    tracing: bool,
    trace: Vec<(char, u16, u8)>,
}

impl TestBus {
    fn new() -> TestBus {
        TestBus {
            ram: vec![0; 0x10000],
            cycles: 0,
            nmi: false,
            irq: false,
            nmi_at: None,
            vectors: Vec::new(),
            tracing: false,
            trace: Vec::new(),
        }
    }

    #[inline]
    fn tick(&mut self) {
        self.cycles += 1;
        if self.nmi_at == Some(self.cycles) {
            self.nmi = true;
        }
    }
}

impl CpuBus for TestBus {
    fn read(&mut self, addr: u16) -> u8 {
        self.tick();
        if (0xFFFA..=0xFFFF).contains(&addr) {
            self.vectors.push(addr);
        }
        let val = self.ram[addr as usize];
        if self.tracing {
            self.trace.push(('R', addr, val));
        }
        val
    }
    fn write(&mut self, addr: u16, val: u8) {
        self.tick();
        self.ram[addr as usize] = val;
        if self.tracing {
            self.trace.push(('W', addr, val));
        }
    }
    fn nmi_pending(&self) -> bool {
        self.nmi
    }
    fn ack_nmi(&mut self) {
        self.nmi = false;
    }
    fn irq_line(&self) -> bool {
        self.irq
    }
}

// ------------------------------------------------------------------ nestest

struct LogLine {
    pc: u16,
    a: u8,
    x: u8,
    y: u8,
    p: u8,
    sp: u8,
    cyc: u64,
}

fn hex8(s: &str, at: usize) -> u8 {
    u8::from_str_radix(&s[at..at + 2], 16).unwrap()
}

fn parse_log_line(line: &str) -> LogLine {
    let pc = u16::from_str_radix(&line[0..4], 16).unwrap();
    let a = hex8(line, line.rfind(" A:").unwrap() + 3);
    let x = hex8(line, line.rfind(" X:").unwrap() + 3);
    let y = hex8(line, line.rfind(" Y:").unwrap() + 3);
    let p = hex8(line, line.rfind(" P:").unwrap() + 3);
    let sp = hex8(line, line.rfind("SP:").unwrap() + 3);
    let cyc_at = line.rfind("CYC:").unwrap() + 4;
    let cyc = line[cyc_at..].trim().parse::<u64>().unwrap();
    LogLine { pc, a, x, y, p, sp, cyc }
}

#[test]
fn cpu_nestest_matches_golden_log() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/nestest/");
    let Ok(rom) = std::fs::read(format!("{dir}nestest.nes")) else {
        println!("test ROMs missing: run scripts/fetch-test-roms.sh");
        return;
    };
    let log = std::fs::read_to_string(format!("{dir}nestest.log")).expect("nestest.log");
    let prg = &rom[16..16 + 16 * 1024];

    let mut bus = TestBus::new();
    bus.ram[0x8000..0xC000].copy_from_slice(prg);
    bus.ram[0xC000..0x10000].copy_from_slice(prg);
    bus.cycles = 7;

    let mut cpu = Cpu::new();
    cpu.a = 0;
    cpu.x = 0;
    cpu.y = 0;
    cpu.p = 0x24;
    cpu.sp = 0xFD;
    cpu.pc = 0xC000;

    let mut lines = 0usize;
    for (i, line) in log.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        lines += 1;
        let e = parse_log_line(line);
        let got = (cpu.pc, cpu.a, cpu.x, cpu.y, cpu.p, cpu.sp, bus.cycles);
        let want = (e.pc, e.a, e.x, e.y, e.p, e.sp, e.cyc);
        assert_eq!(
            got,
            want,
            "\nnestest.log line {}: {}\n  got  PC:{:04X} A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X} CYC:{}\n  want PC:{:04X} A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X} CYC:{}",
            i + 1,
            line,
            got.0, got.1, got.2, got.3, got.4, got.5, got.6,
            want.0, want.1, want.2, want.3, want.4, want.5, want.6,
        );
        cpu.step(&mut bus);
    }
    assert_eq!(lines, 8991, "unexpected nestest.log length");
    // Official + unofficial opcode result codes land in $02/$03.
    assert_eq!((bus.ram[0x02], bus.ram[0x03]), (0, 0), "nestest reported an opcode failure");
}

// --------------------------------------------------------------- interrupts

/// Fresh bus + CPU with `prog` at $0600, NMI vector $0700, IRQ/BRK vector $0800.
fn harness(prog: &[u8]) -> (Cpu, TestBus) {
    let mut bus = TestBus::new();
    bus.ram[0x0600..0x0600 + prog.len()].copy_from_slice(prog);
    bus.ram[0xFFFA] = 0x00;
    bus.ram[0xFFFB] = 0x07;
    bus.ram[0xFFFE] = 0x00;
    bus.ram[0xFFFF] = 0x08;
    let mut cpu = Cpu::new();
    cpu.a = 0;
    cpu.x = 0;
    cpu.y = 0;
    cpu.p = 0x24;
    cpu.sp = 0xFD;
    cpu.pc = 0x0600;
    (cpu, bus)
}

#[test]
fn cpu_power_on_and_reset_take_seven_cycles() {
    let mut bus = TestBus::new();
    bus.ram[0xFFFC] = 0x34;
    bus.ram[0xFFFD] = 0x12;
    let mut cpu = Cpu::new();
    cpu.power_on(&mut bus);
    assert_eq!((cpu.a, cpu.x, cpu.y), (0, 0, 0));
    assert_eq!(cpu.sp, 0xFD);
    assert_eq!(cpu.p, 0x34);
    assert_eq!(cpu.pc, 0x1234);
    assert_eq!(bus.cycles, 7);

    cpu.sp = 0x80;
    cpu.p &= !FLAG_I;
    let before = bus.cycles;
    cpu.reset(&mut bus);
    assert_eq!(cpu.sp, 0x7D, "reset decrements SP by 3");
    assert_ne!(cpu.p & FLAG_I, 0);
    assert_eq!(bus.cycles - before, 7);
    // No writes during reset: the stack area stays clean.
    assert!(bus.ram[0x0100..0x0200].iter().all(|&b| b == 0));
}

#[test]
fn cpu_nmi_is_serviced_after_the_current_instruction() {
    // NOP $1234 (4 cycles); NMI raised during its second cycle.
    let (mut cpu, mut bus) = harness(&[0x0C, 0x34, 0x12, 0xEA]);
    bus.nmi_at = Some(2);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0700, "NMI handler entered");
    assert_eq!(bus.cycles, 4 + 7, "instruction finished before the 7-cycle entry");
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0x03), "pushed PC after the NOP");
    let pushed = bus.ram[0x01FB];
    assert_eq!(pushed & FLAG_B, 0, "B clear on NMI entry");
    assert_ne!(pushed & FLAG_U, 0, "bit 5 set on NMI entry");
    assert_eq!(cpu.sp, 0xFA);
    assert_ne!(cpu.p & FLAG_I, 0);
    assert!(!bus.nmi, "NMI acknowledged");
}

#[test]
fn cpu_nmi_raised_on_the_last_cycle_waits_one_instruction() {
    // NOP $1234 (4 cycles), then NOP; NMI asserted at the end of the final cycle is too late
    // for this instruction's poll (penultimate-cycle rule).
    let (mut cpu, mut bus) = harness(&[0x0C, 0x34, 0x12, 0xEA]);
    bus.nmi_at = Some(4);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0603, "no interrupt yet");
    assert_eq!(bus.cycles, 4);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0700, "serviced after the next instruction");
    assert_eq!(bus.cycles, 4 + 2 + 7);
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0x04));
}

#[test]
fn cpu_irq_is_masked_by_the_i_flag() {
    let (mut cpu, mut bus) = harness(&[0xEA, 0xEA, 0xEA]);
    bus.irq = true;
    assert_ne!(cpu.p & FLAG_I, 0);
    for _ in 0..3 {
        cpu.step(&mut bus);
    }
    assert_eq!(cpu.pc, 0x0603, "no handler entered");
    assert_eq!(bus.cycles, 6);
    assert!(bus.vectors.is_empty(), "no vector fetched");
    assert_eq!(cpu.sp, 0xFD, "nothing pushed");
}

#[test]
fn cpu_cli_lets_exactly_one_more_instruction_run() {
    // CLI; NOP; NOP — the IRQ decision for CLI is made before its flag write, so the IRQ is
    // taken after the following NOP.
    let (mut cpu, mut bus) = harness(&[0x58, 0xEA, 0xEA, 0xEA]);
    bus.irq = true;
    cpu.step(&mut bus); // CLI
    assert_eq!(cpu.pc, 0x0601);
    assert_eq!(cpu.p & FLAG_I, 0);
    assert_eq!(bus.cycles, 2, "no interrupt entry yet");

    cpu.step(&mut bus); // NOP at $0601, then IRQ entry
    assert_eq!(cpu.pc, 0x0800, "IRQ handler entered");
    assert_eq!(bus.cycles, 2 + 2 + 7);
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0x02), "exactly one instruction ran");
    assert_eq!(bus.ram[0x01FB] & FLAG_B, 0, "B clear on IRQ entry");
    assert_ne!(cpu.p & FLAG_I, 0);
}

#[test]
fn cpu_brk_pushes_b_set_and_pc_plus_two() {
    let (mut cpu, mut bus) = harness(&[0x00, 0xFF, 0xEA]);
    cpu.p = 0x24;
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0800);
    assert_eq!(bus.cycles, 7);
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0x02), "pushed PC is BRK+2");
    let pushed = bus.ram[0x01FB];
    assert_ne!(pushed & FLAG_B, 0, "B set on BRK");
    assert_ne!(pushed & FLAG_U, 0);
    assert_ne!(cpu.p & FLAG_I, 0);
}

#[test]
fn cpu_brk_hijacked_by_pending_nmi() {
    let (mut cpu, mut bus) = harness(&[0x00, 0xFF, 0xEA]);
    bus.nmi = true;
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0700, "BRK took the NMI vector");
    assert_ne!(bus.ram[0x01FB] & FLAG_B, 0, "still a BRK push");
    assert!(!bus.nmi);
}

#[test]
fn cpu_jam_idles_forever() {
    let (mut cpu, mut bus) = harness(&[0x02]);
    cpu.step(&mut bus);
    assert!(cpu.jammed);
    let after = bus.cycles;
    cpu.step(&mut bus);
    cpu.step(&mut bus);
    assert_eq!(bus.cycles, after + 2, "one idle read per step");
}

// ------------------------------------------------- bus access sequences
//
// nestest pins down cycle *counts*; MMC3's A12 counter, DMC DMA and PPU open bus depend on the
// exact addresses and order, so trace one instruction of each tricky shape.

fn trace_step(prog: &[u8], setup: impl FnOnce(&mut Cpu, &mut TestBus)) -> Vec<(char, u16, u8)> {
    let (mut cpu, mut bus) = harness(prog);
    setup(&mut cpu, &mut bus);
    bus.tracing = true;
    cpu.step(&mut bus);
    bus.trace
}

#[test]
fn cpu_indexed_read_dummy_only_on_page_cross() {
    // LDA $12F0,X with X=$20 crosses into $13xx: dummy read at the un-carried address.
    let t = trace_step(&[0xBD, 0xF0, 0x12], |c, b| {
        c.x = 0x20;
        b.ram[0x1210] = 0x99;
        b.ram[0x1310] = 0x5A;
    });
    assert_eq!(
        t,
        vec![
            ('R', 0x0600, 0xBD),
            ('R', 0x0601, 0xF0),
            ('R', 0x0602, 0x12),
            ('R', 0x1210, 0x99),
            ('R', 0x1310, 0x5A),
        ]
    );

    // Same instruction with X=$05 stays on the page: no dummy read.
    let t = trace_step(&[0xBD, 0xF0, 0x12], |c, b| {
        c.x = 0x05;
        b.ram[0x12F5] = 0x11;
    });
    assert_eq!(
        t,
        vec![('R', 0x0600, 0xBD), ('R', 0x0601, 0xF0), ('R', 0x0602, 0x12), ('R', 0x12F5, 0x11)]
    );
}

#[test]
fn cpu_indexed_write_always_dummy_reads() {
    // STA $12F0,X, X=$05: no page cross, but the dummy read happens anyway.
    let t = trace_step(&[0x9D, 0xF0, 0x12], |c, _| {
        c.x = 0x05;
        c.a = 0x77;
    });
    assert_eq!(
        t,
        vec![
            ('R', 0x0600, 0x9D),
            ('R', 0x0601, 0xF0),
            ('R', 0x0602, 0x12),
            ('R', 0x12F5, 0x00),
            ('W', 0x12F5, 0x77),
        ]
    );
}

#[test]
fn cpu_rmw_writes_the_old_value_first() {
    // INC $12F0,X with X=$20: dummy read, read, dummy write of the old value, real write.
    let t = trace_step(&[0xFE, 0xF0, 0x12], |c, b| {
        c.x = 0x20;
        b.ram[0x1310] = 0x41;
    });
    assert_eq!(
        t,
        vec![
            ('R', 0x0600, 0xFE),
            ('R', 0x0601, 0xF0),
            ('R', 0x0602, 0x12),
            ('R', 0x1210, 0x00),
            ('R', 0x1310, 0x41),
            ('W', 0x1310, 0x41),
            ('W', 0x1310, 0x42),
        ]
    );

    // ASL $40 (zero page): read, dummy write, write.
    let t = trace_step(&[0x06, 0x40], |_, b| b.ram[0x40] = 0x81);
    assert_eq!(
        t,
        vec![
            ('R', 0x0600, 0x06),
            ('R', 0x0601, 0x40),
            ('R', 0x0040, 0x81),
            ('W', 0x0040, 0x81),
            ('W', 0x0040, 0x02),
        ]
    );
}

#[test]
fn cpu_indirect_modes_read_their_pointers() {
    // LDA ($10,X) with X=$04: dummy read at $10, pointer at $14/$15.
    let t = trace_step(&[0xA1, 0x10], |c, b| {
        c.x = 0x04;
        b.ram[0x14] = 0x00;
        b.ram[0x15] = 0x13;
        b.ram[0x1300] = 0x7E;
    });
    assert_eq!(
        t,
        vec![
            ('R', 0x0600, 0xA1),
            ('R', 0x0601, 0x10),
            ('R', 0x0010, 0x00),
            ('R', 0x0014, 0x00),
            ('R', 0x0015, 0x13),
            ('R', 0x1300, 0x7E),
        ]
    );

    // STA ($10),Y always does the un-carried dummy read.
    let t = trace_step(&[0x91, 0x10], |c, b| {
        c.y = 0x05;
        c.a = 0x33;
        b.ram[0x10] = 0xF0;
        b.ram[0x11] = 0x12;
    });
    assert_eq!(
        t,
        vec![
            ('R', 0x0600, 0x91),
            ('R', 0x0601, 0x10),
            ('R', 0x0010, 0xF0),
            ('R', 0x0011, 0x12),
            ('R', 0x12F5, 0x00),
            ('W', 0x12F5, 0x33),
        ]
    );

    // LDA $FF,X wraps inside the zero page and dummy-reads the un-indexed byte.
    let t = trace_step(&[0xB5, 0xFF], |c, b| {
        c.x = 0x02;
        b.ram[0x0001] = 0x64;
    });
    assert_eq!(
        t,
        vec![('R', 0x0600, 0xB5), ('R', 0x0601, 0xFF), ('R', 0x00FF, 0x00), ('R', 0x0001, 0x64)]
    );
}

#[test]
fn cpu_taken_branch_reads_the_unfixed_address_on_page_cross() {
    let (mut cpu, mut bus) = harness(&[]);
    bus.ram[0x06FE] = 0xD0; // BNE -1
    bus.ram[0x06FF] = 0xFF;
    cpu.pc = 0x06FE;
    cpu.p = 0x24; // Z clear: taken
    bus.tracing = true;
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x06FF);
    assert_eq!(
        bus.trace,
        vec![
            ('R', 0x06FE, 0xD0),
            ('R', 0x06FF, 0xFF),
            ('R', 0x0700, 0x00), // internal cycle, dummy read of PC
            ('R', 0x07FF, 0x00), // page-cross fixup read at the un-carried address
        ]
    );
    assert_eq!(bus.cycles, 4);
}

#[test]
fn cpu_rti_applies_the_i_flag_immediately() {
    // Stack frame: P=$20 (I clear), return address $0602.
    let (mut cpu, mut bus) = harness(&[0x40, 0xFF, 0xEA]);
    bus.ram[0x01FD] = 0x06;
    bus.ram[0x01FC] = 0x02;
    bus.ram[0x01FB] = 0x20;
    cpu.sp = 0xFA;
    bus.irq = true;
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0800, "IRQ taken right after RTI, no instruction in between");
    assert_eq!(bus.cycles, 6 + 7);
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0x02), "return address re-pushed");
    assert_ne!(cpu.p & FLAG_I, 0);
}

#[test]
fn cpu_plp_clearing_i_is_delayed_like_cli() {
    let (mut cpu, mut bus) = harness(&[0x28, 0xEA, 0xEA]);
    bus.ram[0x01FD] = 0x20; // pulled P has I clear
    cpu.sp = 0xFC;
    bus.irq = true;
    cpu.step(&mut bus); // PLP
    assert_eq!(cpu.p & FLAG_I, 0);
    assert_eq!(cpu.pc, 0x0601);
    assert_eq!(bus.cycles, 4, "no interrupt entry yet");
    cpu.step(&mut bus); // NOP, then IRQ
    assert_eq!(cpu.pc, 0x0800);
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0x02), "exactly one instruction ran");
}

#[test]
fn cpu_sei_does_not_stop_the_irq_it_was_meant_to_mask() {
    let (mut cpu, mut bus) = harness(&[0x78, 0xEA]);
    cpu.p = 0x20; // I clear
    bus.irq = true;
    cpu.step(&mut bus); // SEI, then the IRQ it was too late to block
    assert_eq!(cpu.pc, 0x0800);
    assert_eq!(bus.cycles, 2 + 7);
    assert_ne!(bus.ram[0x01FB] & FLAG_I, 0, "pushed P already has SEI's I set");
}

#[test]
fn cpu_nmi_during_irq_entry_hijacks_the_vector() {
    let (mut cpu, mut bus) = harness(&[0xEA, 0xEA]);
    cpu.p = 0x20; // I clear
    bus.irq = true;
    bus.nmi_at = Some(6); // during the IRQ entry, before the vector is chosen
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0700, "entry hijacked to the NMI vector");
    assert_eq!(bus.cycles, 2 + 7, "still a single 7-cycle entry");
    assert_eq!(bus.ram[0x01FB] & FLAG_B, 0, "B clear: hardware interrupt, not BRK");
    assert!(!bus.nmi, "NMI acknowledged");
}

#[test]
fn cpu_taken_branch_without_page_cross_delays_the_interrupt_poll() {
    // BNE +2 (taken, stays on the page): an NMI asserted during its second cycle is not
    // recognized at the end of the branch, only after the following instruction.
    let (mut cpu, mut bus) = harness(&[0xD0, 0x02, 0xFF, 0xFF, 0xEA]);
    cpu.p = 0x24; // Z clear
    bus.nmi_at = Some(2);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0604, "branch taken");
    assert_eq!(bus.cycles, 3, "poll skipped: no entry yet");
    cpu.step(&mut bus); // NOP at $0604, then the NMI
    assert_eq!(cpu.pc, 0x0700);
    assert_eq!(bus.cycles, 3 + 2 + 7);
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0x05));

    // A taken branch that crosses a page polls normally on its extra cycle.
    let (mut cpu, mut bus) = harness(&[]);
    bus.ram[0x06FE] = 0xD0; // BNE -1 -> $06FF, crosses from $0700
    bus.ram[0x06FF] = 0xFF;
    cpu.pc = 0x06FE;
    cpu.p = 0x24;
    bus.nmi_at = Some(2);
    cpu.step(&mut bus);
    assert_eq!(cpu.pc, 0x0700, "NMI serviced at the end of the branch");
    assert_eq!(bus.cycles, 4 + 7);
    assert_eq!((bus.ram[0x01FD], bus.ram[0x01FC]), (0x06, 0xFF));
}
