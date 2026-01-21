//! Cartridge boards (mappers). Owner: cart+mapper slice.
//!
//! Layout: `Cartridge` (this file) owns the shared memory (`Mem`: PRG ROM, CHR ROM/RAM, PRG RAM,
//! header mirroring) and the precomputed slot maps; each board family lives in its own file
//! and implements [`BoardLogic`]. `Board` is a closed enum dispatching to them (no `dyn`, so
//! the whole cartridge stays `Clone + Hash` for rollback snapshots).
//!
//! # Contract for board files
//! - A board is a small `#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]` struct
//!   of registers (it is in save states; arrays over 32 bytes need
//!   `#[serde(with = "crate::big_array")]`; bump `STATE_VERSION` in lib.rs on layout changes).
//!   No heap allocation in any trait method (they run per CPU/PPU access).
//! - `prg_index`/`chr_index` map addresses to *unwrapped-or-wrapped* indices; `Cartridge`
//!   wraps them and caches them per 8 KiB PRG / 1 KiB CHR slot. The cache is refreshed after
//!   every CPU write to $8000-$FFFF, on `reset`, and whenever `ppu_after_read` returns `true`.
//!   A board that changes banks elsewhere (e.g. in `cpu_tick` or `write_low`) must first add a
//!   remap path in this file (e.g. make that hook return `bool`), or its banks go stale.
//! - Every method except `prg_index`/`chr_index`/`write` has a no-op default.
//! - To add a board: implement it in its family file, return `Some` from that file's
//!   `new(info)`, add a `Board` variant + `dispatch!` arm below, and add the number to
//!   `SUPPORTED_MAPPERS`.

mod action53;
mod discrete;
mod fme7;
mod misc;
mod mmc1;
mod mmc2;
mod mmc3;
mod mmc5;
mod multicart;
mod namco;
mod nrom;
mod vrc;

use crate::cart::{Mirroring, RomBytes, RomError, RomInfo};

/// iNES mapper numbers `Cartridge::new` accepts.
pub const SUPPORTED_MAPPERS: &[u16] = &[
    0, 1, 2, 3, 4, 5, 7, 9, 10, 11, 13, 15, 19, 21, 22, 23, 24, 25, 26, 28, 32, 33, 34, 48, 57, 58,
    64, 66, 68, 69, 70, 71, 76, 78, 79, 85, 87, 88, 89, 93, 95, 113, 118, 140, 152, 154, 180, 184,
    185, 200, 201, 202, 203, 206, 210, 212, 225, 226, 227, 229, 231, 233, 255,
];

/// Planned mapper numbers per not-yet-implemented family file. `Cartridge::new` routes these
/// numbers to the file's `new(info)`; while it returns `None` the ROM is `UnsupportedMapper`.
#[allow(dead_code)] // read by tests and tooling
pub(crate) const PLANNED: &[(&str, &[u16])] = &[
    ("mmc5", mmc5::MAPPERS),
    ("fme7", fme7::MAPPERS),
    ("vrc", vrc::MAPPERS),
    ("namco", namco::MAPPERS),
    ("misc", misc::MAPPERS),
    ("multicart", multicart::MAPPERS),
];

pub(crate) const K1: usize = 0x400;
pub(crate) const K4: usize = 0x1000;
pub(crate) const K8: usize = 0x2000;
pub(crate) const K16: usize = 0x4000;
pub(crate) const K32: usize = 0x8000;

/// `i % len`, with a mask for the usual power-of-two sizes (same result, no division).
#[inline]
pub(crate) fn wrap(i: usize, len: usize) -> usize {
    if len.is_power_of_two() { i & (len - 1) } else { i % len }
}

#[inline]
fn wrap_get(mem: &[u8], i: usize) -> u8 {
    mem[wrap(i, mem.len())]
}

/// Cartridge memory shared by all boards. Boards read sizes/header bits from it; `Cartridge`
/// does the actual byte accesses.
#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct Mem {
    #[serde(skip)]
    pub prg: RomBytes,
    /// Empty when the board uses CHR RAM.
    #[serde(skip)]
    pub chr_rom: RomBytes,
    pub chr_ram: Vec<u8>,
    pub prg_ram: Vec<u8>,
    pub header_mirroring: Mirroring,
    pub submapper: u8,
}

