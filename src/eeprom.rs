//! The serial EEPROMs a few cartridges carry instead of battery SRAM.
//!
//! Transcribed from megadrive-python's `eeprom.py`, which is written from Eke-Eke's
//! *Serial EEPROMs in Sega Genesis / Mega Drive cartridges*, version 2 (2010;
//! `gen_eeprom.pdf` in `reference/docs/md_tech.zip`): the two-wire protocol between
//! the 68000 (always the master) and the chip, START/STOP framing, eight-bit words
//! with a ninth acknowledge clock, and the three addressing modes (1: X24C01, one
//! word of 7 address bits + R/W; 2: 24C01-24C16, a device word and a low word; 3:
//! 24C32 and up, a device word and two address words). A written address wraps inside
//! the chip's page, a read address at the end of the chip.

pub const MODE1: u8 = 1;
pub const MODE2: u8 = 2;
pub const MODE3: u8 = 3;

/// What the machine is doing between a START and a STOP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    /// The first word: device address, upper address bits, R/W.
    Device,
    /// Mode 3's second word.
    WordHigh,
    /// The word holding the low eight address bits.
    WordLow,
    /// Data the master is sending.
    Write,
    /// Data the chip is sending.
    Read,
}

/// One serial EEPROM, driven by its two lines.
///
/// `size_mask` and `page_mask` are Eke's own fields: the chip is `size_mask + 1`
/// bytes and a write wraps inside `page_mask + 1` of them. An unwritten chip reads
/// $FF, which is what an erased EEPROM holds.
#[derive(Clone, Debug)]
pub struct SerialEEPROM {
    pub mode: u8,
    pub size_mask: usize,
    pub page_mask: usize,
    pub memory: Vec<u8>,
    pub scl: u8,
    pub sda: u8,
    /// What the chip drives on /SDA: 1 is the released line.
    pub out: u8,
    pub state: State,
    pub bit: u8,
    pub latch: u8,
    /// The byte being sent, most significant bit first.
    pub shift: u8,
    /// The ninth clock of the word.
    pub acknowledging: bool,
    /// 0 drive it, 1 it is on the line, 2 it has been read.
    pub ack_step: u8,
    pub address: usize,
    pub reading: bool,
    master_ack: bool,
    /// Bytes written since power-on, for a report.
    pub writes: u64,
}

impl SerialEEPROM {
    pub fn new(mode: u8, size_mask: usize, page_mask: usize, data: Option<&[u8]>) -> Self {
        let mut memory = vec![0xFF; size_mask + 1];
        if let Some(data) = data {
            let n = data.len().min(size_mask + 1);
            memory[..n].copy_from_slice(&data[..n]);
        }
        SerialEEPROM {
            mode,
            size_mask,
            page_mask,
            memory,
            scl: 1,
            sda: 1,
            out: 1,
            state: State::Idle,
            bit: 0,
            latch: 0,
            shift: 0,
            acknowledging: false,
            ack_step: 0,
            address: 0,
            reading: false,
            master_ack: false,
            writes: 0,
        }
    }

    // -- the two lines ------------------------------------------------------------

    /// The cartridge presents the levels the 68000 just wrote.
    pub fn update(&mut self, scl: u8, sda: u8) {
        let scl = u8::from(scl != 0);
        let sda = u8::from(sda != 0);
        if self.scl != 0 && scl != 0 {
            // START and STOP are the only /SDA changes allowed while /SCL is high.
            if self.sda != 0 && sda == 0 {
                self.start();
            } else if self.sda == 0 && sda != 0 {
                self.stop();
            }
        } else if scl != 0 && self.scl == 0 {
            self.rising(sda);
        } else if self.scl != 0 && scl == 0 {
            self.falling();
        }
        self.scl = scl;
        self.sda = sda;
    }

    /// What the chip is driving on /SDA (1 when it has let go of the line).
    pub fn read_sda(&self) -> u8 {
        self.out
    }

    // -- the machine --------------------------------------------------------------

