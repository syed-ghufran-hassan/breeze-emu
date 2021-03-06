//! This module glues everything together and coordinates emulation.

use dma::*;
use input::Input;
use log_util::LogOnPanic;
use ppu::{FrameBuf, Ppu};
use rom::Rom;
use save::SaveStateFormat;

use spc700::Spc700;
use wdc65816::{Cpu, Mem};
use breeze_backend::{BackendAction, BackendResult, Renderer, AudioSink};

use std::cmp;
use std::env;
use std::fs::File;
use std::io::BufReader;


const CPU_CYCLE: i32 = 6;

pub const WRAM_SIZE: usize = 128 * 1024;
byte_array!(pub Wram[WRAM_SIZE] with save state please);

/// Contains everything connected to the CPU via one of the two address buses. All memory accesses
/// will be directed through this.
pub struct Peripherals {
    pub apu: Spc700,
    pub ppu: Ppu,
    pub rom: Rom,
    /// The 128 KB of working RAM of the SNES (separate from cartridge RAM)
    pub wram: Wram,
    pub input: Input,

    /// `$2181` - WMADDL: WRAM Address low byte
    wmaddl: u8,
    /// `$2182` - WMADDM: WRAM Address middle byte
    wmaddm: u8,
    /// `$2183` - WMADDH: WRAM Address high byte
    wmaddh: u8,
    /// `$4300 - $438A`
    pub dma: [DmaChannel; 8],
    /// `$420c` - HDMAEN: HDMA enable flags
    /// (Note that general DMA doesn't have a register here, since all transactions are started
    /// immediately and the register can't be read)
    hdmaen: u8,
    /// `$4200` - NMITIMEN: Interrupt enable flags
    /// `n-xy---a`
    /// * `n`: Enable NMI on V-Blank
    /// * `x`: Enable IRQ on H-Counter match
    /// * `y`: Enable IRQ on V-Counter match
    /// * `a`: Enable auto-joypad read
    nmien: u8,
    /// `$4201` - WRIO: Programmable I/O Port (out-port)
    /// `abxxxxxx`
    /// * `a`: Connected to Port 0 `IOBit`
    /// * `b`: Connected to Port 1 `IOBit`
    /// * `x`: Not connected
    ///
    /// Any bit set to 0 will be 0 when read from `$4213`. If `a` is 0, reading `$2137` will not
    /// latch the H/V Counters.
    wrio: u8,
    /// `$4202` - WRMPYA: Multiplicand 1
    wrmpya: u8,
    /// `$4203` - WRMPYB: Multiplicand 2
    ///
    /// Writing here will trigger the multiplication
    wrmpyb: u8,
    /// `$4204`/`$4205` - WRDIVH/WRDIVL: Dividend
    wrdiv: u16,
    /// `$4216`/`$4217` - RDDIVH/RDDIVL: Unsigned Division Result (Quotient)
    rddiv: u16,
    /// `$4216`/`$4217` - RDMPYH/RDMPYL: Unsigned Division Remainder / Multiply Product
    rdmpy: u16,
    /// `$4207`/`$4208` - HTIMEL/HTIMEH: H Timer (9-bit value)
    htime: u16,
    /// `$4209`/`$420a` - VTIMEL/VTIMEH: V Timer (9-bit value)
    vtime: u16,
    /// `$420D` - MEMSEL: ROM Access Speed
    /// `-------f`
    /// * `f`: FastROM enable
    memsel: bool,
    /// `$4210` NMI flag and 5A22 Version (the version is constant)
    /// `n---vvvv`
    /// * `n`: `self.nmi`
    /// * `v`: Version
    nmi: bool,
    /// `$4211` TIMEUP - IRQ flag
    /// `i-------`
    /// * `i`: IRQ flag (cleared on read)
    irq: bool,

    /// Additional cycles spent doing IO (in master clock cycles). This is added to the cycle count
    /// returned by the CPU and then reset to 0.
    cy: u32,
}

