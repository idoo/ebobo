//! CPU address space, controllers (incl. Four Score), OAM/DMC DMA and CPU:PPU clock ratio.
//! Owner: bus slice.

use crate::apu::{Apu, AudioOut};
use crate::cart::{Region, RomError, RomInfo};
use crate::cpu::CpuBus;
use crate::mapper::Cartridge;
use crate::ppu::Ppu;
use crate::{HEIGHT, WIDTH};

/// Four Score signature bits, placed at reads 17-24 of each port. nesdev wiki "Four Score"
/// (https://www.nesdev.org/wiki/Four_Score): reads 17-24 return the ID `0,0,0,1,0,0,0,0`
/// on $4016 and `0,0,1,0,0,0,0,0` on $4017 (in read order), i.e. read 20 is 1 on port 1
/// and read 19 is 1 on port 2. LSB-first that is byte $08 / $04.
const FOUR_SCORE_SIG: [u32; 2] = [0x08, 0x04];

/// Everything the rollback snapshot needs besides the CPU: RAM, PPU, APU, cartridge,
/// controller latches, DMA and interrupt latches, cycle counters.
#[derive(Clone, Hash, serde::Serialize, serde::Deserialize)]
pub struct BusState {
    #[serde(with = "crate::big_array")]
    ram: [u8; 0x800],
    ppu: Ppu,
    apu: Apu,
    cart: Cartridge,
    pads: [u8; 4],
    /// Controller shift registers for $4016/$4017; bit 0 is the next bit read.
    shift: [u32; 2],
    strobe: bool,
    four_score: bool,
    open_bus: u8,
    /// CPU cycles since power-on.
    cycles: u64,
    /// Master clocks owed to the PPU.
    master_acc: u8,
    /// Master clocks per CPU cycle / per PPU dot (NTSC 12/4, PAL 16/5).
    cpu_clocks: u8,
    dot_clocks: u8,
    nmi_prev: bool,
    nmi_pending: bool,
    frame_done: bool,
    /// Set while OAM DMA runs (DMC DMA inside it is serviced with the same stall model).
    in_oam_dma: bool,
}

/// `BusState` plus outputs that are not part of the snapshot (frame buffer, audio).
pub struct Bus {
    s: BusState,
    fb: Box<[u16]>,
    audio: AudioOut,
    /// Host setting: skip frame-buffer composition (not part of the snapshot).
    fast_forward: bool,
    /// `cart.has_expansion_audio()`, cached: fixed for the cartridge's lifetime.
    has_ext: bool,
    /// Host setting: hardware 8-sprites-per-line limit (render-only, not in the snapshot).
    sprite_limit: bool,
    /// Host setting: keep the background and the sprites of each picture apart in `planes`
    /// (display-only, not in the snapshot).
    tilt: bool,
    /// Background plane then sprite plane, `WIDTH * HEIGHT` words each (`Ppu::tick_planes`).
    planes: Box<[u16]>,
    /// The picture being drawn has had its planes written from its first line.
    planes_whole: bool,
    /// `planes` hold a whole picture, the one in `fb`.
    planes_valid: bool,
}

impl Bus {
    pub fn new(info: &RomInfo) -> Result<Bus, RomError> {
        let region = info.region;
        let cart = Cartridge::new(info)?;
        let has_ext = cart.has_expansion_audio();
        let (cpu_clocks, dot_clocks) = match region {
            Region::Ntsc => (12, 4),
            Region::Pal => (16, 5),
        };
        let mut ppu = Ppu::new(region);
        ppu.set_esx(info.esx.clone());
        Ok(Bus {
            s: BusState {
                ram: [0; 0x800],
                ppu,
                apu: Apu::new(region),
                cart,
                pads: [0; 4],
                shift: [0; 2],
                strobe: false,
                four_score: false,
                open_bus: 0,
                cycles: 0,
                master_acc: 0,
                cpu_clocks,
                dot_clocks,
                nmi_prev: false,
                nmi_pending: false,
                frame_done: false,
                in_oam_dma: false,
            },
            fb: vec![0u16; WIDTH * HEIGHT].into_boxed_slice(),
            audio: AudioOut::new(region, 48_000.0),
            fast_forward: false,
            has_ext,
            sprite_limit: true,
            tilt: false,
            planes: vec![0u16; 2 * WIDTH * HEIGHT].into_boxed_slice(),
            planes_whole: false,
            planes_valid: false,
        })
    }

    /// Reset button for PPU, APU and cartridge (RAM keeps its contents).
    pub fn reset(&mut self) {
        let s = &mut self.s;
        s.ppu.reset();
        s.apu.reset();
        s.cart.reset();
        s.shift = [0; 2];
        s.strobe = false;
        s.nmi_prev = false;
        s.nmi_pending = false;
        s.in_oam_dma = false;
    }

    /// Controller states for the next frame, controller 1..4.
    pub fn set_pads(&mut self, pads: [u8; 4]) {
        self.s.pads = pads;
        if self.s.strobe {
            self.latch_pads();
        }
    }

