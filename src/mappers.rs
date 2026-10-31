//! The cartridge boards that are their own class: bank mappers and protection chips.
//!
//! Transcribed from megadrive-python's `mappers.py`, whose docstrings give each board's
//! source in MAME (`bus/megadrive/`); none has a datasheet, so these are detectors, not
//! hardware. Every board is in MAME's software list under a slot name ([`Mapper::for_slot`]).
//!
//! **Byte lanes.** A register on one of these boards is a byte at one address, and the
//! 68000's 16-bit bus puts the byte at an even address on D15-D8 and at an odd address on
//! D7-D0. A word access is two byte accesses, and a word read of a protection register
//! reads the value in the high byte and $FF below it.
//!
//! **$400000-$7FFFFF.** The protection chips live there; [`Mapper::upper`] tells the
//! console's bus to ask the cartridge instead of reading open bus.
//!
//! Rust's differences: the reference's class tree is [`Kind`], one enum with the state of
//! each, and the `rb`/`wb` overrides are its match arms. An index past the end of an image
//! reads $FF here where the reference would raise (a too-small image on a banked board).
//! `Cartridge` keeps the fields the reference's base class gives every board; this module
//! takes the ROM and the battery so that a board's state is all in one place.

use crate::cartridge::{Cartridge, ROM_WINDOW};
use crate::svp::Svp;

const WINDOW: u32 = ROM_WINDOW as u32;

/// Fixed bytes in the upper window, by address.
pub type Constants = &'static [(u32, u8)];
/// Fixed words read from $A130xx, by register offset in bytes.
pub type TimeWords = &'static [(u32, u16)];

fn lookup<T: Copy>(table: &[(u32, T)], address: u32) -> Option<T> {
    table
        .iter()
        .find(|&&(at, _)| at == address)
        .map(|&(_, v)| v)
}

/// MAME's `bitswap<N>(value, a, b, ...)`: the first index lands in the top bit.
fn bitswap(value: u8, bits: [u8; 8]) -> u8 {
    bits.iter()
        .fold(0, |out, &index| (out << 1) | ((value >> index) & 1))
}

/// ST M95320, the 4 KB SPI EEPROM on Pier Solar, after MAME's `stm95.cpp`. Seven
/// instructions, each eight bits clocked in on SCK's rising edge while /CS is low. A write
/// is not delayed and block protection is ignored, as in MAME.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpiState {
    Idle,
    Wrsr,
    Rdsr,
    Read,
    Write,
    Reading,
    Writing,
}

#[derive(Clone, Debug)]
pub struct M95320 {
    state: SpiState,
    shift: u32,
    count: u32,
    address: usize,
    si: u32,
    sck: u32,
    cs: u32,
    wel: u32,
}

impl M95320 {
    pub const SIZE: usize = 0x1000;

    fn new() -> Self {
        M95320 {
            state: SpiState::Idle,
            shift: 0,
            count: 0,
            address: 0,
            si: 0,
            sck: 0,
            cs: 1,
            wel: 0,
        }
    }

    fn select(&mut self, level: u32) {
        self.cs = level;
        if level != 0 {
            self.count = 0;
            self.state = SpiState::Idle;
        }
    }

    fn so(&self) -> u16 {
        if matches!(self.state, SpiState::Reading | SpiState::Rdsr) {
            ((self.shift >> 8) & 1) as u16
        } else {
            0
        }
    }

    fn clock(&mut self, level: u32, memory: &mut [u8]) {
        let rising = level != 0 && self.sck == 0;
        self.sck = level;
        if self.cs != 0 || !rising {
            return;
        }
        match self.state {
            SpiState::Idle => {
                self.shift = (self.shift << 1) | self.si;
                self.count += 1;
                if self.count == 8 {
                    self.count = 0;
                    self.command(self.shift & 0xFF);
                }
            }
            SpiState::Wrsr => {
                self.count += 1;
                if self.count == 8 {
                    self.state = SpiState::Idle;
                    self.count = 0;
                }
            }
            SpiState::Rdsr => {
                self.shift <<= 1;
                self.count += 1;
                if self.count == 8 {
                    self.state = SpiState::Idle;
                    self.count = 0;
                }
            }
            SpiState::Read | SpiState::Write => {
                self.shift = (self.shift << 1) | self.si;
                self.count += 1;
                if self.count == 16 {
                    self.address = self.shift as usize & (Self::SIZE - 1);
                    if self.state == SpiState::Read {
                        self.shift = u32::from(memory[self.address]);
                        self.state = SpiState::Reading;
                    } else {
                        self.state = SpiState::Writing;
                    }
                    self.count = 0;
                }
            }
            SpiState::Reading => {
                self.shift <<= 1;
                self.count += 1;
                if self.count == 8 {
                    self.address = (self.address + 1) & (Self::SIZE - 1);
                    self.shift |= u32::from(memory[self.address]);
                    self.count = 0;
                }
            }
            SpiState::Writing => {
                self.shift = (self.shift << 1) | self.si;
                self.count += 1;
                if self.count == 8 {
                    memory[self.address] = self.shift as u8;
                    self.address = (self.address + 1) & (Self::SIZE - 1);
                    self.count = 0;
                }
            }
        }
    }

    fn command(&mut self, opcode: u32) {
        match opcode {
            0x01 => {
                if self.wel != 0 {
                    self.state = SpiState::Wrsr;
                }
                self.wel = 0;
            }
            0x02 => {
                if self.wel != 0 {
                    self.state = SpiState::Write;
                }
                self.shift = 0;
                self.wel = 0;
            }
            0x03 => {
                self.state = SpiState::Read;
                self.shift = 0;
            }
            0x04 => self.wel = 0,
            0x05 => {
                self.state = SpiState::Rdsr;
                self.shift = self.wel << 1;
            }
            0x06 => self.wel = 1,
            _ => {}
        }
    }
}