impl_save_state!(Peripherals {
    apu, ppu, rom, wram, dma, hdmaen, nmien, wrio, wrmpya, wrmpyb, wrdiv, rddiv, rdmpy, htime,
    vtime, memsel, nmi, irq, cy, input, wmaddl, wmaddm, wmaddh
} ignore {});

impl Peripherals {
    pub fn new(rom: Rom, input: Input) -> Peripherals {
        Peripherals {
            rom: rom,
            input: input,
            wmaddl: 0,
            wmaddm: 0,
            wmaddh: 0,
            wrdiv: 0xffff,
            htime: 0x1ff,
            vtime: 0x1ff,
            memsel: false,
            wrio: 0xff,

            apu: Spc700::default(),
            ppu: Ppu::default(),
            wram: Wram::default(),
            dma: [DmaChannel::default(); 8],
            hdmaen: 0x00,
            nmien: 0x00,
            wrmpya: 0xff,
            wrmpyb: 0,
            rddiv: 0,
            rdmpy: 0,
            nmi: false,
            irq: false,
            cy: 0,
        }
    }

    fn nmi_enabled(&self) -> bool { self.nmien & 0x80 != 0 }
    fn v_irq_enabled(&self) -> bool { self.nmien & 0x10 != 0 }
    fn h_irq_enabled(&self) -> bool { self.nmien & 0x20 != 0 }

    /// Adds the time needed to access the given memory location to the cycle counter.
    fn do_io_cycle(&mut self, bank: u8, addr: u16) {
        const FAST: u32 = 0;
        const SLOW: u32 = 2;
        const XSLOW: u32 = 6;

        self.cy += match bank {
            0x00 ... 0x3f => match addr {
                0x0000 ... 0x1fff | 0x6000 ... 0xffff => SLOW,
                0x4000 ... 0x41ff => XSLOW,
                _ => FAST,
            },
            0x40 ... 0x7f => SLOW,
            0x80 ... 0xbf => match addr {
                0x0000 ... 0x1fff | 0x6000 ... 0x7fff => SLOW,
                0x4000 ... 0x41ff => XSLOW,
                0x8000 ... 0xffff => if self.memsel { FAST } else { SLOW },
                _ => FAST
            },
            0xc0 ... 0xff => if self.memsel { FAST } else { SLOW },
            _ => FAST,
        }
    }

    fn get_and_inc_wram_addr(&mut self) -> usize {
        let addr = (self.wmaddh as usize) << 16 |
                   (self.wmaddm as usize) << 8 |
                   (self.wmaddl as usize);

        let new_addr = addr + 1;
        self.wmaddl = new_addr as u8;
        self.wmaddm = (new_addr >> 8) as u8;
        self.wmaddh = (new_addr >> 16) as u8 & 1;
        addr
    }
}

