//! The I/O chip: the version register, three ports with their registers, and the pads.
//!
//! Transcribed from megadrive-python's `io.py`. Registers at $A10000-$A1001F, one byte
//! each at the odd address, the even address reading the same (CMD `gen-hw.txt` 3.0,
//! checked on hardware):
//!
//! ```text
//! $A10001 version   $A10003/5/7 data A/B/C   $A10009/B/D control A/B/C
//! $A1000F/15/1B TxData   $A10011/17/1D RxData   $A10013/19/1F S-Ctrl
//! ```
//!
//! CMD 3.0 gives the power-on values and the read rules, which this module follows.
//! The serial mode itself is not modelled. A device on a port sees the level on TH
//! and drives the six input pins D5-D0; the 6-button pad counts the *edges* of TH, so
//! every access that can change TH tells the port at what master clock it did.
//!
//! Rust's difference: the reference's duck-typed devices are the [`Device`] enum.

// Button bits of the pad's own word (active high here; the pins are active low).
pub const UP: u16 = 1 << 0;
pub const DOWN: u16 = 1 << 1;
pub const LEFT: u16 = 1 << 2;
pub const RIGHT: u16 = 1 << 3;
pub const B: u16 = 1 << 4;
pub const C: u16 = 1 << 5;
pub const A: u16 = 1 << 6;
pub const START: u16 = 1 << 7;
/// The 6-button pad's four extra buttons (Terakawa's table).
pub const Z: u16 = 1 << 8;
pub const Y: u16 = 1 << 9;
pub const X: u16 = 1 << 10;
pub const MODE: u16 = 1 << 11;

/// The buttons by name, for tools that take them as text.
pub const BUTTONS: [(&str, u16); 12] = [
    ("up", UP),
    ("down", DOWN),
    ("left", LEFT),
    ("right", RIGHT),
    ("b", B),
    ("c", C),
    ("a", A),
    ("start", START),
    ("z", Z),
    ("y", Y),
    ("x", X),
    ("mode", MODE),
];

/// A 3-button control pad (CMD `gen-hw.txt` 3.2).
///
/// [`Pad3::pins`] returns the six input pins D5-D0 (TR, TL, RIGHT, LEFT, DOWN, UP),
/// low for a pressed button, given the level on TH.
#[derive(Clone, Debug, Default)]
pub struct Pad3 {
    pub pressed: u16,
}

impl Pad3 {
    pub fn pins(&self, th: u8) -> u8 {
        let held = self.pressed;
        let value = if th != 0 {
            // ?1CBRLDU: TR = C, TL = B.
            (held & (UP | DOWN | LEFT | RIGHT))
                | (if held & B != 0 { 0x10 } else { 0 })
                | (if held & C != 0 { 0x20 } else { 0 })
        } else {
            // ?0SA00DU: TR = A, TL = Start, LEFT and RIGHT read as 0.
            (held & (UP | DOWN))
                | 0x0C
                | (if held & A != 0 { 0x10 } else { 0 })
                | (if held & START != 0 { 0x20 } else { 0 })
        };
        (!value & 0x3F) as u8
    }
}

/// Ein Terakawa's three times for his own 6-button pads, in milliseconds (his pads'
/// figures, not the hardware's: "It may mean parameters differ one by one
/// controller").
pub const TERAKAWA_TRIGGER_MS: f64 = 1.1;
pub const TERAKAWA_VALID_MS: f64 = 1.6;
pub const TERAKAWA_RESTART_MS: f64 = 1.8;

/// CMD's figure for the same capacitor: "If TH isn't modified in about 8192 (probably
/// less than that) 68000 CPU cycles, a 'time-out' will occur" (`gen-hw.txt` 3.2). The
/// board's default: the only source that says *from what* the pad times out, and the
/// shortest, which is the safe end.
pub const CMD_TIMEOUT_M68K_CYCLES: i64 = 8192;

/// Master clocks a millisecond (the NTSC master clock; PAL's is 0.9 % slower, far
/// inside the spread the sources show).
pub const MCLK_PER_MS: f64 = 53_693.175;
/// Master clocks a 68000 clock.
pub const MCLK_PER_M68K: i64 = 7;