impl Mem {
    #[inline]
    pub fn chr(&self) -> &[u8] {
        if self.chr_rom.is_empty() { &self.chr_ram } else { &self.chr_rom }
    }

    /// PRG ROM index of `size`-byte bank `bank` at offset `off`, wrapped to the ROM size.
    #[inline]
    pub fn prg_off(&self, bank: usize, size: usize, off: usize) -> usize {
        wrap(bank.wrapping_mul(size) + off, self.prg.len())
    }

    /// Number of 8 KiB PRG banks (at least 1).
    #[inline]
    pub fn prg_8k(&self) -> usize {
        (self.prg.len() / K8).max(1)
    }

    /// Number of 16 KiB PRG banks (at least 1).
    #[inline]
    pub fn prg_16k(&self) -> usize {
        (self.prg.len() / K16).max(1)
    }
}

/// Where a hooked nametable access ($2000-$2FFF) goes (boards with `ppu_hooks()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nt {
    /// 1 KiB page of the console's CIRAM (0-3; 2/3 = the four-screen VRAM half).
    Ciram(usize),
    /// Byte supplied by the cartridge (ExRAM, fill mode, extended attributes).
    Value(u8),
}

/// Board behaviour. `mem` is the shared cartridge memory (read-only for the board).
pub(crate) trait BoardLogic {
    /// PRG ROM index for $8000-$FFFF; `a` = addr - $8000 (0..$8000). May be unwrapped.
    fn prg_index(&self, a: usize, mem: &Mem) -> usize;

    /// CHR index for pattern address `a` ($0000-$1FFF). May be unwrapped.
    fn chr_index(&self, a: usize, mem: &Mem) -> usize;

    /// CPU write of $8000-$FFFF (registers). `cycle` = absolute CPU cycle.
    fn write(&mut self, addr: u16, val: u8, cycle: u64, mem: &Mem);

    /// CPU write of $4020-$5FFF (expansion registers, e.g. MMC5 $5xxx). Default: ignored.
    fn write_low(&mut self, _addr: u16, _val: u8, _cycle: u64, _mem: &Mem) {}

    /// CPU write of $6000-$7FFF, seen by the board before/alongside PRG RAM (boards with
    /// registers there: NINA-001, J87, J-11, Sunsoft-1). Return `true` when banks changed.
    fn write_ram_area(&mut self, _addr: u16, _val: u8) -> bool {
        false
    }

    /// CPU read of $4020-$5FFF with side effects. Default: open bus.
    fn read_low(&mut self, addr: u16, mem: &Mem) -> Option<u8> {
        self.peek_low(addr, mem)
    }

    /// Side-effect-free CPU read of $4020-$5FFF. Default: open bus.
    fn peek_low(&self, _addr: u16, _mem: &Mem) -> Option<u8> {
        None
    }

    /// 8 KiB PRG RAM bank for $6000-$7FFF, `None` = disabled (open bus). Default: bank 0.
    fn prg_ram_bank(&self, _mem: &Mem) -> Option<usize> {
        Some(0)
    }

    /// Whether $6000-$7FFF is write-enabled (when `prg_ram_bank` is `Some`). Default: yes.
    fn prg_ram_writable(&self) -> bool {
        true
    }

    /// PRG ROM index for $6000-$7FFF (`a` = addr - $6000) when the board maps ROM there
    /// (FME-7 command 8); `None` = PRG RAM rules apply. Default: `None`.
    fn prg_low_rom(&self, _a: usize, _mem: &Mem) -> Option<usize> {
        None
    }

    /// PPU pattern access hook before the byte is fetched (read and write), e.g. MMC3 A12.
    /// `dot` is the PPU's absolute dot counter.
    fn ppu_access(&mut self, _addr: u16, _dot: u64) {}

    /// Hook after a pattern read (latch boards: MMC2/MMC4). Return `true` when banks changed
    /// so the slot maps are recomputed.
    fn ppu_after_read(&mut self, _addr: u16) -> bool {
        false
    }

