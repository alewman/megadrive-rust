//! Virtua Racing's cartridge: the SVP (`rom_svp`), an SSP1601 DSP and its memory controller.
//!
//! Transcribed from megadrive-python's `svp.py`, itself a transcription of MAME 0.285's
//! `bus/megadrive/svp.cpp`. Nothing here is hardware-measured: Virtua Racing is the only
//! software, and MAME runs it. The DSP is `ssp1601-rust`.
//!
//! Rust's differences: the reference's `SVP` is both the board and the DSP's host; here
//! [`Memory`] is the host and [`Svp`] holds it beside the DSP, so the two can be borrowed
//! apart. The idle look keys on a vector of the same fields. The reference's `unmatched`
//! set, a diagnostic only, is not kept.

use std::collections::HashMap;

use ssp1601_rust::{Host, Ssp1601};

/// How many DSP cycles run between looks for an idle loop, and how many looks are kept.
const IDLE_CHUNK: i64 = 256;
const IDLE_WINDOW: usize = 64;

/// The DSP's clock is the master clock * 3/7 (MAME: MASTER_CLOCK_NTSC / 7 * 3).
const SSP_CLOCK_NUM: i64 = 3;
const SSP_CLOCK_DEN: i64 = 7;

const HAVE_ADDR: u16 = 1; // an address has gone into PMC and it waits for the mode
const PMC_SET: u16 = 2; // PMC holds both and the next PMx access programs that register

/// The auto-increment a PM mode word asks for: 0, 1, 2, 4, 8, 16, 32 or 128 words.
fn increment(mode: u32) -> u32 {
    let bits = (mode >> 11) & 7;
    if bits == 0 {
        return 0;
    }
    let step: u32 = if bits == 7 { 128 } else { 1 << (bits - 1) };
    if mode & 0x8000 != 0 {
        step.wrapping_neg()
    } else {
        step
    }
}

/// Overwrite mode: only the nibbles of `new` that are not zero are written.
fn overwrite(mut old: u16, new: u16) -> u16 {
    for shift in [12, 8, 4, 0] {
        let nibble = new & (0xF << shift);
        if nibble != 0 {
            old = (old & !(0xF << shift)) | nibble;
        }
    }
    old
}

/// The memory controller: everything the DSP reaches through its host.
#[derive(Clone, Debug)]
pub struct Memory {
    rom_words: Vec<u16>,
    dram: Vec<u16>,
    iram: Vec<u16>,
    pmac_read: [u32; 5],
    pmac_write: [u32; 5],
    /// 32 bits: the address word below, the mode word above.
    pmc: u32,
    status: u16,
    /// The word both sides read and write.
    xst: u16,
    /// Bit 0: the DSP wrote XST, bit 1: the 68000 did.
    xst2: u16,
    /// Bumped by anything that changes what the DSP could read from outside: the
    /// 68000's writes and status reads, and a DSP write that changes a word.
    version: u64,
}

impl Memory {
    fn store_dram(&mut self, address: usize, data: u16, mode: u32) {
        let old = self.dram[address];
        let new = if mode & 0x0400 != 0 {
            overwrite(old, data)
        } else {
            data
        };
        if new != old {
            self.dram[address] = new;
            self.version += 1;
        }
    }

    fn read_pmc(&mut self) -> u16 {
        let low = self.pmc as u16;
        if self.status & HAVE_ADDR != 0 {
            self.status = (self.status | PMC_SET) & !HAVE_ADDR;
            return ((low << 4) & 0xFFF0) | ((low >> 4) & 0xF);
        }
        self.status |= HAVE_ADDR;
        low
    }

    fn write_pmc(&mut self, value: u16) {
        if self.status & HAVE_ADDR != 0 {
            self.status = (self.status | PMC_SET) & !HAVE_ADDR;
            self.pmc = (self.pmc & 0xFFFF) | (u32::from(value) << 16);
        } else {
            self.status |= HAVE_ADDR;
            self.pmc = (self.pmc & 0xFFFF_0000) | u32::from(value);
        }
    }

    fn rom_word(&self, index: usize) -> u16 {
        self.rom_words.get(index).copied().unwrap_or(0xFFFF)
    }

