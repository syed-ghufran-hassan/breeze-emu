//! Input emulation layer
//!
//! Our input emulation is modeled directly after the SNES hardware: We emulate the 2 controller
//! ports separately, down to the individual wires. `ControllerPortAttachment`s (CPAs) can be
//! plugged into each and have full control over the port: They can transmit individual bits on the
//! two data lines when the SNES reads from the port, read/write the `IOBit` line, or read the latch
//! line.
//!
//! This flexible setup should allow emulating all available peripherals: From the standard joypad
//! to light guns and the multitap, everything should be implementable on top of
//! `ControllerPortAttachment`.
//!
//! However, nothing is stopping the user from building unusable configurations, such as plugging a
//! light gun into port 1 (this doesn't work because the `IOBit` line of port 1 isn't connected to
//! the PPUs counter latch line). The frontend should warn on these.

use frontend::input::ControllerPortAttachment;

use std::io::{BufRead, Write};
use std::mem;
use std::ops::{Index, IndexMut};

/// Represents the 2 controller ports on the SNES
pub struct Ports(pub Option<Box<ControllerPortAttachment>>,
                 pub Option<Box<ControllerPortAttachment>>);

impl Ports {
    /// Run a closure on each attached peripheral
    fn for_each_peripheral<F>(&mut self, mut f: F) where F: FnMut(&mut ControllerPortAttachment) {
        if let Some(ref mut cpa) = self.0 {
            let periph: &mut ControllerPortAttachment = &mut **cpa;
            f(periph)
        }
        if let Some(ref mut cpa) = self.1 {
            let periph: &mut ControllerPortAttachment = &mut **cpa;
            f(periph)
        }
    }
}

impl Index<u8> for Ports {
    type Output = Option<Box<ControllerPortAttachment>>;
    fn index(&self, i: u8) -> &Self::Output {
        match i {
            0 => &self.0,
            1 => &self.1,
            _ => panic!("{} is not a valid controller port index (only 0 and 1 are valid)", i),
        }
    }
}

impl IndexMut<u8> for Ports {
    fn index_mut(&mut self, i: u8) -> &mut Self::Output {
        match i {
            0 => &mut self.0,
            1 => &mut self.1,
            _ => panic!("{} is not a valid controller port index (only 0 and 1 are valid)", i),
        }
    }
}

enum InputMode {
    Normal(Ports),
    Recorded(Ports, Box<Write>),
    Replayed(Box<BufRead>),
}

impl Default for InputMode {
    fn default() -> Self {
        InputMode::Normal(Ports(None, None))
    }
}

/// Controller input management.
#[derive(Default)]
pub struct Input {
    mode: InputMode,

    /// Auto-Joypad Data (`$4218` - `$421f`)
    auto_read_data: [u8; 8],
    /// Current latch state. Peripherals will have `set_latch` called when this changes.
    latch: bool,
}

impl_save_state!(Input { auto_read_data, latch } ignore { mode });

impl Input {
    /// Start recording input to a `Write` implementor, often a file.
    ///
    /// When reading data from a controller port, the recorder will write that data to the given
    /// `Box<Write>`.
    pub fn start_recording(&mut self, w: Box<Write>) {
        assert!(!self.is_recording(), "already recording");
        assert!(!self.is_replaying(), "cannot record while already replaying");

        let old_mode = mem::replace(&mut self.mode, InputMode::default());
        self.mode = match old_mode {
            InputMode::Normal(ports) => InputMode::Recorded(ports, w),
            InputMode::Recorded(..) => panic!("already recording"),
            InputMode::Replayed(_) => panic!("cannot record while already replaying"),
        };
    }

    /// Start replaying input from a recording made with `start_recording`. While replaying, user
    /// input is ignored (but input sources are still updated).
    pub fn start_replay(&mut self, r: Box<BufRead>) {
        assert!(!self.is_replaying(), "already replaying");
        assert!(!self.is_recording(), "cannot start a replay while recording input");

        self.mode = InputMode::Replayed(r);
    }

    /// Gets the `Ports` on the SNES.
    ///
    /// If we're replaying input, this will panic (since no ports actually exist).
    pub fn unwrap_ports(&mut self) -> &mut Ports {
        match self.mode {
            InputMode::Normal(ref mut ports) => ports,
            InputMode::Recorded(ref mut ports, _) => ports,
            InputMode::Replayed(_) => panic!("called Input::unwrap_ports while replaying"),
        }
    }

    pub fn is_recording(&self) -> bool {
        match self.mode {
            InputMode::Recorded(..) => true,
            _ => false,
        }
    }

    pub fn is_replaying(&self) -> bool {
        match self.mode {
            InputMode::Replayed(..) => true,
            _ => false,
        }
    }