    /// Whether the PPU must route nametable accesses through `nt_read`/`nt_write` and signal
    /// fetch phases via `ppu_fetch` (MMC5). Read once by `Cartridge::new`. Boards answering
    /// `true` take the generic CHR path (no slot map), so `chr_index` may depend on fetch state.
    fn ppu_hooks(&self) -> bool {
        false
    }

    /// PPU entered a fetch phase while rendering: `sprites` = dots 257-320 (sprite patterns),
    /// else background (dots 321-256). `tall` = 8x16 sprites. Only with `ppu_hooks()`.
    fn ppu_fetch(&mut self, _sprites: bool, _tall: bool) {}

    /// Nametable read of $2000-$2FFF (`addr` & $0FFF). Only called with `ppu_hooks()`.
    fn nt_read(&mut self, _addr: u16) -> Nt {
        Nt::Ciram(0)
    }

    /// Nametable write; returns the CIRAM page to store into, `None` = the board consumed it.
    /// Only called with `ppu_hooks()`.
    fn nt_write(&mut self, _addr: u16, _val: u8) -> Option<usize> {
        Some(0)
    }

    /// PRG RAM byte index (unwrapped) for $8000-$FFFF (`a` = addr - $8000) when the board maps
    /// RAM there (MMC5); `None` = ROM. Only consulted with `ppu_hooks()`.
    fn prg_high_ram(&self, _a: usize) -> Option<usize> {
        None
    }

    /// Mirroring override; `None` = header mirroring. Ignored when the header says four-screen.
    fn mirroring(&self, _mem: &Mem) -> Option<Mirroring> {
        None
    }

    /// Cartridge IRQ line level.
    fn irq(&self) -> bool {
        false
    }

    /// Once per CPU cycle (cycle-counting IRQs, expansion audio clocks).
    fn cpu_tick(&mut self) {}

    /// Reset button. RAM is kept.
    fn reset(&mut self) {}

    /// Whether the board has expansion audio (VRC6/VRC7/MMC5/N163/5B/FDS).
    fn has_audio(&self) -> bool {
        false
    }

    /// Current expansion-audio sample, already mixed for the board. Scale: 0..=32767 is the
    /// same loudness range as the 2A03 mixer's full scale (APU output 0.0..=1.0 → 0..=32767);
    /// the bus adds it to the APU sample (`AudioOut::push_ext`). Integer, no allocation,
    /// callable every cycle.
    fn audio_level(&self) -> i32 {
        0
    }
}

#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
enum Board {
    Nrom(nrom::Nrom),
    Mmc1(mmc1::Mmc1),
    Mmc2(mmc2::Mmc2),
    Mmc3(mmc3::Mmc3),
    Uxrom(discrete::Uxrom),
    Cnrom(discrete::Cnrom),
    Axrom(discrete::Axrom),
    ColorDreams(discrete::ColorDreams),
    Gxrom(discrete::Gxrom),
    Camerica(discrete::Camerica),
    Action53(action53::Action53),
    Mmc5(mmc5::Mmc5),
    Fme7(fme7::Fme7),
    Vrc(vrc::Vrc),
    Namco(namco::Namco),
    Misc(misc::Misc),
    Multicart(multicart::Multicart),
}

macro_rules! dispatch {
    ($board:expr, $b:ident => $e:expr) => {
        match $board {
            Board::Nrom($b) => $e,
            Board::Mmc1($b) => $e,
            Board::Mmc2($b) => $e,
            Board::Mmc3($b) => $e,
            Board::Uxrom($b) => $e,
            Board::Cnrom($b) => $e,
            Board::Axrom($b) => $e,
            Board::ColorDreams($b) => $e,
            Board::Gxrom($b) => $e,
            Board::Camerica($b) => $e,
            Board::Action53($b) => $e,
            Board::Mmc5($b) => $e,
            Board::Fme7($b) => $e,
            Board::Vrc($b) => $e,
            Board::Namco($b) => $e,
            Board::Misc($b) => $e,
            Board::Multicart($b) => $e,
        }
    };
}