/// A 6-button control pad: a 3-button pad with a counter on TH.
///
/// Terakawa (`reference/web/terakawa-6button.html`; hardware-corrected) and CMD
/// (`gen-hw.txt` 3.2, "checked on the real thing") agree on every value it returns;
/// the reference's docstring has both tables. The counter advances on each rising
/// edge of TH; after the second the low phase forces D0-D3 low, after the third the
/// high phase carries Mode, X, Y and Z and the low phase forces D0-D3 high, and from
/// the fourth on the pad is a 3-button pad again until it times out. **The timeout is
/// deliberately not claimed**: CMD's shape and figure, as a field.
#[derive(Clone, Debug)]
pub struct Pad6 {
    pub pressed: u16,
    /// How long TH may stay still before the counter goes back to the start.
    pub timeout_mclk: i64,
    /// The pad's own MODE switch: False is a pad powered up with MODE held, which
    /// "will respond like a 3-button one" (CMD 3.2).
    pub mode_switch: bool,
    /// Rising edges of TH since the sequence began (0 before the first).
    pub edges: u8,
    /// When TH last changed, in master clocks.
    pub last_change: i64,
    /// TH is pulled up until a port drives it.
    pub th_level: u8,
}

impl Default for Pad6 {
    fn default() -> Self {
        Pad6::new(CMD_TIMEOUT_M68K_CYCLES * MCLK_PER_M68K, true)
    }
}

impl Pad6 {
    pub fn new(timeout_mclk: i64, mode_switch: bool) -> Self {
        Pad6 {
            pressed: 0,
            timeout_mclk,
            mode_switch,
            edges: 0,
            last_change: 0,
            th_level: 1,
        }
    }

    fn timeout(&mut self, mclk: i64) {
        if self.edges != 0 && mclk - self.last_change >= self.timeout_mclk {
            self.edges = 0;
        }
    }

    /// TH went to `th` at master clock `mclk`: count its rising edges.
    pub fn set_th(&mut self, th: u8, mclk: i64) {
        let th = u8::from(th != 0);
        self.timeout(mclk);
        if th != self.th_level {
            if th != 0 && self.mode_switch && self.edges < 4 {
                self.edges += 1;
            }
            self.last_change = mclk;
        }
        self.th_level = th;
    }

    pub fn pins(&mut self, th: u8, mclk: i64) -> u8 {
        self.timeout(mclk);
        let held = self.pressed;
        let edges = self.edges;
        let value = if th != 0 {
            if edges == 3 {
                // ?1CBMXYZ: Mode, X, Y and Z on D3-D0, B and C still on D4 and D5.
                let value = (if held & Z != 0 { 0x01 } else { 0 })
                    | (if held & Y != 0 { 0x02 } else { 0 })
                    | (if held & X != 0 { 0x04 } else { 0 })
                    | (if held & MODE != 0 { 0x08 } else { 0 })
                    | (if held & B != 0 { 0x10 } else { 0 })
                    | (if held & C != 0 { 0x20 } else { 0 });
                return (!value & 0x3F) as u8;
            }
            // ?1CBRLDU, as the 3-button pad.
            (held & (UP | DOWN | LEFT | RIGHT))
                | (if held & B != 0 { 0x10 } else { 0 })
                | (if held & C != 0 { 0x20 } else { 0 })
        } else {
            // ?0SA00DU, with D0-D3 forced low after the second rising edge and high
            // after the third: the two phases a program uses to recognise the pad.
            let (directions, low) = match edges {
                2 => (0, 0x0F), // ?0SA0000
                3 => (0, 0x00), // ?0SA1111
                _ => (held & (UP | DOWN), 0x0C),
            };
            directions
                | low
                | (if held & A != 0 { 0x10 } else { 0 })
                | (if held & START != 0 { 0x20 } else { 0 })
        };
        (!value & 0x3F) as u8
    }
}

/// Anything that can sit on a control port (the reference's `Pad3 | Pad6 | None`).
#[derive(Clone, Debug)]
pub enum Device {
    Pad3(Pad3),
    Pad6(Pad6),
}

impl Device {
    /// The pads a caller can ask for by name (`--pad`): "3button", "6button", or
    /// "none" (None).
    pub fn by_name(name: &str) -> Result<Option<Device>, String> {
        match name {
            "3button" => Ok(Some(Device::Pad3(Pad3::default()))),
            "6button" => Ok(Some(Device::Pad6(Pad6::default()))),
            "none" => Ok(None),
            _ => Err(format!("unknown pad {name:?} (3button, 6button, none)")),
        }
    }