impl Mem for Peripherals {
    fn load(&mut self, bank: u8, addr: u16) -> u8 {
        self.do_io_cycle(bank, addr);
        match bank {
            0x00 ... 0x3f | 0x80 ... 0xbf => match addr {
                // Mirror of first 8k of WRAM
                0x0000 ... 0x1fff => self.wram[addr as usize],
                // PPU
                0x2100 ... 0x2133 => {
                    once!(warn!("read from write-only PPU register ${:04X}", addr));
                    0
                }
                0x2134 ... 0x213f => self.ppu.load(addr),
                // APU IO registers
                0x2140 ... 0x217f => self.apu.read_port((addr & 0b11) as u8),
                0x2180 => {
                    let addr = self.get_and_inc_wram_addr();
                    self.wram[addr]
                }
                0x2181 ... 0x2183 => {
                    once!(warn!("open-bus load from WRAM register ${:02X}", addr));
                    0   // FIXME Emulate open-bus
                }
                0x4016 | 0x4017 => self.input.load(addr),
                0x4202 => self.wrmpya,
                0x4203 => self.wrmpyb,
                0x4210 => {
                    const CPU_VERSION: u8 = 2;  // FIXME Is 2 okay in all cases? Does anyone care?
                    let nmi = if self.nmi { 0x80 } else { 0 };
                    self.nmi = false;   // Cleared on read
                    nmi | CPU_VERSION
                }
                0x4211 => {
                    let val = if self.irq { 0x80 } else { 0 };
                    self.irq = false;
                    val
                }
                // HVBJOY - PPU Status
                0x4212 => {
                    // `vh-----a`
                    // V-Blank, H-Blank, Auto-Joypad-Read in progress
                    // FIXME: Use exact timings and set `a`
                    (if self.ppu.in_v_blank() { 0x80 } else { 0 }) +
                    (if self.ppu.in_h_blank() { 0x40 } else { 0 })
                }
                // RDDIVL - Unsigned Division Result (Quotient) (lower 8bit)
                0x4214 => self.rddiv as u8,
                // RDDIVH - Unsigned Division Result (Quotient) (upper 8bit)
                0x4215 => (self.rddiv >> 8) as u8,
                // RDMPYL
                0x4216 => self.rdmpy as u8,
                // RDMPYH
                0x4217 => (self.rdmpy >> 8) as u8,
                // Input ports
                0x4218 ... 0x421f => self.input.load(addr),
                // DMA channels (0x43xr, where x is the channel and r is the channel register)
                0x4300 ... 0x43ff => self.dma[(addr as usize & 0x00f0) >> 4].load(addr as u8 & 0xf),
                0x6000 ... 0xffff => self.rom.load(bank, addr),
                _ => {
                    once!(warn!("invalid/unimplemented load from ${:02X}:{:04X}", bank, addr));
                    0
                }
            },
            // WRAM banks. The first 8k are mapped into the start of all banks.
            0x7e | 0x7f => self.wram[(bank as usize - 0x7e) * 65536 + addr as usize],
            0x40 ... 0x7d | 0xc0 ... 0xff => self.rom.load(bank, addr),
            _ => unreachable!(),    // Rust should know this!
        }
    }

    fn store(&mut self, bank: u8, addr: u16, value: u8) {
        self.do_io_cycle(bank, addr);
        match bank {
            0x00 ... 0x3f | 0x80 ... 0xbf => match addr {
                0x0000 ... 0x1fff => self.wram[addr as usize] = value,
                // PPU registers. Let it deal with the access.
                0x2100 ... 0x2133 => self.ppu.store(addr, value),
                0x2134 ... 0x213f => once!(warn!("store to read-only PPU register ${:04X}", addr)),
                // APU IO registers.
                0x2140 ... 0x217f => self.apu.store_port((addr & 0b11) as u8, value),
                0x2180 => {
                    let addr = self.get_and_inc_wram_addr();
                    self.wram[addr] = value;
                }
                0x2181 => self.wmaddl = value,
                0x2182 => self.wmaddm = value,
                0x2183 => self.wmaddh = value & 1,
                0x2184 ... 0x21ff => once!(warn!("invalid store: ${:02X} to ${:02X}:{:04X}", value,
                    bank, addr)),
                0x4016 => self.input.store(addr, value),
                0x4200 => {
                    // NMITIMEN - NMI/IRQ enable
                    // E-HV---J
                    // E: Enable NMI
                    // H: Enable IRQ on H-Counter
                    // V: Enable IRQ on V-Counter
                    // J: Enable Auto-Joypad-Read

                    // Check useless bits
                    if value & 0x4e != 0 { once!(warn!("Invalid value for NMIEN: ${:02X}", value)) }
                    self.nmien = value;
                }
                0x4201 => {
                    // FIXME: Propagate to controller ports and the I/O read port
                    self.wrio = value;
                    self.ppu.can_latch_counters = value & 0x80 != 0;
                }
                0x4202 => self.wrmpya = value,
                // WRMPYB: Performs multiplication on write
                0x4203 => {
                    self.wrmpyb = value;
                    self.rdmpy = self.wrmpya as u16 * value as u16;
                }
                0x4204 => self.wrdiv = (self.wrdiv & 0xff00) | value as u16,
                0x4205 => self.wrdiv = ((value as u16) << 8) | (self.wrdiv & 0xff),
                // WRDIVB: Performs division on write
                0x4206 => {
                    self.rddiv = if value == 0 { 0xffff } else { self.wrdiv / value as u16 };
                    self.rdmpy = if value == 0 { value as u16 } else { self.wrdiv % value as u16 };
                }
                0x4207 => self.htime = (self.htime & 0xff00) | value as u16,
                0x4208 => {
                    assert!(value & 0x01 == value, "invalid value for $4207: ${:02X}", value);
                    self.htime = ((value as u16) << 8) | (self.htime & 0xff);
                }
                0x4209 => self.vtime = (self.vtime & 0xff00) | value as u16,
                0x420a => {
                    assert!(value & 0x01 == value, "invalid value for $4209: ${:02X}", value);
                    self.vtime = ((value as u16) << 8) | (self.vtime & 0xff);
                }
                // MDMAEN - Party enable
                0x420b => self.cy += do_dma(self, value),
                // HDMAEN - HDMA enable
                0x420c => self.hdmaen = value,
                // MEMSEL - FastROM select
                // (FIXME Maybe warn when unused bits are set)
                0x420d => self.memsel = value & 0x01 != 0,
                // DMA channels (0x43xr, where x is the channel and r is the channel register)
                0x4300 ... 0x43ff => {
                    self.dma[(addr as usize & 0x00f0) >> 4].store(addr as u8 & 0xf, value);
                }
                0x8000 ... 0xffff => self.rom.store(bank, addr, value),
                _ => panic!("invalid store: ${:02X} to ${:02X}:{:04X}", value, bank, addr)
            },
            // WRAM main banks
            0x7e | 0x7f => self.wram[(bank as usize - 0x7e) * 65536 + addr as usize] = value,
            0x40 ... 0x7d | 0xc0 ... 0xff => self.rom.store(bank, addr, value),
            _ => unreachable!(),    // Rust should know this!
        }
    }
}