    /// A programmable-memory access; None when the register is not in that mode.
    ///
    /// Registers 0-3 are programmable only while ST5 or ST6 is set (else they are the
    /// status words); register 4 always is. The first access after PMC was set
    /// *programs* the register with it, whatever the access is, and moves no data.
    fn pm_io(&mut self, reg: usize, write: bool, data: u16, st: u16) -> Option<u16> {
        if self.status & PMC_SET != 0 {
            if write {
                self.pmac_write[reg] = self.pmc;
            } else {
                self.pmac_read[reg] = self.pmc;
            }
            self.status &= !PMC_SET;
            return Some(0);
        }
        self.status &= !HAVE_ADDR; // "just in case"
        if reg != 4 && st & 0x60 == 0 {
            return None;
        }
        let entry = if write {
            self.pmac_write[reg]
        } else {
            self.pmac_read[reg]
        };
        let mode = entry >> 16;
        let address = (entry & 0xFFFF) as usize;
        let mut moved = entry;
        let mut out = 0;
        if write {
            if mode & 0x43FF == 0x0018 {
                // DRAM
                self.store_dram(address, data, mode);
                moved = moved.wrapping_add(increment(mode));
            } else if mode & 0xFBFF == 0x4018 {
                // DRAM, "cell" increment: 1, or 31 from an odd word
                self.store_dram(address, data, mode);
                moved = moved.wrapping_add(if address & 1 != 0 { 31 } else { 1 });
            } else if mode & 0x47FF == 0x001C {
                // IRAM
                if self.iram[address & 0x3FF] != data {
                    self.iram[address & 0x3FF] = data;
                    self.version += 1;
                }
                moved = moved.wrapping_add(increment(mode));
            }
        } else if mode & 0xFFF0 == 0x0800 {
            // ROM, one word on
            moved = moved.wrapping_add(1);
            out = self.rom_word(address | ((mode as usize & 0xF) << 16));
        } else if mode & 0x47FF == 0x0018 {
            // DRAM
            out = self.dram[address];
            moved = moved.wrapping_add(increment(mode));
        }
        if write {
            self.pmac_write[reg] = moved;
        } else {
            self.pmac_read[reg] = moved;
        }
        self.pmc = moved; // "PMC corresponds to the last PMx accessed"
        Some(out)
    }
}

impl Host for Memory {
    fn program_read(&mut self, address: u16) -> u16 {
        if address < 0x400 {
            return self.iram[usize::from(address)];
        }
        self.rom_word(usize::from(address))
    }

    fn ext_read(&mut self, n: u16, st: u16) -> u16 {
        match n {
            0 => match self.pm_io(0, false, 0, st) {
                Some(result) => result,
                None => {
                    let value = self.xst2;
                    self.xst2 &= !2;
                    value
                }
            },
            1 | 2 => self.pm_io(usize::from(n), false, 0, st).unwrap_or(0),
            3 => match self.pm_io(3, false, 0, st) {
                Some(result) => result,
                None => self.xst,
            },
            4 => self.pm_io(4, false, 0, st).unwrap_or(0),
            6 => self.read_pmc(),
            7 => {
                self.status &= !(PMC_SET | HAVE_ADDR);
                0
            }
            _ => 0,
        }
    }

    fn ext_write(&mut self, n: u16, value: u16, st: u16) {
        match n {
            0 => {
                if self.pm_io(0, true, value, st).is_none() {
                    self.xst2 = value;
                }
            }
            1 | 2 => {
                self.pm_io(usize::from(n), true, value, st);
            }
            3 => {
                if self.pm_io(3, true, value, st).is_none() {
                    self.xst2 |= 1;
                    self.xst = value;
                }
            }
            4 => {
                self.pm_io(4, true, value, st);
            }
            6 => self.write_pmc(value),
            _ => {}
        }
    }
}

/// Virtua Racing: 2 MB of ROM, 128 KB of DRAM at $300000 and the DSP behind them.
#[derive(Clone, Debug)]
pub struct Svp {
    pub dsp: Ssp1601,
    pub memory: Memory,
    dsp_cycles: i64,
    /// Cycles of the idle loop the DSP is in, or 0.
    idle_period: i64,
    /// The cycle count at the loop's head.
    idle_base: i64,
    /// Where the DSP is owed to be (the cycles an idle DSP has not run).
    target: i64,
    next_look: i64,
    seen: HashMap<Vec<u32>, (i64, u64)>,
}