/// Super Mario World 64's small arithmetic device.
#[derive(Clone, Debug, Default)]
pub struct Smw64 {
    page: [u32; 2],
    latch: [u32; 3],
    ctrl: [u32; 2],
    data: [u32; 2],
    page_67: u32,
    page_61: u32,
}

/// One board's own state.
#[derive(Clone, Debug)]
pub enum Kind {
    /// Nothing but the tables on [`Mapper`]: fixed bytes, /TIME words, a RAM.
    Plain,
    RockHeaven(&'static [(u32, u16)]),
    Kof98,
    SquirrelKing { latch: u8 },
    LionKing2 { latch: [u8; 2] },
    TekkenSpecial { latch: u8 },
    Tc2000 { latch: u8 },
    ChineseFighter3 { page: Option<u32>, latch: [u8; 4] },
    TopFighter { bank: u32, latch: u8, mode: u8 },
    Realtec { size: u8, select: u8, locked: bool, slots: [u8; 0x40], pages: usize },
    MultiCartPirate { pages: usize, select: usize },
    Codemasters2in1 { game: usize },
    SuperMarioWorld64(Box<Smw64>),
    FutbolArgentino96,
    BeggarPrince { remap: bool },
    LegendOfWukong { half: usize },
    StarOdyssey { windows: bool, base: u32 },
    PierSolar { pages: usize, select: [u8; 8], chip: M95320 },
    SonicAndKnuckles { sub: Option<Box<Cartridge>>, patch: Vec<u8>, map_upper: bool },
    GameGenie { sub: Option<Box<Cartridge>>, bypass: bool, reg_enable: bool, regs: [u16; 0x20], addr: [u32; 6], data: [u16; 6] },
    Svp(Box<Svp>),
} // fmt: skip

/// A board as byte-wide reads and writes over $000000-$7FFFFF.
#[derive(Clone, Debug)]
pub struct Mapper {
    pub kind: Kind,
    /// The 4 MB window as one array (the reference's `Cartridge.rom`).
    pub rom: Vec<u8>,
    /// The image padded to a power of two.
    pub padded: Vec<u8>,
    pub constants: Constants,
    pub time_words: TimeWords,
    /// Whether the board decodes $400000-$7FFFFF itself.
    pub upper: bool,
    /// (first address, last address, chip size in bytes); the chip repeats inside.
    ram: (u32, u32, usize),
    ram_on: bool,
    locked: bool,
    pub sram: Vec<u8>,
}

const NONE: Constants = &[];
const NO_TIME: TimeWords = &[];

/// A RAM's `(first, last, size)` for the boards that have one.
const NO_RAM: (u32, u32, usize) = (0, 0, 0);

impl Mapper {
    /// True for a slot name that is its own board here.
    pub fn is_board(slot: &str) -> bool {
        Self::table(slot).is_some()
    }