/// SNES system state
///
/// Contains all registers, RAMs, cartridge memory, timing information, latches, flip-flops, etc.
pub struct Snes {
    cpu: Cpu<Peripherals>,
    master_cy: u64,
    /// Master clock cycles for the APU not yet accounted for (can be negative)
    apu_master_cy_debt: i32,
    /// Master clock cycles for the PPU not yet accounted for (can be negative)
    ppu_master_cy_debt: i32,
    /// Master cycle at which the emulator should enable CPU and APU tracing. This will print all
    /// opcodes as they are executed (as long as the `trace` log level is enabled).
    trace_start: u64,
}

impl_save_state!(Snes { cpu, master_cy, apu_master_cy_debt, ppu_master_cy_debt }
    ignore { trace_start });

impl Snes {
    pub fn new(rom: Rom) -> Self {
        Snes {
            cpu: Cpu::new(Peripherals::new(rom, Input::default())),
            master_cy: 0,
            apu_master_cy_debt: 0,
            ppu_master_cy_debt: 0,
            trace_start: !0,
        }
    }

    /// Get a reference to the `Peripherals` instance
    pub fn peripherals(&self) -> &Peripherals { &self.cpu.mem }

    /// Get a mutable reference to the `Peripherals` instance
    pub fn peripherals_mut(&mut self) -> &mut Peripherals { &mut self.cpu.mem }