/// PRG ROM/RAM, CHR ROM/RAM and board registers. Everything except ROM bytes is part of the
/// rollback snapshot, so keep it small and `Clone + Hash`.
#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub struct Cartridge {
    mem: Mem,
    battery: bool,
    board: Board,
    /// Wrapped PRG offset of each 8 KiB slot of $8000-$FFFF; valid when `fast_map`.
    prg_map: [usize; 4],
    /// Wrapped CHR offset of each 1 KiB slot of $0000-$1FFF; valid when `fast_map`.
    chr_map: [usize; 8],
    /// PRG and CHR sizes are powers of two at least one slot large, so a slot never wraps
    /// mid-way and reads are base + offset. Otherwise reads take the generic path.
    fast_map: bool,
    /// `fast_map` for CHR, off for hooked boards whose CHR banks follow the fetch phase.
    chr_fast: bool,
    /// Board's `ppu_hooks()`, cached at construction (the PPU checks it per NT access).
    hooks: bool,
}

impl Cartridge {
    pub fn new(info: &RomInfo) -> Result<Cartridge, RomError> {
        if info.prg_rom.is_empty() {
            return Err(RomError::Invalid("PRG ROM size is zero"));
        }
        let n = info.mapper;
        let board = match n {
            0 => Some(Board::Nrom(nrom::Nrom)),
            1 => Some(Board::Mmc1(mmc1::Mmc1::new())),
            2 => Some(Board::Uxrom(discrete::Uxrom::default())),
            3 => Some(Board::Cnrom(discrete::Cnrom::default())),
            4 => Some(Board::Mmc3(mmc3::Mmc3::new())),
            7 => Some(Board::Axrom(discrete::Axrom::default())),
            9 => Some(Board::Mmc2(mmc2::Mmc2::new())),
            10 => Some(Board::Mmc2(mmc2::Mmc2::new_mmc4())),
            11 => Some(Board::ColorDreams(discrete::ColorDreams::default())),
            28 => Some(Board::Action53(action53::Action53::default())),
            66 => Some(Board::Gxrom(discrete::Gxrom::default())),
            71 => Some(Board::Camerica(discrete::Camerica::default())),
            _ if mmc5::MAPPERS.contains(&n) => mmc5::Mmc5::new(info).map(Board::Mmc5),
            _ if fme7::MAPPERS.contains(&n) => fme7::Fme7::new(info).map(Board::Fme7),
            _ if vrc::MAPPERS.contains(&n) => vrc::Vrc::new(info).map(Board::Vrc),
            _ if namco::MAPPERS.contains(&n) => namco::Namco::new(info).map(Board::Namco),
            _ if misc::MAPPERS.contains(&n) => misc::Misc::new(info).map(Board::Misc),
            _ if multicart::MAPPERS.contains(&n) => {
                multicart::Multicart::new(info).map(Board::Multicart)
            }
            _ => None,
        };
        let board = board.ok_or(RomError::UnsupportedMapper(n))?;
        let chr_ram = if info.chr_rom.is_empty() {
            vec![0; info.chr_ram_size.max(K8)]
        } else {
            Vec::new()
        };
        let mut prg_ram = vec![0; info.prg_ram_size];
        if let Some(trainer) = &info.trainer {
            if prg_ram.len() >= K8 {
                prg_ram[0x1000..0x1200].copy_from_slice(trainer);
            }
        }
        let mem = Mem {
            prg: info.prg_rom.clone(),
            chr_rom: info.chr_rom.clone(),
            chr_ram,
            prg_ram,
            header_mirroring: info.mirroring,
            submapper: info.submapper,
        };
        let chr_len = mem.chr().len();
        let fast_map = mem.prg.len().is_power_of_two()
            && mem.prg.len() >= K8
            && chr_len.is_power_of_two()
            && chr_len >= K1;
        let hooks = dispatch!(&board, b => b.ppu_hooks());
        let mut c = Cartridge {
            mem,
            battery: info.battery,
            board,
            prg_map: [0; 4],
            chr_map: [0; 8],
            fast_map,
            chr_fast: fast_map && !hooks,
            hooks,
        };
        c.remap();
        Ok(c)
    }