    /// What the pad is called in a report.
    pub fn kind(&self) -> &'static str {
        match self {
            Device::Pad3(_) => "3-button",
            Device::Pad6(_) => "6-button",
        }
    }

    pub fn pressed(&self) -> u16 {
        match self {
            Device::Pad3(pad) => pad.pressed,
            Device::Pad6(pad) => pad.pressed,
        }
    }

    pub fn set_pressed(&mut self, pressed: u16) {
        match self {
            Device::Pad3(pad) => pad.pressed = pressed,
            Device::Pad6(pad) => pad.pressed = pressed,
        }
    }

    /// TH changed (a 3-button pad is combinational and has nothing to remember).
    pub fn set_th(&mut self, th: u8, mclk: i64) {
        if let Device::Pad6(pad) = self {
            pad.set_th(th, mclk);
        }
    }

    pub fn pins(&mut self, th: u8, mclk: i64) -> u8 {
        match self {
            Device::Pad3(pad) => pad.pins(th),
            Device::Pad6(pad) => pad.pins(th, mclk),
        }
    }
}

/// One port's data, control and serial registers, and the device on it.
#[derive(Clone, Debug)]
pub struct Port {
    pub device: Option<Device>,
    /// The value last written (bit 7 is a latch: CMD 3.0).
    pub data: u8,
    pub data_written: bool,
    /// Bits 6-0: 1 = output.
    pub control: u8,
    pub tx_data: u8,
    pub s_control: u8,
}

impl Port {
    pub fn new(device: Option<Device>) -> Self {
        Port {
            device,
            data: 0x00,
            data_written: false,
            control: 0x00,
            tx_data: 0xFF,
            s_control: 0x00,
        }
    }

    /// The level on TH: driven by the port when it is an output, else pulled up.
    pub fn th(&self) -> u8 {
        if self.control & 0x40 != 0 {
            (self.data >> 6) & 1
        } else {
            1
        }
    }

    /// Tell the device where TH is now, after a write that could have moved it.
    pub fn set_th(&mut self, mclk: i64) {
        let th = self.th();
        if let Some(device) = &mut self.device {
            device.set_th(th, mclk);
        }
    }

    pub fn read_data(&mut self, mclk: i64) -> u8 {
        let outputs = self.control & 0x7F;
        let th = self.th();
        let pins = match &mut self.device {
            Some(device) => (th << 6) | device.pins(th, mclk),
            None => 0x7F, // undriven inputs read 1
        };
        (self.data & (outputs | 0x80)) | (pins & !outputs & 0x7F)
    }
}

/// The version register and ports A, B and C (EXT).
#[derive(Clone, Debug)]
pub struct IOChip {
    pub version: u8,
    pub ports: [Port; 3],
}

impl IOChip {
    /// `pads` None is the reference's default, two 3-button pads.
    pub fn new(version: u8, pads: Option<[Option<Device>; 2]>) -> Self {
        let [a, b] = pads.unwrap_or_else(|| {
            [
                Some(Device::Pad3(Pad3::default())),
                Some(Device::Pad3(Pad3::default())),
            ]
        });
        IOChip {
            version,
            ports: [Port::new(a), Port::new(b), Port::new(None)],
        }
    }

    /// The byte register at `address` ($A10000-$A1001F; even reads as odd).
    pub fn read(&mut self, address: u32, mclk: i64) -> u8 {
        let register = ((address >> 1) & 0x0F) as usize;
        if register == 0 {
            return self.version;
        }
        if register <= 3 {
            return self.ports[register - 1].read_data(mclk);
        }
        if register <= 6 {
            return self.ports[register - 4].control;
        }
        let port = &self.ports[(register - 7) / 3];
        match (register - 7) % 3 {
            0 => port.tx_data,
            1 => 0x00, // RxData
            _ => port.s_control & 0xF0,
        }
    }

    pub fn write(&mut self, address: u32, value: u8, mclk: i64) {
        let register = ((address >> 1) & 0x0F) as usize;
        if register == 0 {
            return; // the version register is read-only
        }
        if register <= 3 {
            let port = &mut self.ports[register - 1];
            port.data = value;
            port.data_written = true;
            port.set_th(mclk);
        } else if register <= 6 {
            // A control-register write can make TH an output or let it float back up,
            // which is a TH edge as much as a data write is.
            let port = &mut self.ports[register - 4];
            port.control = value;
            port.set_th(mclk);
        } else {
            let port = &mut self.ports[(register - 7) / 3];
            match (register - 7) % 3 {
                0 => port.tx_data = value,
                2 => port.s_control = value & 0xF0,
                _ => {}
            }
        }
    }
}