    /// Runs emulation until the next frame is completed.
    pub fn render_frame<F>(&mut self, mut render: F) -> BackendResult<Vec<BackendAction>>
    where F: FnMut(&FrameBuf) -> BackendResult<Vec<BackendAction>> {
        /// Approximated APU clock divider. It's actually somewhere around 20.9..., which is why we
        /// can't directly use `MASTER_CLOCK_FREQ / APU_CLOCK_FREQ` (it would round down, which
        /// might not be critical, but better safe than sorry).
        const APU_DIVIDER: i32 = 21;

        let working_cy = LogOnPanic::new("cycle count", self.master_cy);

        loop {
            // Store an action we should perform.
            let mut actions = vec![];
            let mut frame_rendered = false;

            if self.master_cy >= self.trace_start {
                self.cpu.trace = true;
                self.cpu.mem.apu.trace = true;
            }

            // Run a CPU instruction and calculate the master cycles elapsed
            let cpu_master_cy = self.cpu.dispatch() as i32 * CPU_CYCLE + self.cpu.mem.cy as i32;
            self.cpu.mem.cy = 0;

            // In case the CPU did no work, we pretend that it still took a few cycles. This happens
            // if a WAI instruction was executed and the CPU is doing nothing while waiting for an
            // interrupt. We need to emulate the rest of the SNES to some degree or everything
            // freezes. This should probably be fixed in a better way.
            let cpu_master_cy = cmp::max(3, cpu_master_cy); // HACK: Use at least 3 master cycles
            self.master_cy += cpu_master_cy as u64;

            // Now we "owe" the other components a few cycles:
            self.apu_master_cy_debt += cpu_master_cy;
            self.ppu_master_cy_debt += cpu_master_cy;

            // Run all components until we no longer owe them:
            while self.apu_master_cy_debt > APU_DIVIDER {
                // (Since the APU uses lots of cycles to do stuff - lower clock rate and such - we
                // only run it if we owe it `APU_DIVIDER` master cycles - or one SPC700 cycle)
                let apu_master_cy = self.cpu.mem.apu.dispatch() as i32 * APU_DIVIDER;
                self.apu_master_cy_debt -= apu_master_cy;
            }
            while self.ppu_master_cy_debt > 0 {
                let cy = self.cpu.mem.ppu.update();
                self.ppu_master_cy_debt -= cy as i32;

                let (v, h) = (self.cpu.mem.ppu.v_counter(), self.cpu.mem.ppu.h_counter());
                match (v, h) {
                    (0, 0) => self.cpu.mem.nmi = false,
                    (0, 6) => {
                        let channels = self.cpu.mem.hdmaen;
                        self.cpu.mem.cy += init_hdma(&mut self.cpu.mem, channels);
                    }
                    (0 ... 224, 278) => {
                        // FIXME: 224 or 239, depending on overscan
                        let channels = self.cpu.mem.hdmaen;
                        self.cpu.mem.cy += do_hdma(&mut self.cpu.mem, channels);
                    }
                    (224, 256) => {
                        // Last pixel in the current frame was rendered
                        for action in try!(render(&self.cpu.mem.ppu.framebuf)) {
                            actions.push(action);
                        }
                        frame_rendered = true;
                    }
                    (225, 0) => {
                        // First V-Blank pixel
                        self.cpu.mem.input.new_frame();

                        // FIXME This timing is wrong, the NMI flag is set later
                        self.cpu.mem.nmi = true;
                        if self.cpu.mem.nmi_enabled() {
                            self.cpu.trigger_nmi();
                            // XXX Break to handle the NMI immediately. Let's hope we don't owe the PPU
                            // too many cycles.
                            break;
                        }
                    }
                    (225, 50) => {
                        // Auto-Joypad read
                        // "This begins between dots 32.5 and 95.5 of the first V-Blank scanline,
                        // and ends 4224 master cycles later."
                        // FIXME start this at the right position
                        // FIXME Set auto read status bit
                        if self.cpu.mem.nmien & 1 != 0 {
                            self.cpu.mem.input.perform_auto_read();
                        }
                    }
                    (_, 180) => {
                        // Approximate DRAM refresh (FIXME Probably incorrect, but does it matter?)
                        self.cpu.mem.cy += 40;
                    }
                    _ => {}
                }

                {
                    let cpu = &mut self.cpu;
                    if cpu.mem.ppu.v_counter() == cpu.mem.vtime && cpu.mem.v_irq_enabled() {
                        //trace!("V-IRQ at V={}", cpu.mem.ppu.v_counter());
                        cpu.mem.irq = true;
                        cpu.trigger_irq();
                        break;
                    }
                    if cpu.mem.ppu.h_counter() == cpu.mem.htime && cpu.mem.h_irq_enabled() {
                        //trace!("H-IRQ at H={}", cpu.mem.ppu.h_counter());
                        cpu.mem.irq = true;
                        cpu.trigger_irq();
                        break;
                    }
                }
            }

            if frame_rendered { return Ok(actions); }

            working_cy.set(self.master_cy);
        }
    }
}