impl Svp {
    /// The trace's line: the frame shows only what the 68000 has taken from the DSP, not
    /// how far it has got.
    pub fn trace_line(&self) -> String {
        fn hex<T: std::fmt::UpperHex>(values: &[T]) -> String {
            let list: Vec<String> = values.iter().map(|v| format!("{v:X}")).collect();
            list.join(",")
        }
        fn words(values: &[u16]) -> u32 {
            let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            crate::trace::crc32(&bytes)
        }
        let (d, m) = (&self.dsp, &self.memory);
        format!(
            "S cyc={} idle={} base={} target={} x={:X} y={:X} a={:X} st={:X} pc={:X} sp={} \
             stack={} r={} ram={:08X} dram={:08X} iram={:08X} pmr={} pmw={} pmc={:X} \
             status={:X} xst={:X} xst2={:X}",
            self.dsp_cycles,
            self.idle_period,
            self.idle_base,
            self.target,
            d.x,
            d.y,
            d.a,
            d.st,
            d.pc,
            d.sp,
            hex(&d.stack),
            hex(&d.r),
            words(&d.ram),
            words(&m.dram),
            words(&m.iram),
            hex(&m.pmac_read),
            hex(&m.pmac_write),
            m.pmc,
            m.status,
            m.xst,
            m.xst2,
        )
    }

    pub fn new(padded: &[u8]) -> Self {
        let rom_words = padded
            .chunks_exact(2)
            .map(|pair| u16::from(pair[0]) << 8 | u16::from(pair[1]))
            .collect();
        Svp {
            dsp: Ssp1601::new(),
            memory: Memory {
                rom_words,
                dram: vec![0; 0x10000],
                iram: vec![0; 0x400],
                pmac_read: [0; 5],
                pmac_write: [0; 5],
                pmc: 0,
                status: 0,
                xst: 0,
                xst2: 0,
                version: 0,
            },
            dsp_cycles: 0,
            idle_period: 0,
            idle_base: 0,
            target: 0,
            next_look: IDLE_CHUNK,
            seen: HashMap::new(),
        }
    }

    // -- the clock --------------------------------------------------------------------

    /// Run the DSP up to `mclk` master clocks from power-on.
    ///
    /// **Idle loops are not executed, exactly.** When the DSP's whole state, the memory
    /// controller's, and `version` come round to what they were some cycles ago, it is
    /// in a loop that changes nothing and stays in it until the 68000 touches the chip.
    /// From then on the cycles are only counted, and [`Svp::wake`] runs the DSP from the
    /// loop's head for the cycles over a whole number of turns.
    pub fn catch_up(&mut self, mclk: i64) {
        let due = mclk * SSP_CLOCK_NUM / SSP_CLOCK_DEN;
        if self.idle_period != 0 {
            self.target = due;
            return;
        }
        if self.dsp_cycles < due {
            let cycles = (due - self.dsp_cycles) as u64;
            self.dsp_cycles += self.dsp.run(cycles, &mut self.memory) as i64;
            if self.dsp_cycles >= self.next_look {
                self.look();
            }
        }
        self.target = self.dsp_cycles.max(due);
    }

    /// Has the DSP come back to a state it was in, with nothing outside changed?
    fn look(&mut self) {
        self.next_look = self.dsp_cycles + IDLE_CHUNK;
        let signature = self.signature();
        if let Some(&(cycles, version)) = self.seen.get(&signature) {
            if version == self.memory.version {
                self.idle_period = self.dsp_cycles - cycles;
                self.idle_base = self.dsp_cycles;
                return;
            }
        }
        if self.seen.len() >= IDLE_WINDOW {
            self.seen.clear();
        }
        self.seen
            .insert(signature, (self.dsp_cycles, self.memory.version));
    }

    /// The 68000 is about to change what the DSP can see: bring an idle DSP up to date.
    fn wake(&mut self) {
        if self.idle_period != 0 {
            let owed = self.target - self.idle_base;
            let turns = owed.div_euclid(self.idle_period);
            let rest = owed.rem_euclid(self.idle_period);
            self.dsp_cycles = self.idle_base + turns * self.idle_period;
            self.idle_period = 0;
            self.dsp_cycles += self.dsp.run(rest as u64, &mut self.memory) as i64;
            self.seen.clear();
            self.next_look = self.dsp_cycles + IDLE_CHUNK;
        }
        self.memory.version += 1;
    }