    pub fn set_four_score(&mut self, on: bool) {
        self.s.four_score = on;
    }

    pub fn four_score(&self) -> bool {
        self.s.four_score
    }

    /// True once per frame when the PPU entered vblank during the last CPU step.
    pub fn take_frame_done(&mut self) -> bool {
        core::mem::take(&mut self.s.frame_done)
    }

    pub fn frame(&self) -> &[u16] {
        &self.fb
    }

    pub fn audio(&mut self) -> &mut AudioOut {
        &mut self.audio
    }

    pub fn set_fast_forward(&mut self, on: bool) {
        if on != self.fast_forward {
            // A picture drawn partly leaves the planes mixed.
            self.planes_whole = false;
        }
        self.fast_forward = on;
        self.audio.set_muted(on);
    }

    /// Turns the tilt planes on or off. Off forgets them; on, they become readable once a
    /// whole picture was drawn. Restores and state loads leave the setting alone.
    pub fn set_tilt(&mut self, on: bool) {
        if on == self.tilt {
            return;
        }
        self.tilt = on;
        self.planes_whole = false;
        self.planes_valid = false;
    }

    /// Background and sprite plane of the picture in [`Bus::frame`], once tilt is on and a
    /// whole picture was drawn with it.
    pub fn tilt_planes(&self) -> Option<(&[u16], &[u16])> {
        self.planes_valid.then(|| self.planes.split_at(WIDTH * HEIGHT))
    }

    pub fn state(&self) -> &BusState {
        &self.s
    }

    pub fn restore(&mut self, state: &BusState) {
        self.s.clone_from(state);
        self.s.ppu.set_sprite_limit(self.sprite_limit);
    }

    pub fn set_sprite_limit(&mut self, on: bool) {
        self.sprite_limit = on;
        self.s.ppu.set_sprite_limit(on);
    }

    /// Installs a deserialized state: re-attaches the ROM-derived parts serde skips (ROM
    /// bytes, ESX banks) from the running machine. Fails, leaving `self` untouched, when the
    /// state does not fit this cartridge.
    pub fn load_state(&mut self, mut state: BusState) -> Result<(), ()> {
        state.cart.adopt_rom(&self.s.cart)?;
        state.ppu.adopt_rom(&self.s.ppu);
        self.s = state;
        Ok(())
    }

    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.s.cart.battery_ram()
    }

    pub fn load_battery_ram(&mut self, data: &[u8]) {
        self.s.cart.load_battery_ram(data);
    }

    /// Debug write to internal RAM ($0000-$1FFF, mirrored). Other addresses are ignored: tools
    /// use it to set game variables, never to emulate bus writes (no side effects, no cycles).
    pub fn poke_ram(&mut self, addr: u16, val: u8) {
        if addr < 0x2000 {
            self.s.ram[(addr & 0x7FF) as usize] = val;
        }
    }

    /// Side-effect-free CPU address space read.
    pub fn peek(&self, addr: u16) -> u8 {
        let s = &self.s;
        match addr {
            0x0000..=0x1FFF => s.ram[(addr & 0x7FF) as usize],
            0x2000..=0x3FFF => s.ppu.peek_register(addr),
            0x4015 => s.apu.peek_status() | (s.open_bus & 0x20),
            0x4016 | 0x4017 => (s.open_bus & 0xE0) | self.port_bit((addr & 1) as usize),
            0x4020..=0xFFFF => s.cart.cpu_peek(addr).unwrap_or(s.open_bus),
            _ => s.open_bus,
        }
    }

    fn latch_pads(&mut self) {
        let s = &mut self.s;
        let p = s.pads.map(u32::from);
        if s.four_score {
            s.shift[0] = p[0] | p[2] << 8 | FOUR_SCORE_SIG[0] << 16 | 0xFF00_0000;
            s.shift[1] = p[1] | p[3] << 8 | FOUR_SCORE_SIG[1] << 16 | 0xFF00_0000;
        } else {
            s.shift[0] = p[0] | 0xFFFF_FF00;
            s.shift[1] = p[1] | 0xFFFF_FF00;
        }
    }

    #[inline]
    fn port_bit(&self, port: usize) -> u8 {
        if self.s.strobe {
            self.s.pads[port] & 1
        } else {
            (self.s.shift[port] & 1) as u8
        }
    }

    /// One CPU cycle of everything that is not the CPU.
    #[inline]
    fn clock(&mut self) {
        {
            let s = &mut self.s;
            // An empty slice tells the PPU to skip pixel composition.
            let draw = !self.fast_forward;
            let fb: &mut [u16] = if draw { &mut self.fb } else { &mut [] };
            let planes: &mut [u16] = if draw && self.tilt { &mut self.planes } else { &mut [] };
            if s.cpu_clocks == 12 {
                // NTSC: exactly 3 dots per CPU cycle; `master_acc` stays 0.
                s.ppu.tick_planes(&mut s.cart, fb, planes);
                s.ppu.tick_planes(&mut s.cart, fb, planes);
                s.ppu.tick_planes(&mut s.cart, fb, planes);
            } else {
                s.master_acc += s.cpu_clocks;
                while s.master_acc >= s.dot_clocks {
                    s.master_acc -= s.dot_clocks;
                    s.ppu.tick_planes(&mut s.cart, fb, planes);
                }
            }
            // NMI-edge and frame-done latching once per CPU cycle: inside `tick` the NMI line
            // only rises at 241/1 and falls at -1/1, never both within one cycle's dots, so
            // this equals sampling after every dot.
            let line = s.ppu.nmi_line();
            if line && !s.nmi_prev {
                s.nmi_pending = true;
            }
            s.nmi_prev = line;
            if s.ppu.take_frame_done() {
                s.frame_done = true;
                // The picture ended; the next one starts with the settings as they are now.
                let on = draw && self.tilt;
                if on && self.planes_whole {
                    self.planes_valid = true;
                }
                self.planes_whole = on;
            }
        }
        let s = &mut self.s;
        if self.has_ext {
            self.audio.push_ext(s.cart.expansion_audio());
        }
        s.apu.tick(&mut self.audio);
        s.cart.cpu_tick();
        s.cycles += 1;
    }

    /// DMC sample fetch: the CPU is halted for 3 cycles, the 4th reads the sample byte.
    #[inline]
    fn service_dmc(&mut self) {
        if let Some(addr) = self.s.apu.dmc_dma_request() {
            self.clock();
            self.clock();
            self.clock();
            self.clock();
            let v = self.read_mem(addr);
            self.s.apu.dmc_dma_complete(v);
        }
    }

    /// Address decode of a read (no clocking). Updates open bus.
    #[inline]
    fn read_mem(&mut self, addr: u16) -> u8 {
        let s = &mut self.s;
        let v = match addr {
            0x0000..=0x1FFF => s.ram[(addr & 0x7FF) as usize],
            0x2000..=0x3FFF => {
                if addr & 7 == 2 && s.ppu.nmi_cancel_window() {
                    s.nmi_pending = false;
                }
                s.ppu.read_register(addr, &mut s.cart)
            }
            0x4015 => s.apu.read_status() | (s.open_bus & 0x20),
            0x4016 | 0x4017 => {
                let port = (addr & 1) as usize;
                let bit = if s.strobe {
                    s.pads[port] & 1
                } else {
                    let b = (s.shift[port] & 1) as u8;
                    s.shift[port] = s.shift[port] >> 1 | 0x8000_0000;
                    b
                };
                (s.open_bus & 0xE0) | bit
            }
            0x4020..=0xFFFF => s.cart.cpu_read(addr).unwrap_or(s.open_bus),
            _ => s.open_bus,
        };
        s.open_bus = v;
        v
    }

    fn oam_dma(&mut self, page: u8) {
        self.s.in_oam_dma = true;
        // Halt cycle, plus one alignment cycle when the DMA would start on a put cycle.
        self.clock();
        if self.s.cycles & 1 == 1 {
            self.clock();
        }
        let base = u16::from(page) << 8;
        for i in 0..256u16 {
            self.service_dmc();
            self.clock();
            let v = self.read_mem(base | i);
            self.clock();
            self.s.ppu.oam_dma_write(v);
        }
        self.s.in_oam_dma = false;
    }
}