    /// Re-attaches the ROM bytes (skipped by serde) of `running`, the cartridge this state was
    /// saved from, and rebuilds the derived bank maps. Errors when the state's board or memory
    /// sizes do not match `running` (corrupt or foreign state).
    pub(crate) fn adopt_rom(&mut self, running: &Cartridge) -> Result<(), ()> {
        if std::mem::discriminant(&self.board) != std::mem::discriminant(&running.board)
            || self.mem.chr_ram.len() != running.mem.chr_ram.len()
            || self.mem.prg_ram.len() != running.mem.prg_ram.len()
            || self.fast_map != running.fast_map
            || self.chr_fast != running.chr_fast
            || self.hooks != running.hooks
        {
            return Err(());
        }
        self.mem.prg = running.mem.prg.clone();
        self.mem.chr_rom = running.mem.chr_rom.clone();
        self.remap();
        Ok(())
    }

    /// Recomputes `prg_map`/`chr_map` after a bank register change.
    fn remap(&mut self) {
        if !self.fast_map {
            return;
        }
        for s in 0..4 {
            self.prg_map[s] = self.prg_index(0x8000 + (s * K8) as u16);
        }
        let len = self.mem.chr().len();
        for s in 0..8 {
            self.chr_map[s] = wrap(self.chr_index((s * K1) as u16), len);
        }
    }

    /// Absolute CHR index (before wrapping) of pattern address `addr`.
    #[inline]
    fn chr_index(&self, addr: u16) -> usize {
        let a = addr as usize & 0x1FFF;
        let mem = &self.mem;
        dispatch!(&self.board, b => b.chr_index(a, mem))
    }

    /// Wrapped PRG ROM index for $8000-$FFFF.
    fn prg_index(&self, addr: u16) -> usize {
        let a = addr as usize - 0x8000;
        let mem = &self.mem;
        wrap(dispatch!(&self.board, b => b.prg_index(a, mem)), mem.prg.len())
    }

    /// Index into `prg_ram` for $6000-$7FFF, or `None` when RAM is absent or disabled.
    #[inline]
    fn prg_ram_index(&self, addr: u16) -> Option<usize> {
        if self.mem.prg_ram.is_empty() {
            return None;
        }
        let off = (addr as usize - 0x6000) & 0x1FFF;
        let mem = &self.mem;
        let bank = dispatch!(&self.board, b => b.prg_ram_bank(mem))?;
        Some(wrap(bank * K8 + off, mem.prg_ram.len()))
    }

    /// PRG ROM byte for $8000-$FFFF.
    #[inline]
    fn prg_read(&self, addr: u16) -> u8 {
        if self.fast_map {
            let a = addr as usize & 0x7FFF;
            return self.mem.prg[self.prg_map[a >> 13] + (a & (K8 - 1))];
        }
        self.mem.prg[self.prg_index(addr)]
    }

    /// Index into `prg_ram` when a hooked board maps RAM at `addr` in $8000-$FFFF.
    #[cold]
    fn high_ram_index(&self, addr: u16) -> Option<usize> {
        if self.mem.prg_ram.is_empty() {
            return None;
        }
        let i = dispatch!(&self.board, b => b.prg_high_ram(addr as usize - 0x8000))?;
        Some(wrap(i, self.mem.prg_ram.len()))
    }

    /// CPU read of $4020-$FFFF. `None` = nothing drives the bus (open bus).
    #[inline]
    pub fn cpu_read(&mut self, addr: u16) -> Option<u8> {
        if addr < 0x6000 {
            let mem = &self.mem;
            return dispatch!(&mut self.board, b => b.read_low(addr, mem));
        }
        self.cpu_peek(addr)
    }