    /// Everything that decides what the DSP and the memory controller do next.
    fn signature(&self) -> Vec<u32> {
        let d = &self.dsp;
        let m = &self.memory;
        let mut out = Vec::with_capacity(0x200 + 40);
        out.extend([
            u32::from(d.pc),
            u32::from(d.x),
            u32::from(d.y),
            d.a,
            u32::from(d.st),
            d.sp as u32,
        ]);
        out.extend(d.stack.iter().map(|&v| u32::from(v)));
        out.extend(d.r.iter().map(|&v| u32::from(v)));
        out.extend(d.ram.iter().map(|&v| u32::from(v)));
        out.extend([
            u32::from(m.status),
            m.pmc,
            u32::from(m.xst),
            u32::from(m.xst2),
        ]);
        out.extend(m.pmac_read);
        out.extend(m.pmac_write);
        out
    }

    // -- the 68000's side -------------------------------------------------------------

    pub fn read_word(&mut self, address: u32) -> u16 {
        let m = &self.memory;
        if address < 0x200000 {
            return m.rom_word((address >> 1) as usize);
        }
        if (0x300000..0x320000).contains(&address) {
            return m.dram[((address - 0x300000) >> 1) as usize];
        }
        if (0x390000..0x3A0000).contains(&address) {
            // "rewritten 68k test code": the cell arrangement the SVP's test reads
            let a = (address - 0x390000) >> 1;
            let a = (a & 0x7001) | ((a & 0x3E) << 6) | ((a & 0xFC0) >> 5);
            return m.dram[a as usize];
        }
        if (0x3A0000..0x3B0000).contains(&address) {
            let a = (address - 0x3A0000) >> 1;
            let a = (a & 0x7801) | ((a & 0x1E) << 6) | ((a & 0x7E0) >> 4);
            return m.dram[a as usize];
        }
        0xFFFF
    }

    /// A word the VDP's DMA reads. From the DRAM it arrives one word late: the DMA gets
    /// the word before the address it asks for (`svp.py` gives the evidence).
    pub fn dma_read_word(&mut self, address: u32) -> u16 {
        if (0x300000..0x320000).contains(&address) {
            return self.read_word(address - 2);
        }
        self.read_word(address)
    }

    pub fn write_word(&mut self, address: u32, value: u16) {
        if (0x300000..0x320000).contains(&address) {
            let index = ((address - 0x300000) >> 1) as usize;
            if self.memory.dram[index] != value {
                self.wake();
                self.memory.dram[index] = value;
            }
        }
    }

    pub fn write_byte(&mut self, address: u32, value: u8) {
        if (0x300000..0x320000).contains(&address) {
            let index = ((address - 0x300000) >> 1) as usize;
            let old = self.memory.dram[index];
            let new = if address & 1 != 0 {
                (old & 0xFF00) | u16::from(value)
            } else {
                (old & 0x00FF) | u16::from(value) << 8
            };
            if new != old {
                self.wake();
                self.memory.dram[index] = new;
            }
        }
    }

    /// $A15000/$A15002: the DSP's XST; $A15004: the status, bit 0 cleared by the read.
    pub fn read_a15_word(&mut self, address: u32) -> u16 {
        let offset = (address & 0xFF) >> 1;
        if offset < 2 {
            return self.memory.xst;
        }
        if offset == 2 {
            let value = self.memory.xst2;
            if value & 1 != 0 {
                self.wake();
                self.memory.xst2 &= !1;
            }
            return value;
        }
        0
    }

    pub fn write_a15(&mut self, address: u32, high: u8, low: u8, word: bool) {
        let offset = (address & 0xFF) >> 1;
        if offset < 2 {
            self.wake();
            let m = &mut self.memory;
            if word {
                m.xst = u16::from(high) << 8 | u16::from(low);
            } else if address & 1 != 0 {
                m.xst = (m.xst & 0xFF00) | u16::from(low);
            } else {
                m.xst = (m.xst & 0x00FF) | u16::from(high) << 8;
            }
            m.xst2 |= 2;
        }
    }
}