    /// `(constants, time words, upper, RAM, RAM on at power-on)` for a slot, with a builder
    /// of the board's own state.
    #[allow(clippy::type_complexity)]
    fn table(
        slot: &str,
    ) -> Option<(Constants, TimeWords, bool, (u32, u32, usize), bool, fn(&[u8]) -> Kind)> {
        const fn fixed(values: Constants) -> (Constants, TimeWords, bool) {
            (values, NO_TIME, true)
        }
        fn plain(_: &[u8]) -> Kind {
            Kind::Plain
        }
        let c = |(constants, time, upper): (Constants, TimeWords, bool)| {
            Some((constants, time, upper, NO_RAM, true, plain as fn(&[u8]) -> Kind))
        };
        let upper = |build: fn(&[u8]) -> Kind| Some((NONE, NO_TIME, true, NO_RAM, true, build));
        let lower = |build: fn(&[u8]) -> Kind| Some((NONE, NO_TIME, false, NO_RAM, true, build));
        match slot {
            // Creaton, Gamtec, Chuanpu and Never Ending Soft (gamtec.cpp).
            "rom_16mj2" => c(fixed(&[(0x400002, 0x98), (0x400004, 0xC9), (0x400006, 0x18)])),
            "rom_elfwor" => c(fixed(&[(0x400000, 0x55), (0x400002, 0x0F), (0x400004, 0xC9), (0x400006, 0x18)])),
            "rom_smouse" => c(fixed(&[(0x400000, 0x55), (0x400002, 0x0F), (0x400004, 0xAA), (0x400006, 0xF0)])),
            "rom_777casino" => c(fixed(&[(0x400000, 0x63), (0x400002, 0x98), (0x400004, 0xC9), (0x400006, 0x18)])),
            "rom_soulb" => c(fixed(&[(0x400000, 0x63), (0x400002, 0x98), (0x400004, 0xC9), (0x400006, 0xF0)])),
            "rom_sbubl" => c(fixed(&[(0x400000, 0x55), (0x400002, 0x0F)])),
            "rom_cjmjclub" => c(fixed(&[(0x400000, 0x90), (0x400002, 0xD3)])),
            "rom_mjlov" => c(fixed(&[(0x400000, 0x90), (0x401000, 0xD3)])),
            "rom_rockheaven" => upper(|_| Kind::RockHeaven(&[(0x500008, 0x5082)])),
            "rockworld" => upper(|_| Kind::RockHeaven(&[(0x500008, 0x4000), (0x500208, 0xA000)])),
            // The /TIME range as read-only words: xboy.cpp and smb.cpp.
            "rom_smb" => c((NONE, &[(0x00, 0x1C)], false)),
            "rom_smb2" => c((NONE, &[(0x00, 0x0A)], false)),
            "rom_bugs" => c((NONE, &[(0x00, 0x28), (0x02, 0x01), (0x3E, 0x1F)], false)),
            "rom_pokea" => c((NONE, &[(0x00, 0x14), (0x02, 0x01), (0x3E, 0x1F)], false)),
            "rom_kof99" => c((NONE, &[(0x00, 0x00), (0x02, 0x01), (0x3E, 0x1F)], false)),
            "rom_rx3" => c((&[(0x400004, 0xC9), (0x400006, 0xF0)], &[(0x00, 0x0C)], true)),
            "rom_kof98" => upper(|_| Kind::Kof98),
            "rom_squir" => upper(|_| Kind::SquirrelKing { latch: 0xFF }),
            "rom_lion2" => upper(|_| Kind::LionKing2 { latch: [0xFF, 0xFF] }),
            "rom_tekkensp" => upper(|_| Kind::TekkenSpecial { latch: 0 }),
            "rom_tc2000" => lower(|_| Kind::Tc2000 { latch: 0 }),
            "rom_chinf3" => upper(|_| Kind::ChineseFighter3 { page: None, latch: [0xFF; 4] }),
            "rom_lion3" | "rom_pokestad" => {
                upper(|_| Kind::TopFighter { bank: 0, latch: 0, mode: 0 })
            }
            "rom_realtec" => upper(|padded| Kind::Realtec {
                size: 0,
                select: 0,
                locked: false,
                slots: [0x3F; 0x40],
                pages: (padded.len() >> 13).max(1),
            }),
            "rom_redcl" => c((&[(0x400000, 0x55), (0x400004, 0xAA)], NO_TIME, true)),
            "rom_mcpir" => lower(|padded| Kind::MultiCartPirate {
                pages: (padded.len() >> 17).max(1),
                select: 0,
            }),
            "rom_cm2in1" => lower(|_| Kind::Codemasters2in1 { game: 0 }),
            "rom_smw64" => upper(|_| {
                Kind::SuperMarioWorld64(Box::new(Smw64 { page: [8, 8], ..Smw64::default() }))
            }),
            // Battery RAM on odd boards.
            "rom_tplay96" => Some((NONE, NO_TIME, false, (0x200000, 0x20FFFF, 0x8000), true, plain)),
            "rom_sanguo5" => Some((NONE, NO_TIME, false, (0x200000, 0x203FFF, 0x2000), true, plain)),
            "rom_yasech" => Some((
                &[(0x400000, 0x63), (0x400002, 0x98), (0x400004, 0xC9), (0x400006, 0x18)],
                NO_TIME, true, (0x200000, 0x201FFF, 0x1000), true, plain,
            )),
            "rom_sram_arg96" => Some((
                &[(0x4C6201, 0x0A), (0x4C6601, 0x09), (0x4C6A01, 0x07)],
                NO_TIME, true, (0x200000, 0x203FFF, 0x2000), true,
                |_| Kind::FutbolArgentino96,
            )),
            // Super Fighter Team.
            "rom_xinqig" => Some((NONE, NO_TIME, true, (0x400000, 0x407FFF, 0x4000), true, plain)),
            "rom_sf001" => Some((NONE, NO_TIME, true, (0x400000, 0x407FFF, 0x4000), false,
                                 |_| Kind::BeggarPrince { remap: false })),
            "rom_sf001_beggarp1" => Some((NONE, NO_TIME, false, (0x3C0000, 0x3FFFFF, 0x4000), false,
                                          |_| Kind::BeggarPrince { remap: false })),
            "rom_sf002" => Some((NONE, NO_TIME, false, (0x3C0000, 0x3FFFFF, 0x2000), false,
                                 |_| Kind::LegendOfWukong { half: 1 })),
            "rom_sf004" => Some((NONE, NO_TIME, false, (0x200000, 0x2FFFFF, 0x8000), false,
                                 |_| Kind::StarOdyssey { windows: false, base: 0 })),
            // Pier Solar, the lock-on boards and the SVP.
            "rom_stm95" | "rom_psolar" => lower(|padded| Kind::PierSolar {
                pages: padded.len().div_ceil(1 << 19).max(1),
                select: [0, 1, 2, 3, 4, 5, 6, 7],
                chip: M95320::new(),
            }),
            "rom_sk" => lower(|_| Kind::SonicAndKnuckles {
                sub: None,
                patch: Vec::new(),
                map_upper: false,
            }),
            "rom_ggenie" => lower(|_| Kind::GameGenie {
                sub: None,
                bypass: false,
                reg_enable: false,
                regs: [0; 0x20],
                addr: [0; 6],
                data: [0; 6],
            }),
            "rom_svp" => lower(|padded| Kind::Svp(Box::new(Svp::new(padded)))),
            _ => None,
        }
    } // fmt: skip