    fn start(&mut self) {
        self.state = State::Device;
        self.bit = 0;
        self.latch = 0;
        self.acknowledging = false;
        self.ack_step = 0;
        self.reading = false;
        self.out = 1;
    }

    fn stop(&mut self) {
        self.state = State::Idle;
        self.acknowledging = false;
        self.ack_step = 0;
        self.out = 1;
    }

    /// /SCL went high: the bit on /SDA is valid, so this is where one is counted.
    fn rising(&mut self, sda: u8) {
        if self.state == State::Idle {
            return;
        }
        if self.acknowledging {
            if self.ack_step == 1 {
                if self.state == State::Read {
                    // The master acknowledges by holding /SDA low; if it does not,
                    // "the READ operation ends and the EEPROM waits for STOP".
                    self.master_ack = sda == 0;
                }
                self.ack_step = 2;
            }
            return;
        }
        if self.state != State::Read {
            self.latch = (self.latch << 1) | sda;
        }
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.acknowledging = true;
            self.ack_step = 0;
        }
    }

    /// /SCL went low: the sender puts the next bit, or the word is over.
    fn falling(&mut self) {
        if self.state == State::Idle {
            return;
        }
        if self.acknowledging {
            if self.ack_step == 0 {
                // The receiver drives the acknowledge through the ninth clock: the chip
                // when it is being addressed or written to, the master when reading.
                self.out = if self.state != State::Read { 0 } else { 1 };
                self.ack_step = 1;
            } else if self.ack_step == 2 {
                self.acknowledging = false;
                self.word_done();
            }
            return;
        }
        if self.state == State::Read {
            self.shift <<= 1;
            self.out = (self.shift >> 7) & 1;
        }
    }

    /// The acknowledge cycle is over: act on the word and set up the next.
    fn word_done(&mut self) {
        if self.state == State::Read {
            if self.master_ack {
                self.address = (self.address + 1) & self.size_mask;
                self.send(self.memory[self.address]);
            } else {
                self.state = State::Idle; // it waits for STOP; nothing is driven meanwhile
                self.out = 1;
            }
            return;
        }
        let word = self.latch;
        self.latch = 0;
        match self.state {
            State::Device => {
                self.reading = word & 1 != 0;
                if self.mode == MODE1 {
                    // "The 7 first bits set the memory address to be read/write and the
                    // 8th bit set the operation type" (Eke, mode 1).
                    self.address = usize::from(word >> 1) & self.size_mask;
                    self.begin_data();
                } else {
                    if self.mode == MODE2 {
                        // The device field carries the address's upper bits, and only
                        // those: the low eight stay where the last access left them.
                        self.address = ((usize::from((word >> 1) & 0x07) << 8)
                            | (self.address & 0xFF))
                            & self.size_mask;
                    }
                    if self.reading {
                        self.begin_data();
                    } else {
                        self.state = if self.mode == MODE3 {
                            State::WordHigh
                        } else {
                            State::WordLow
                        };
                        self.out = 1;
                    }
                }
            }
            State::WordHigh => {
                self.address = ((usize::from(word) << 8) | (self.address & 0xFF)) & self.size_mask;
                self.state = State::WordLow;
                self.out = 1;
            }
            State::WordLow => {
                self.address = ((self.address & !0xFF) | usize::from(word)) & self.size_mask;
                self.begin_data();
            }
            State::Write => {
                self.memory[self.address] = word;
                self.writes += 1;
                // "The address will roll up to base memory address when maximal write
                // page size has been reached": only the low bits move.
                self.address =
                    (self.address & !self.page_mask) | ((self.address + 1) & self.page_mask);
                self.out = 1;
            }
            State::Idle | State::Read => {}
        }
    }

    fn begin_data(&mut self) {
        if self.reading {
            self.state = State::Read;
            self.send(self.memory[self.address]);
        } else {
            self.state = State::Write;
            self.out = 1;
        }
    }

    fn send(&mut self, value: u8) {
        self.shift = value;
        self.out = (self.shift >> 7) & 1;
    }
}