    pub fn new_frame(&mut self) {
        match self.mode {
            InputMode::Normal(ref mut ports)
            | InputMode::Recorded(ref mut ports, _) => {
                ports.for_each_peripheral(|p| p.next_frame())
            }
            InputMode::Replayed(_) => {}
        }
    }

    /// Read the `Data1` and `Data2` line of a controller port.
    fn read_port(&mut self, port: u8) -> (bool, bool) {
        let data = match self.mode {
            InputMode::Normal(ref mut ports) |
            InputMode::Recorded(ref mut ports, _) => match ports[port] {
                Some(ref mut cpa) => cpa.read_bit(),
                None => (false, false),     // If nothing is attached, we read 0s
            },
            InputMode::Replayed(_) => unimplemented!(),
        };

        self.record_port_data(port, data);

        data
    }

    /// Read from an input register. Updates the controller state if this is the first load in this
    /// frame.
    pub fn load(&mut self, reg: u16) -> u8 {
        match reg {
            // $4016: JOYSER0 - NES-style Joypad Access Port 1
            // Rd: ------ca (Data2, Data1 line)
            // Wr: -------l (Latch)
            // $4017: JOYSER1 - NES-style Joypad Access Port 2
            // Rd: ---111db (Data2, Data1 line)
            0x4016 | 0x4017 => {
                let (data1, data2) = self.read_port((reg - 0x4016) as u8);
                let value = data1 as u8 | (data2 as u8) << 1;
                if reg == 0x4017 {
                    value | 0b00011100
                } else {
                    value
                }
            }
            0x4218 ... 0x421f => {
                // Read from auto-joypad register
                self.auto_read_data[reg as usize - 0x4218]
            }
            _ => panic!("${:04X} is not an input register", reg)
        }
    }

    /// Store to an input register. Stores to `$4016` can change the latch line.
    pub fn store(&mut self, reg: u16, val: u8) {
        if reg == 0x4016 {
            let new_latch = val & 0x01 != 0;
            if self.latch != new_latch {
                // Latch changed state
                match self.mode {
                    InputMode::Normal(ref mut ports) | InputMode::Recorded(ref mut ports, _) => {
                        ports.for_each_peripheral(|p| p.set_latch(new_latch))
                    }
                    InputMode::Replayed(_) => {}
                }
                self.latch = new_latch;
            }
        } else {
            panic!("invalid input reg store to ${:04X}", reg);
        }
    }

    /// Called when auto joypad read is enabled and it's time to do one.
    ///
    /// On the real console, auto joypad read takes place in the first few scanline in V-Blank. We
    /// pretend it's instantaneous and set the auto joypad read bit in `$4212` manually.
    pub fn perform_auto_read(&mut self) {
        // Store 1, then 0 to the latch, latching both ports
        self.store(0x4016, 1);
        self.store(0x4016, 0);

        // Read 16 times (16*4=64 bits=8 bytes) from both ports and store the result in `self.auto_read_data`
        // High bytes first (`JOY1H`/`JOY3H` for port 0, `JOY2H`/`JOY4H` for port 1)
        for _ in 0..8 {
            let (a, b) = self.read_port(0);
            self.auto_read_data[1] <<= 1;       // `JOY1H`
            self.auto_read_data[1] |= a as u8;
            self.auto_read_data[5] <<= 1;       // `JOY3H`
            self.auto_read_data[5] |= b as u8;
            let (a, b) = self.read_port(1);
            self.auto_read_data[3] <<= 1;       // `JOY2H`
            self.auto_read_data[3] |= a as u8;
            self.auto_read_data[7] <<= 1;       // `JOY4H`
            self.auto_read_data[7] |= b as u8;
        }
        // Then the low bytes (`JOY1L`/`JOY3L` for port 0, `JOY2L`/`JOY4L` for port 1)
        for _ in 0..8 {
            let (a, b) = self.read_port(0);
            self.auto_read_data[0] <<= 1;       // `JOY1L`
            self.auto_read_data[0] |= a as u8;
            self.auto_read_data[4] <<= 1;       // `JOY3L`
            self.auto_read_data[4] |= b as u8;
            let (a, b) = self.read_port(1);
            self.auto_read_data[2] <<= 1;       // `JOY2L`
            self.auto_read_data[2] |= a as u8;
            self.auto_read_data[6] <<= 1;       // `JOY4L`
            self.auto_read_data[6] |= b as u8;
        }
    }

    /// Callback for recording controller data received via the 2 data lines.
    fn record_port_data(&mut self, _port: u8, (_data1, _data2): (bool, bool)) {
        // FIXME Unimplemented
        // FIXME Record `IOBit` when it's supported

        if let InputMode::Recorded(ref _ports, ref _out) = self.mode {
            unimplemented!()
        }
    }
}