    /// The board for `slot`: `rom` is the base class's 4 MB window, `image` the file.
    pub fn for_slot(slot: &str, image: &[u8], rom: Vec<u8>, padded: Vec<u8>) -> Option<Mapper> {
        let (constants, time_words, upper, ram, ram_on, build) = Self::table(slot)?;
        let kind = build(&padded);
        let mut mapper = Mapper {
            kind,
            rom,
            padded,
            constants,
            time_words,
            upper,
            ram,
            ram_on,
            locked: false,
            sram: vec![0xFF; ram.2],
        };
        if slot == "rom_redcl" {
            // The dump is XORed with $40 and four bytes too long; the decrypted image is
            // the 4 MB window, $BF (=$FF ^ $40) beyond it.
            let mut plain = vec![0xBFu8; ROM_WINDOW];
            let end = (4 + ROM_WINDOW).min(image.len());
            for (out, byte) in plain.iter_mut().zip(image.get(4..end).unwrap_or(&[])) {
                *out = byte ^ 0x40;
            }
            mapper.rom = plain;
        }
        if matches!(mapper.kind, Kind::PierSolar { .. }) {
            mapper.sram = vec![0xFF; M95320::SIZE];
        }
        Some(mapper)
    }

    fn pad(&self, index: usize) -> u8 {
        self.padded.get(index).copied().unwrap_or(0xFF)
    }

    /// The reference's `padded[i % len(padded)]`.
    fn wrapped(&self, index: usize) -> u8 {
        self.padded[index % self.padded.len()]
    }

    fn ram_index(&self, address: u32) -> Option<usize> {
        let (first, last, size) = self.ram;
        if size != 0 && self.ram_on && first <= address && address <= last {
            Some(((address - first) >> 1) as usize & (size - 1))
        } else {
            None
        }
    }

    /// `Board.rb`, the base of every board.
    fn base_rb(&self, address: u32) -> u8 {
        if address < WINDOW {
            return self.rom[address as usize];
        }
        lookup(self.constants, address).unwrap_or(0xFF)
    }

    /// `_Battery.rb` (the base, for a board with no RAM).
    fn battery_rb(&self, address: u32) -> u8 {
        if let Some(index) = self.ram_index(address) {
            return if address & 1 != 0 { self.sram[index] } else { 0xFF };
        }
        self.base_rb(address)
    }

    fn battery_wb(&mut self, address: u32, value: u8) {
        if let Some(index) = self.ram_index(address) {
            if address & 1 != 0 {
                self.sram[index] = value;
            }
        }
    }

    /// The byte the board drives at `address` (0 to $7FFFFF).
    fn rb(&mut self, address: u32) -> u8 {
        let a = address as usize;
        match &mut self.kind {
            Kind::Plain => self.battery_rb(address),
            Kind::RockHeaven(words) => match lookup(words, address & !1) {
                Some(word) if address & 1 == 0 => (word >> 8) as u8,
                Some(word) => word as u8,
                None => self.base_rb(address),
            },
            Kind::Kof98 => {
                if (0x480000..0x4C0000).contains(&address) {
                    return if address & 1 == 0 { 0xAA } else { 0 };
                }
                if (0x4C0000..0x500000).contains(&address) {
                    return if address & 1 == 0 { 0xF0 } else { 0 };
                }
                self.base_rb(address)
            }
            Kind::SquirrelKing { latch } => {
                if (0x400000..0x400008).contains(&address) && address & 1 == 0 {
                    return *latch;
                }
                self.base_rb(address)
            }
            Kind::LionKing2 { latch } => {
                if address == 0x400002 || address == 0x400006 {
                    return latch[((address >> 2) & 1) as usize];
                }
                self.base_rb(address)
            }
            Kind::TekkenSpecial { latch } => {
                if address == 0x400002 {
                    return latch.wrapping_sub(1);
                }
                self.base_rb(address)
            }
            Kind::Tc2000 { latch } => {
                if address < 0x100000 {
                    self.rom[a]
                } else if address < 0x200000 && address & 1 == 0 {
                    *latch
                } else {
                    0xFF
                }
            }
            Kind::ChineseFighter3 { page, latch } => {
                if address < 0x200000 {
                    return match *page {
                        None => self.rom[a],
                        Some(page) => {
                            let page = page as usize;
                            self.wrapped((page << 16) | (a & 0xFFFF))
                        }
                    };
                }
                if (0x400000..0x500000).contains(&address) {
                    return latch[((address >> 2) & 3) as usize];
                }
                self.base_rb(address)
            }
            Kind::TopFighter { bank, latch, mode } => {
                let (bank, latch, mode) = (*bank, *latch, *mode);
                if address < 0x200000 {
                    if bank == 0 {
                        return self.rom[a];
                    }
                    if address >= 0x100000 {
                        return 0xFF;
                    }
                    let half = (address >> 15) & 0x1F;
                    let page = if half & 1 == 0 {
                        (half | bank) & 0x3F
                    } else {
                        ((half - 1) | (bank | 1)) & 0x3F
                    };
                    return self.wrapped(((page as usize) << 15) | (a & 0x7FFF));
                }
                if (0x600000..0x700000).contains(&address) && address & 7 == 5 {
                    return match mode {
                        0 => latch << 1,
                        1 => latch >> 1,
                        2 => bitswap(latch, [3, 2, 1, 0, 7, 6, 5, 4]),
                        _ => bitswap(latch, [0, 1, 2, 3, 4, 5, 6, 7]),
                    };
                }
                self.base_rb(address)
            }
            Kind::Realtec { slots, pages, .. } => {
                if address < 0x400000 {
                    let slot = usize::from(slots[(a >> 13) & 0x3F]);
                    let page = slot % *pages;
                    return self.pad((page << 13) | (a & 0x1FFF));
                }
                self.base_rb(address)
            }
            Kind::MultiCartPirate { pages, select } => {
                if address < WINDOW {
                    let window = (a >> 17) & 3;
                    let page = (*select + window) & (*pages - 1);
                    return self.pad((page << 17) | (a & 0x1FFFF));
                }
                0xFF
            }
            Kind::Codemasters2in1 { game } => {
                if address < WINDOW {
                    let game = *game;
                    return self.wrapped((game << 21) | (a & 0x1FFFFF));
                }
                0xFF
            }
            Kind::SuperMarioWorld64(_) => self.smw64_rb(address),
            Kind::FutbolArgentino96 => {
                if let Some(index) = self.ram_index(address) {
                    return if address & 1 != 0 { self.sram[index] } else { 0xFF };
                }
                if address < WINDOW {
                    return self.pad(a & 0x1FFFFF);
                }
                lookup(self.constants, address).unwrap_or(0xFF)
            }
            Kind::BeggarPrince { remap } => {
                if *remap && address < 0x40000 {
                    return self.wrapped((0x0E << 18) | a);
                }
                self.battery_rb(address)
            }
            Kind::LegendOfWukong { half } => {
                if address >= 0x200000 && !(self.ram_on && address >= 0x3C0000) {
                    let half = *half;
                    return self.wrapped((half << 21) | (a & 0x1FFFFF));
                }
                self.battery_rb(address)
            }
            Kind::StarOdyssey { windows, base } => {
                if address < 0x200000 {
                    let (windows, base) = (*windows, *base as usize);
                    if !windows {
                        // window 0's page follows the base register in this mode too
                        return self.wrapped((base << 18) | (a & 0x3FFFF));
                    }
                    if address >= 0x140000 {
                        return 0xFF;
                    }
                    let page = (base + (a >> 18)) & 7;
                    return self.wrapped((page << 18) | (a & 0x3FFFF));
                }
                self.battery_rb(address)
            }
            Kind::PierSolar { pages, select, .. } => {
                if address < WINDOW {
                    let page = usize::from(select[a >> 19]) % *pages;
                    return self.pad((page << 19) | (a & 0x7FFFF));
                }
                0xFF
            }
            Kind::SonicAndKnuckles { .. } => {
                if let Some(value) = self.sk_patch(address) {
                    return value;
                }
                match self.sk_window(address) {
                    None => self.base_rb(address),
                    Some(where_) => {
                        let Kind::SonicAndKnuckles { sub, .. } = &mut self.kind else {
                            unreachable!()
                        };
                        sub.as_mut().expect("the window has a cartridge").read_byte(where_)
                    }
                }
            }
            Kind::GameGenie { .. } => {
                let word = self.read_word(address & !1);
                if address & 1 != 0 {
                    word as u8
                } else {
                    (word >> 8) as u8
                }
            }
            Kind::Svp(svp) => {
                let word = svp.read_word(address & !1);
                if address & 1 != 0 {
                    word as u8
                } else {
                    (word >> 8) as u8
                }
            }
        }
    }