impl CpuBus for Bus {
    #[inline]
    fn read(&mut self, addr: u16) -> u8 {
        self.service_dmc();
        self.clock();
        self.read_mem(addr)
    }

    #[inline]
    fn write(&mut self, addr: u16, val: u8) {
        self.clock();
        let s = &mut self.s;
        s.open_bus = val;
        match addr {
            0x0000..=0x1FFF => s.ram[(addr & 0x7FF) as usize] = val,
            0x2000..=0x3FFF => {
                if addr & 7 == 0 && val & 0x80 == 0 && s.ppu.nmi_cancel_window() {
                    s.nmi_pending = false;
                }
                s.ppu.write_register(addr, val, &mut s.cart);
                if addr & 7 == 0 {
                    // Enabling NMI while vblank is set raises the line right at the write.
                    let line = s.ppu.nmi_line();
                    if line && !s.nmi_prev {
                        s.nmi_pending = true;
                    }
                    s.nmi_prev = line;
                }
            }
            0x4014 => self.oam_dma(val),
            0x4016 => {
                let strobe = val & 1 != 0;
                let falling = s.strobe && !strobe;
                s.strobe = strobe;
                if strobe || falling {
                    self.latch_pads();
                }
            }
            0x4000..=0x4013 | 0x4015 | 0x4017 => s.apu.write_register(addr, val),
            0x4018..=0x401F => {}
            0x4020..=0xFFFF => {
                let cycle = s.cycles;
                s.cart.cpu_write(addr, val, cycle);
            }
        }
    }

    #[inline]
    fn nmi_pending(&self) -> bool {
        self.s.nmi_pending
    }

    #[inline]
    fn ack_nmi(&mut self) {
        self.s.nmi_pending = false;
    }

    #[inline]
    fn irq_line(&self) -> bool {
        self.s.apu.irq() || self.s.cart.irq()
    }
}