    /// Side-effect-free variant of `cpu_read` for debuggers and tests.
    #[inline]
    pub fn cpu_peek(&self, addr: u16) -> Option<u8> {
        match addr {
            0x8000..=0xFFFF => {
                if self.hooks {
                    if let Some(i) = self.high_ram_index(addr) {
                        return Some(self.mem.prg_ram[i]);
                    }
                }
                Some(self.prg_read(addr))
            }
            0x6000..=0x7FFF => {
                let mem = &self.mem;
                match dispatch!(&self.board, b => b.prg_low_rom(addr as usize - 0x6000, mem)) {
                    Some(i) => Some(mem.prg[wrap(i, mem.prg.len())]),
                    None => self.prg_ram_index(addr).map(|i| mem.prg_ram[i]),
                }
            }
            _ => {
                let mem = &self.mem;
                dispatch!(&self.board, b => b.peek_low(addr, mem))
            }
        }
    }

    /// CPU write of $4020-$FFFF. `cpu_cycle` is the absolute CPU cycle counter (MMC1 ignores
    /// a write on the cycle right after another write, e.g. the dummy write of RMW opcodes).
    pub fn cpu_write(&mut self, addr: u16, val: u8, cpu_cycle: u64) {
        if (0x6000..0x8000).contains(&addr) {
            if dispatch!(&self.board, b => b.prg_ram_writable()) {
                if let Some(i) = self.prg_ram_index(addr) {
                    self.mem.prg_ram[i] = val;
                }
            }
            if dispatch!(&mut self.board, b => b.write_ram_area(addr, val)) {
                self.remap();
            }
            return;
        }
        let mem = &self.mem;
        if addr < 0x6000 {
            dispatch!(&mut self.board, b => b.write_low(addr, val, cpu_cycle, mem));
            // Rare (expansion-area writes); boards like NINA-003/006 switch banks here.
            self.remap();
            return;
        }
        if self.hooks && dispatch!(&self.board, b => b.prg_ram_writable()) {
            if let Some(i) = self.high_ram_index(addr) {
                self.mem.prg_ram[i] = val;
            }
        }
        let mem = &self.mem;
        dispatch!(&mut self.board, b => b.write(addr, val, cpu_cycle, mem));
        self.remap();
    }

    #[inline]
    fn chr_get(&self, addr: u16) -> u8 {
        if self.chr_fast {
            let a = addr as usize & 0x1FFF;
            return self.mem.chr()[self.chr_map[a >> 10] + (a & (K1 - 1))];
        }
        let i = self.chr_index(addr);
        wrap_get(self.mem.chr(), i)
    }

    /// Pattern byte at `addr` with no mapper side effects (render-only overlays).
    #[inline]
    pub(crate) fn ppu_peek(&self, addr: u16) -> u8 {
        self.chr_get(addr)
    }

    /// PPU pattern-table read, `addr` in $0000-$1FFF. `ppu_dot` is the PPU's absolute dot
    /// counter; MMC3 uses it to filter A12 rises (rise counts only when A12 stayed low for
    /// more than ~8 dots).
    #[inline]
    pub fn ppu_read(&mut self, addr: u16, ppu_dot: u64) -> u8 {
        dispatch!(&mut self.board, b => b.ppu_access(addr, ppu_dot));
        let v = self.chr_get(addr);
        if dispatch!(&mut self.board, b => b.ppu_after_read(addr)) {
            self.remap();
        }
        v
    }

    /// PPU pattern-table write (CHR RAM boards), `addr` in $0000-$1FFF.
    pub fn ppu_write(&mut self, addr: u16, val: u8, ppu_dot: u64) {
        dispatch!(&mut self.board, b => b.ppu_access(addr, ppu_dot));
        if self.mem.chr_rom.is_empty() {
            let i = wrap(self.chr_index(addr), self.mem.chr_ram.len());
            self.mem.chr_ram[i] = val;
        }
    }

    /// Whether the board wants nametable/fetch-phase hooks (constant per cartridge).
    #[inline(always)]
    pub fn ppu_hooks(&self) -> bool {
        self.hooks
    }

    /// Hooked nametable read, `addr` in $2000-$3EFF. Only when `ppu_hooks()`.
    #[inline]
    pub fn nt_read(&mut self, addr: u16) -> Nt {
        dispatch!(&mut self.board, b => b.nt_read(addr & 0x0FFF))
    }