    fn smw64_rb(&mut self, address: u32) -> u8 {
        let a = address as usize;
        if address < 0x100000 {
            return self.pad(a & 0x7FFFF);
        }
        if !(0x600000..0x700000).contains(&address) {
            return self.base_rb(address);
        }
        let Kind::SuperMarioWorld64(state) = &mut self.kind else {
            unreachable!()
        };
        match (address >> 16) & 7 {
            0 => {
                let page = state.page[0] as usize;
                self.wrapped((page << 16) | (a & 0xFFFF))
            }
            1 => {
                let page = state.page[1] as usize;
                self.wrapped((page << 16) | (a & 0xFFFF))
            }
            6 => {
                let offset = address & 0xF;
                if offset & 1 == 0 {
                    return 0xFF;
                }
                let value = match offset {
                    1 => state.latch[0],
                    3 => state.latch[0] + 1,
                    5 => state.latch[1],
                    7 => state.latch[1] + 1,
                    9 => state.latch[2],
                    11 => state.latch[2] + 1,
                    13 => state.latch[2] + 2,
                    _ => state.latch[2] + 3,
                };
                value as u8
            }
            7 => {
                if address & 1 == 0 {
                    return 0xFF;
                }
                let mut result = 0;
                if state.page_61 & 0x80 != 0 {
                    result = if state.page_67 & 0x40 != 0 {
                        state.ctrl[1] & state.data[1]
                    } else {
                        state.ctrl[1] ^ 0xFF
                    };
                }
                if address & 2 != 0 {
                    return (result & 0x7F) as u8;
                }
                if state.page_67 & 0x80 != 0 {
                    if state.page_67 & 0x20 != 0 {
                        state.latch[2] = (state.data[1] << 2) & 0xFC;
                    } else {
                        state.latch[0] = (state.data[0] ^ (state.ctrl[1] << 1)) & 0xFE;
                    }
                }
                result as u8
            }
            _ => 0xFF,
        }
    }