/// The emulator.
pub struct Emulator<R: Renderer, A: AudioSink> {
    /// The renderer this emulator instance uses to display the screen
    pub renderer: R,
    /// The audio sink to be used for APU output
    pub audio: A,
    pub snes: Snes,
    #[allow(dead_code)]
    priv_: (),
}

impl<R: Renderer, A: AudioSink> Emulator<R, A> {
    /// Creates a new emulator instance from a loaded ROM and a renderer.
    ///
    /// This will also create a default `Input` instance without any attached peripherals.
    pub fn new(rom: Rom, renderer: R, audio: A) -> Self {
        // Start tracing at this master cycle (`!0` by default, which practically disables tracing)
        let trace_start: u64 = match env::var("BREEZE_TRACE") {
            Ok(string) => match string.parse() {
                Ok(trace) => {
                    info!("BREEZE_TRACE env var: starting trace after {} master cycles (make sure \
                           that the `trace` log level is enabled for the `wdc65816` crate)", trace);
                    trace
                },
                Err(_) => {
                    error!("ignoring invalid value for BREEZE_TRACE: {}", string);
                    !0
                },
            },
            Err(env::VarError::NotPresent) => !0,
            Err(env::VarError::NotUnicode(_)) => {
                error!("BREEZE_TRACE value isn't valid unicode, ignoring it");
                !0
            }
        };

        let mut snes = Snes::new(rom);
        snes.trace_start = trace_start;

        Emulator {
            renderer: renderer,
            audio: audio,
            snes: snes,
            priv_: (),
        }
    }

    /// Get a reference to the `Peripherals` instance
    pub fn peripherals(&self) -> &Peripherals { &self.snes.cpu.mem }

    /// Get a mutable reference to the `Peripherals` instance
    pub fn peripherals_mut(&mut self) -> &mut Peripherals { &mut self.snes.cpu.mem }

    /// Handles a `BackendAction`. Returns `true` if the emulator should exit.
    pub fn handle_action(&mut self, action: BackendAction) -> bool {
        match action {
            BackendAction::Exit => return true,
            BackendAction::SaveState => {
                let path = "breeze.sav";
                let mut file = File::create(path).unwrap();
                self.snes.create_save_state(SaveStateFormat::default(), &mut file).unwrap();
                info!("created a save state in '{}'", path);
            }
            BackendAction::LoadState => {
                if self.snes.cpu.mem.input.is_recording() || self.snes.cpu.mem.input.is_replaying() {
                    error!("cannot load a save state while recording or replaying input!");
                } else {
                    let file = File::open("breeze.sav").unwrap();
                    let mut bufrd = BufReader::new(file);
                    self.snes.restore_save_state(SaveStateFormat::default(), &mut bufrd).unwrap();
                    info!("restored save state");
                }
            }
        }

        false
    }

    /// Runs emulation until a frame is completed, renders the frame and handles an action dictated
    /// by the backend.
    ///
    /// Returns `true` if the backend requested an exit, `false` otherwise.
    pub fn render_frame(&mut self) -> BackendResult<bool> {
        let actions = {
            let renderer = &mut self.renderer;
            self.snes.render_frame(|framebuf| renderer.render(&**framebuf))
        };

        for action in try!(actions) {
            if self.handle_action(action) { return Ok(true); }
        }

        Ok(false)
    }

    /// Runs the emulator in a loop
    ///
    /// This will emulate the system and render frames until the backend signals that the emulator
    /// should exit.
    pub fn run(&mut self) -> BackendResult<()> {
        while !try!(self.render_frame()) {}
        Ok(())
    }
}