    /// Hooked nametable write; `Some(page)` = store in that CIRAM page. Only when `ppu_hooks()`.
    #[inline]
    pub fn nt_write(&mut self, addr: u16, val: u8) -> Option<usize> {
        dispatch!(&mut self.board, b => b.nt_write(addr & 0x0FFF, val))
    }

    /// Fetch-phase signal from a rendering PPU (see `BoardLogic::ppu_fetch`).
    #[inline]
    pub fn ppu_fetch(&mut self, sprites: bool, tall: bool) {
        dispatch!(&mut self.board, b => b.ppu_fetch(sprites, tall))
    }

    /// Current nametable mirroring (boards may switch it at runtime).
    #[inline]
    pub fn mirroring(&self) -> Mirroring {
        let h = self.mem.header_mirroring;
        if h == Mirroring::FourScreen {
            return h;
        }
        let mem = &self.mem;
        dispatch!(&self.board, b => b.mirroring(mem)).unwrap_or(h)
    }

    /// Cartridge IRQ line level.
    #[inline]
    pub fn irq(&self) -> bool {
        dispatch!(&self.board, b => b.irq())
    }

    /// Called once per CPU cycle, for boards with cycle-counting IRQs. May do nothing.
    #[inline]
    pub fn cpu_tick(&mut self) {
        dispatch!(&mut self.board, b => b.cpu_tick())
    }

    /// Reset button: boards that react to reset do it here; RAM is kept.
    pub fn reset(&mut self) {
        dispatch!(&mut self.board, b => b.reset());
        self.remap();
    }

    /// Whether the board produces expansion audio (the bus caches this once).
    pub fn has_expansion_audio(&self) -> bool {
        dispatch!(&self.board, b => b.has_audio())
    }

    /// Current expansion-audio level, scale documented on `BoardLogic::audio_level`.
    #[inline]
    pub fn expansion_audio(&self) -> i32 {
        dispatch!(&self.board, b => b.audio_level())
    }

    /// Battery-backed PRG RAM, if the header has the battery flag.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.battery.then_some(&self.mem.prg_ram[..])
    }

    pub fn load_battery_ram(&mut self, data: &[u8]) {
        let n = data.len().min(self.mem.prg_ram.len());
        self.mem.prg_ram[..n].copy_from_slice(&data[..n]);
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use crate::cart::{Region, RomBytes};
    use std::sync::Arc;

    /// Every PRG 16 KiB bank is filled with its bank number, every CHR 4 KiB bank too.
    pub fn info(mapper: u16, prg_banks_16k: usize, chr_banks_4k: usize) -> RomInfo {
        let prg: Vec<u8> = (0..prg_banks_16k).flat_map(|b| std::iter::repeat_n(b as u8, K16)).collect();
        let chr: Vec<u8> = (0..chr_banks_4k).flat_map(|b| std::iter::repeat_n(b as u8, K4)).collect();
        RomInfo {
            mapper,
            submapper: 0,
            prg_rom: RomBytes(Arc::from(prg)),
            chr_rom: RomBytes(Arc::from(chr)),
            chr_ram_size: if chr_banks_4k == 0 { K8 } else { 0 },
            prg_ram_size: K8,
            battery: true,
            mirroring: Mirroring::Horizontal,
            region: Region::Ntsc,
            four_score: false,
            crc32: 0,
            trainer: None,
            esx: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::info;
    use super::*;

    #[test]
    fn mapper_unsupported_rejected() {
        assert_eq!(Cartridge::new(&info(250, 2, 2)).err(), Some(RomError::UnsupportedMapper(250)));
    }

    #[test]
    fn mapper_planned_tables_disjoint_from_supported() {
        let mut seen = std::collections::HashSet::new();
        for (_, list) in PLANNED {
            for n in *list {
                assert!(seen.insert(*n), "mapper {n} planned twice");
                assert!(!SUPPORTED_MAPPERS.contains(n) || Cartridge::new(&info(*n, 2, 2)).is_ok());
            }
        }
    }
}