    /// A byte written at `address`.
    fn wb(&mut self, address: u32, value: u8) {
        match &mut self.kind {
            Kind::Plain | Kind::FutbolArgentino96 => self.battery_wb(address, value),
            Kind::SquirrelKing { latch } => {
                if (0x400000..0x400008).contains(&address) && address & 1 == 0 {
                    *latch = value;
                }
            }
            Kind::LionKing2 { latch } => {
                if address == 0x400000 || address == 0x400004 {
                    latch[((address >> 2) & 1) as usize] = value;
                }
            }
            Kind::TekkenSpecial { latch } => {
                if address == 0x400000 {
                    *latch = 0;
                } else if matches!(address, 0x400004 | 0x400006 | 0x400008 | 0x40000A)
                    && value & 1 != 0
                {
                    *latch |= 1 << ((address - 0x400004) >> 1);
                }
            }
            Kind::Tc2000 { latch } => {
                if (0x100000..0x200000).contains(&address) && address & 1 == 0 {
                    match address & 0xF {
                        0x0 => *latch = 0x00,
                        0x8 => *latch = 0x50,
                        0xC => *latch = 0xA0,
                        _ => {}
                    }
                }
            }
            Kind::ChineseFighter3 { page, latch } => {
                if (0x400000..0x500000).contains(&address) {
                    latch[((address >> 2) & 3) as usize] = value;
                } else if (0x600000..0x700000).contains(&address) && address & 1 == 0 {
                    // Only the even lane (D15-D8) counts: a word write's zero low byte
                    // must not.
                    *page = (value != 0).then_some(u32::from(value & 0xF));
                }
            }
            Kind::TopFighter { bank, latch, mode } => {
                if (0x600000..0x700000).contains(&address) {
                    if address & 7 == 1 {
                        *latch = value;
                    } else if address & 7 == 3 {
                        *mode = value & 3;
                    }
                } else if address >= 0x700000 {
                    *bank = u32::from(value);
                }
            }
            Kind::Realtec { size, select, locked, slots, .. } => {
                if address == 0x402000 {
                    *size = (value & 3) << 4;
                } else if address == 0x404000 {
                    *select = (value & 3) << 4;
                } else if address == 0x400000 && !*locked && value & 1 != 0 {
                    let base = *select & *size;
                    for (i, slot) in slots.iter_mut().enumerate() {
                        *slot = ((i as u8 & !*size) | base) & 0x3F;
                    }
                    *locked = true;
                }
            }
            Kind::SuperMarioWorld64(state) => {
                if !(0x600000..0x700000).contains(&address) {
                    return;
                }
                let value = u32::from(value);
                match (address >> 16) & 7 {
                    0 => {
                        if address & 3 == 1 {
                            state.ctrl[0] = value;
                        } else if address & 3 == 3 {
                            match state.ctrl[0] & 7 {
                                0 => {
                                    state.latch[0] =
                                        ((state.latch[0] ^ state.data[0]) ^ value) & 0xFE
                                }
                                1 => state.latch[1] = value & 0xFE,
                                7 => state.page[1] = 8 + ((value & 0x1C) >> 2),
                                _ => {}
                            }
                            state.data[0] = value;
                        }
                    }
                    1 => {
                        if address & 3 == 3 {
                            state.page_61 = value;
                        }
                    }
                    4 => {
                        if address & 3 == 1 {
                            state.ctrl[1] = value;
                        } else if address & 3 == 3 {
                            state.data[1] = value;
                        }
                    }
                    7 => {
                        if address & 3 == 1 {
                            state.page_67 = value;
                            if state.page_61 & 0x80 != 0 {
                                state.page[0] = 8 + ((value & 0x1C) >> 2);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Kind::BeggarPrince { remap } => {
                if address == 0xE01 && !self.locked {
                    let on = value & 0x80 != 0;
                    *remap = on;
                    self.ram_on = on;
                    self.locked = value & 0x20 != 0;
                } else {
                    self.battery_wb(address, value);
                }
            }
            Kind::LegendOfWukong { half } => {
                if address == 0xE01 {
                    *half = if value & 0x80 != 0 { 0 } else { 1 };
                    self.ram_on = value & 1 != 0;
                } else {
                    self.battery_wb(address, value);
                }
            }
            Kind::StarOdyssey { windows, base } => {
                if address == 0xD01 && !self.locked {
                    self.ram_on = value & 0x80 != 0;
                } else if address == 0xE01 && !self.locked {
                    self.locked = value & 0x80 == 0;
                    *windows = value & 0x40 != 0;
                } else if address == 0xF01 && !self.locked {
                    *base = u32::from((value >> 4) & 7);
                } else {
                    self.battery_wb(address, value);
                }
            }
            Kind::SonicAndKnuckles { .. } => {
                if let Some(where_) = self.sk_window(address) {
                    let Kind::SonicAndKnuckles { sub, .. } = &mut self.kind else {
                        unreachable!()
                    };
                    sub.as_mut().expect("the window has a cartridge").write_byte(where_, value);
                }
            }
            Kind::GameGenie { .. } | Kind::Svp(_) => self.write_byte(address, value),
            _ => {}
        }
    }

    // -- Sonic & Knuckles ------------------------------------------------------------

    /// The 256 KB patch ROM's byte, where it shows.
    fn sk_patch(&self, address: u32) -> Option<u8> {
        let Kind::SonicAndKnuckles { patch, map_upper, .. } = &self.kind else {
            return None;
        };
        if !patch.is_empty() && *map_upper && (0x300000..0x340000).contains(&address) {
            return Some(patch[(address as usize - 0x300000) & (patch.len() - 1)]);
        }
        None
    }

    /// The second cartridge's address for `address`, or None for the S&K ROM.
    fn sk_window(&self, address: u32) -> Option<u32> {
        let Kind::SonicAndKnuckles { sub, map_upper, .. } = &self.kind else {
            return None;
        };
        if sub.is_none() || address < 0x200000 || address >= WINDOW {
            return None;
        }
        if *map_upper {
            return (address < 0x300000).then_some(address);
        }
        Some(address - 0x200000)
    }

    // -- the 68000's paths -----------------------------------------------------------

    pub fn read_word(&mut self, address: u32) -> u16 {
        match &mut self.kind {
            Kind::SonicAndKnuckles { .. } => {
                if self.sk_patch(address).is_none() {
                    if let Some(where_) = self.sk_window(address) {
                        let Kind::SonicAndKnuckles { sub, .. } = &mut self.kind else {
                            unreachable!()
                        };
                        return sub
                            .as_mut()
                            .expect("the window has a cartridge")
                            .read_word(where_);
                    }
                }
                u16::from(self.rb(address)) << 8 | u16::from(self.rb(address | 1))
            }
            Kind::GameGenie { sub, bypass, reg_enable, regs, addr, data } => {
                if address >= WINDOW {
                    return 0xFFFF;
                }
                let a = address as usize;
                match sub {
                    Some(sub) if *bypass => {
                        for index in 0..6 {
                            if address >> 1 == addr[index] >> 1 && (regs[0] >> index) & 1 != 0 {
                                return data[index];
                            }
                        }
                        sub.read_word(address)
                    }
                    _ => {
                        if *reg_enable {
                            return regs[(a >> 1) & 0x1F];
                        }
                        u16::from(self.rom[a]) << 8 | u16::from(self.rom[a + 1])
                    }
                }
            }
            Kind::Svp(svp) => svp.read_word(address),
            _ => u16::from(self.rb(address)) << 8 | u16::from(self.rb(address | 1)),
        }
    }

    pub fn read_byte(&mut self, address: u32) -> u8 {
        if matches!(self.kind, Kind::PierSolar { .. }) && (0x18100..0x18200).contains(&address) {
            return self.pad(address as usize & 0x7FFF);
        }
        self.rb(address)
    }

    pub fn write_word(&mut self, address: u32, value: u16) {
        match &mut self.kind {
            Kind::TopFighter { bank, .. } if address >= 0x700000 => {
                // A word write counts by its low byte, not its (zero) high one.
                *bank = u32::from(value & 0xFF);
            }
            Kind::SonicAndKnuckles { .. } => {
                if let Some(where_) = self.sk_window(address) {
                    let Kind::SonicAndKnuckles { sub, .. } = &mut self.kind else {
                        unreachable!()
                    };
                    sub.as_mut().expect("the window has a cartridge").write_word(where_, value);
                }
            }
            Kind::GameGenie { .. } => {
                if address < 0x40 {
                    self.genie_lanes((address >> 1) as usize, value, 0xFFFF);
                }
            }
            Kind::Svp(svp) => svp.write_word(address, value),
            _ => {
                self.wb(address & !1, (value >> 8) as u8);
                self.wb(address | 1, value as u8);
            }
        }
    }

    pub fn write_byte(&mut self, address: u32, value: u8) {
        match &mut self.kind {
            Kind::GameGenie { .. } => {
                if address < 0x40 {
                    if address & 1 != 0 {
                        self.genie_lanes((address >> 1) as usize, u16::from(value), 0x00FF);
                    } else {
                        self.genie_lanes((address >> 1) as usize, u16::from(value) << 8, 0xFF00);
                    }
                }
            }
            Kind::Svp(svp) => svp.write_byte(address, value),
            _ => self.wb(address, value),
        }
    }

    fn genie_lanes(&mut self, offset: usize, value: u16, mask: u16) {
        let Kind::GameGenie { bypass, reg_enable, regs, addr, data, .. } = &mut self.kind else {
            return;
        };
        regs[offset] = (regs[offset] & !mask) | (value & mask);
        if offset == 0 {
            if value & 0x400 != 0 {
                *bypass = true;
            } else {
                *bypass = false;
                *reg_enable = value & 0x200 != 0;
            }
            if value & 0x100 != 0 {
                for index in 0..6 {
                    addr[index] = u32::from(regs[2 + 3 * index] & 0x3F) << 16
                        | u32::from(regs[3 + 3 * index]);
                    data[index] = regs[4 + 3 * index];
                }
            }
        } else if offset == 1 {
            regs[1] |= 1;
        }
    }

    /// A word the VDP's DMA reads.
    pub fn dma_read_word(&mut self, address: u32) -> u16 {
        match &mut self.kind {
            Kind::Svp(svp) => svp.dma_read_word(address),
            _ => self.read_word(address),
        }
    }

    // -- /TIME, $A13000-$A130FF ------------------------------------------------------

    pub fn read_time_word(&mut self, address: u32) -> Option<u16> {
        match &mut self.kind {
            Kind::StarOdyssey { base, .. } => Some(0xFF00 | (*base as u16) << 4),
            Kind::PierSolar { chip, .. } => {
                (address & 0xFE == 0x0A).then_some(0xFF00 | chip.so())
            }
            Kind::SonicAndKnuckles { sub, .. } => sub.as_mut()?.read_time_word(address),
            _ => lookup(self.time_words, address & 0xFE),
        }
    }

    /// A write to $A130xx with the byte lanes the 68000 drove: a byte write puts its byte
    /// on both and only the lane at `address` is the register's. The word form's second
    /// offset is the whole `address | 1`, as the reference passes it.
    pub fn write_time_lanes(&mut self, address: u32, high: u8, low: u8, word: bool) {
        if let Kind::SonicAndKnuckles { sub, map_upper, .. } = &mut self.kind {
            if matches!(address & 0xFF, 0xF0 | 0xF1) && (word || address & 1 != 0) {
                *map_upper = low & 1 != 0;
            }
            if let Some(sub) = sub {
                sub.write_time_lanes(address, high, low, word);
            }
            return;
        }
        if word {
            self.time_byte(address & 0xFE, high);
            self.time_byte(address | 1, low);
        } else {
            self.time_byte(address & 0xFF, low);
        }
    }

    fn time_byte(&mut self, offset: u32, value: u8) {
        match &mut self.kind {
            Kind::MultiCartPirate { select, .. } => {
                if offset & 1 == 0 {
                    *select = ((offset & 0x3E) >> 1) as usize;
                }
            }
            Kind::PierSolar { select, chip, .. } => {
                if matches!(offset, 3 | 5 | 7) {
                    select[4 + (offset >> 1) as usize] = value & 0xF;
                } else if offset == 9 {
                    let value = u32::from(value);
                    chip.si = value & 1;
                    chip.clock((value >> 1) & 1, &mut self.sram);
                    chip.select((value >> 3) & 1);
                }
            }
            _ => {}
        }
    }

    /// The console's reset button reached the cartridge connector.
    pub fn reset(&mut self) {
        match &mut self.kind {
            Kind::Codemasters2in1 { game } => *game ^= 1,
            Kind::SonicAndKnuckles { sub, map_upper, .. } => {
                *map_upper = false;
                if let Some(sub) = sub {
                    sub.reset();
                }
            }
            Kind::GameGenie { sub, bypass, reg_enable, regs, addr, data } => {
                *bypass = false;
                *reg_enable = false;
                *regs = [0; 0x20];
                *addr = [0; 6];
                *data = [0; 6];
                if let Some(sub) = sub {
                    sub.reset();
                }
            }
            _ => {}
        }
    }

    // -- lock-on and the battery -----------------------------------------------------

    /// Whether the board has a socket on top.
    pub fn has_socket(&self) -> bool {
        matches!(self.kind, Kind::SonicAndKnuckles { .. } | Kind::GameGenie { .. })
    }

    pub fn attach(&mut self, cartridge: Cartridge, patch_rom: Vec<u8>) {
        match &mut self.kind {
            Kind::SonicAndKnuckles { sub, patch, .. } => {
                *sub = Some(Box::new(cartridge));
                *patch = patch_rom;
            }
            Kind::GameGenie { sub, .. } => *sub = Some(Box::new(cartridge)),
            _ => {}
        }
    }

    fn sub(&self) -> Option<Option<&Cartridge>> {
        match &self.kind {
            Kind::SonicAndKnuckles { sub, .. } | Kind::GameGenie { sub, .. } => {
                Some(sub.as_deref())
            }
            _ => None,
        }
    }

    pub fn battery(&self) -> &'static str {
        match self.sub() {
            Some(Some(sub)) => sub.battery(),
            Some(None) => "",
            None if self.sram.is_empty() => "",
            None => "sram",
        }
    }

    pub fn save_bytes(&self) -> Vec<u8> {
        match self.sub() {
            Some(Some(sub)) => sub.save_bytes(),
            Some(None) => Vec::new(),
            None => self.sram.clone(),
        }
    }

    pub fn load_bytes(&mut self, data: &[u8]) {
        match &mut self.kind {
            Kind::SonicAndKnuckles { sub, .. } | Kind::GameGenie { sub, .. } => {
                if let Some(sub) = sub {
                    sub.load_bytes(data);
                }
            }
            _ => {
                let n = data.len().min(self.sram.len());
                self.sram[..n].copy_from_slice(&data[..n]);
            }
        }
    }

    /// The SVP's DSP is one more clock domain.
    pub fn catch_up(&mut self, mclk: i64) {
        if let Kind::Svp(svp) = &mut self.kind {
            svp.catch_up(mclk);
        }
    }

    /// The SVP's trace line, on the board that has one.
    pub fn svp_line(&self) -> Option<String> {
        match &self.kind {
            Kind::Svp(svp) => Some(svp.trace_line()),
            _ => None,
        }
    }

    pub fn clocked(&self) -> bool {
        matches!(self.kind, Kind::Svp(_))
    }

    pub fn read_a15_word(&mut self, address: u32) -> Option<u16> {
        match &mut self.kind {
            Kind::Svp(svp) => Some(svp.read_a15_word(address)),
            _ => None,
        }
    }

    pub fn write_a15(&mut self, address: u32, high: u8, low: u8, word: bool) {
        if let Kind::Svp(svp) = &mut self.kind {
            svp.write_a15(address, high, low, word);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(slot: &str) -> Mapper {
        let image = vec![0u8; 0x100000];
        let padded = image.clone();
        Mapper::for_slot(slot, &image, vec![0; ROM_WINDOW], padded).unwrap()
    }

    #[test]
    fn a_protection_word_reads_the_value_high_and_ff_low() {
        let mut m = board("rom_16mj2");
        assert_eq!(m.read_word(0x400002), 0x98FF);
        assert!(m.upper);
    }

    #[test]
    fn time_words_are_the_boards_own() {
        let mut m = board("rom_bugs");
        assert_eq!(m.read_time_word(0xA1303E), Some(0x1F));
        assert_eq!(m.read_time_word(0xA13010), None);
        assert!(!m.upper);
    }

    #[test]
    fn m95320_reads_back_what_was_written() {
        fn send(chip: &mut M95320, memory: &mut [u8], byte: u32) {
            for bit in (0..8).rev() {
                chip.si = (byte >> bit) & 1;
                chip.clock(1, memory);
                chip.clock(0, memory);
            }
        }
        let mut memory = vec![0xFF; M95320::SIZE];
        let mut chip = M95320::new();
        chip.select(0);
        send(&mut chip, &mut memory, 0x06);
        chip.select(1);
        chip.select(0);
        for byte in [0x02, 0x00, 0x10, 0x5A] {
            send(&mut chip, &mut memory, byte);
        }
        chip.select(1);
        assert_eq!(memory[0x10], 0x5A);
    }
}
