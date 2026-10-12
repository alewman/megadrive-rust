//! The VDP (315-5313 / YM7101): ports, FIFO, DMA, counters, status and interrupts.
//!
//! Transcribed from megadrive-python's `vdp.py`, whose module docstring gives each
//! rule's source: the pixel durations, H and V counter sequences and event positions
//! are Nemesis's logic-analyser work as tabulated in Exodus (MIT; `S315-5313_Timing.cpp`),
//! agreeing with Kabuto's table; the HV counter after Exodus `GetHVCounter` and CMD
//! `genvdp.txt`, sampled as fitted to Nemesis's captures; the access slots after
//! Exodus, Kabuto and MoD; and the ports, FIFO, read cache and DMA after the model
//! Nemesis built beside VDPFIFOTesting (Exodus `S315-5313_Ports.cpp`,
//! `S315-5313_General.cpp`), with the die-derived constants below measured on Nuked-MD.
//!
//! The VDP catches up lazily: [`Vdp::sync`] brings it to a master clock, doing the slot
//! work and the line events on the way. After a 68000 port access, `hold` is the MCLK
//! the access held the 68000.
//!
//! Rust's differences:
//! - The VDP's state is [`VdpState`] and the picture [`Renderer`], two fields of
//!   [`Vdp`], so the renderer can be handed the state it reads.
//! - What the reference reaches through `self.board` (DMA reads, open bus, the PSG,
//!   the two refresh clocks) is a [`VdpHost`] passed in.
//! - The `until` predicates of `_advance` are the [`Until`] enum, and the reference's
//!   identity test `self.slots is self.hmode.slots_active` is [`Slots`].
//! - The H and V mode tables are built once, in [`tables`].

use std::sync::OnceLock;

use crate::board::{ram_refresh_paused, CART_REFRESH_MCLK};
use crate::machine::{Machine, MCLK_PER_LINE};
use crate::render::Renderer;

/// What the VDP reaches on the board: the reference's `self.board`.
pub trait VdpHost {
    /// A word the VDP's 68000-to-VDP DMA reads from the 68000's bus.
    fn dma_read_word(&mut self, address: u32) -> u16;
    /// The 68000's undriven data bus: the next opcode word it will fetch.
    fn open_bus(&self) -> u16;
    /// Run both sound chips up to `mclk`.
    fn sound_sync(&mut self, mclk: i64);
    fn psg_write(&mut self, value: u8);
    fn psg_write_test(&mut self, bits: u8);
    fn cart_refresh_due(&mut self) -> &mut i64;
    fn ram_refresh_due(&mut self) -> &mut i64;
}

// -- the horizontal scan ------------------------------------------------------------

/// MCLK per pixel in H40's sync region by internal H counter value (Exodus
/// `mclkTicksForHCounterH40EDClkArray`); 8 everywhere else.
fn h40_sync(h: u16) -> i64 {
    match h {
        0x1CD..=0x1D3 => 10,
        0x1D4 | 0x1D5 => 9,
        0x1D6..=0x1DC => 10,
        0x1DD => 8,
        0x1DE..=0x1E4 => 10,
        0x1E5 | 0x1E6 => 9,
        0x1E7..=0x1ED => 10,
        _ => 8,
    }
}

/// The HV counter as the port gives it, from the internal V and H counters: H's upper
/// eight bits; V's lower eight, or in interlace its bits 7-1 and 8 (doubled: 6-0 and
/// 7) (Exodus `S315-5313_Ports.cpp` 1666-1730; CMD `genvdp.txt`).
pub fn hv_word(v: i64, h: u16, interlace: bool, double: bool) -> u16 {
    let v = if double { v << 1 } else { v };
    let mut external = v & 0xFF;
    if interlace {
        external = (external & 0xFE) | ((v >> 8) & 1);
    }
    ((external << 8) as u16) | (h >> 1)
}

/// The pixel clock regime of a pixel (`HVUnit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Regime {
    H32,
    H40,
    H40Long,
}

/// An edge rule: "new", "old", or an integer n (old AND new when the increment
/// changes bit n or above, else new).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    New,
    Old,
    Bits(u32),
    Or,
}

/// What one measured console's HV counter reads show at the two sampling phases where
/// a read catches a counter changing (Nemesis's captures; the reference's docstring
/// explains each field).
pub struct HVUnit {
    pub name: &'static str,
    edge: [(Regime, Rule); 3],
    jumps: &'static [(Regime, u16, u16, u16)],
    v_carry1: &'static [(Regime, Rule)],
    next_clear: &'static [(Regime, u32)],
    late: &'static [(Regime, u32)],
}

impl HVUnit {
    pub fn h_at_edge(&self, regime: Regime, old: u16, new: u16) -> u16 {
        if new != old.wrapping_add(1) {
            return self
                .jumps
                .iter()
                .find(|&&(r, o, n, _)| r == regime && o == old && n == new)
                .map_or(new, |&(_, _, _, value)| value);
        }
        let rule = self
            .edge
            .iter()
            .find(|(r, _)| *r == regime)
            .expect("every regime")
            .1;
        match rule {
            Rule::Old => old,
            Rule::Bits(bits) if (old ^ new) >> bits != 0 => old & new,
            _ => new,
        }
    }

    pub fn h_late(&self, regime: Regime, old: u16, new: u16) -> u16 {
        match self.late.iter().find(|(r, _)| *r == regime) {
            Some(&(_, bits)) if new == old.wrapping_add(1) && (old ^ new) >> bits != 0 => old & new,
            _ => new,
        }
    }

    pub fn v_at_edge(&self, regime: Regime, old: i64, new: i64) -> i64 {
        if new == old + 1 && old & 3 == 1 {
            match self.v_carry1.iter().find(|(r, _)| *r == regime) {
                Some((_, Rule::Old)) => return old,
                Some((_, Rule::Or)) => return old | new,
                _ => {}
            }
        }
        new
    }

    pub fn h_at_next(&self, regime: Regime, h: u16, following: u16) -> u16 {
        let bits = self
            .next_clear
            .iter()
            .find(|(r, _)| *r == regime)
            .map_or(0, |&(_, b)| b);
        if bits != 0 && following == h.wrapping_add(1) && (h ^ following) >> bits != 0 {
            return h & !((1 << bits) - 1);
        }
        h
    }
}

/// PAL Model 1: H32 edge reads: 30,033 of 30,033 fit the rule; the jump reads $E1 54
/// times in 67; the V counter carrying into bit 1 shows old 13 times in 22. H40: the
/// 8-MCLK pixels read cleanly; the long ones show the old value.
pub static PAL_MODEL1: HVUnit = HVUnit {
    name: "Nemesis's PAL Model 1",
    edge: [
        (Regime::H32, Rule::Bits(2)),
        (Regime::H40, Rule::New),
        (Regime::H40Long, Rule::Old),
    ],
    jumps: &[(Regime::H32, 0x127, 0x1D2, 0x1C2)],
    v_carry1: &[(Regime::H32, Rule::Old)],
    next_clear: &[],
    late: &[],
};
/// NTSC-J Model 1: every rule fits all but a few percent of its reads.
pub static NTSCJ_MODEL1: HVUnit = HVUnit {
    name: "Nemesis's NTSC-J Model 1",
    edge: [
        (Regime::H32, Rule::Bits(3)),
        (Regime::H40, Rule::Bits(2)),
        (Regime::H40Long, Rule::Old),
    ],
    jumps: &[
        (Regime::H32, 0x127, 0x1D2, 0x1C2),
        (Regime::H40, 0x16C, 0x1C9, 0x148),
    ],
    v_carry1: &[(Regime::H32, Rule::Or)],
    next_clear: &[(Regime::H32, 3)],
    late: &[(Regime::H40Long, 2)],
};

pub fn hv_unit(name: &str) -> &'static HVUnit {
    match name {
        "pal-model1" => &PAL_MODEL1,
        "ntscj-model1" => &NTSCJ_MODEL1,
        _ => panic!("unknown HV unit {name:?}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HName {
    H32,
    H40,
}

/// One horizontal mode's line: the H counter sequence and when each value starts.
///
/// `hvals[i]` is the internal H counter during pixel `i` of the line, the line starting
/// at the V counter increment point; `starts[i]` is that pixel's first MCLK from the
/// line start. `slots_active`/`slots_idle` are the access slots' offsets on an active
/// line and on an idle one.
pub struct HMode {
    pub name: HName,
    /// Which of the five line tables this is.
    pub id: usize,
    pub hvals: Vec<u16>,
    pub starts: Vec<i64>,
    /// MCLK in the line: MCLK_PER_LINE but on some of H40's VSYNC lines in an
    /// interlaced odd field.
    pub length: i64,
    offset_of: Vec<i64>,
    pub vinc: u16,
    pub hblank_set: u16,
    pub hblank_clear: u16,
    pub last: u16,
    pub first_high: u16,
    pub slots_active: Vec<i64>,
    pub no_read_fill: Vec<i64>,
    pub slots_idle: Vec<i64>,
    /// The first blanked line's (`FIRST_IDLE_SLOT_H`).
    pub slots_first_idle: Vec<i64>,
    /// When a transfer can read with the die's DMA figures (`dma_ticks`).
    pub dma_ticks: Vec<i64>,
    /// Where display pixel 0 falls in `hvals`.
    pub display_at: usize,
    /// The dot clock's edges, by line offset (`DOT_CLOCK_SLOW_WINDOWS`).
    dot_edges: Vec<bool>,
}

fn bisect_right(list: &[i64], value: i64) -> usize {
    list.partition_point(|&x| x <= value)
}

fn bisect_left(list: &[i64], value: i64) -> usize {
    list.partition_point(|&x| x < value)
}

/// Python's `list[i]` for an index that may be -1.
fn wrap(len: usize, index: i64) -> usize {
    index.rem_euclid(len as i64) as usize
}

impl HMode {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: HName,
        id: usize,
        last: u16,
        first_high: u16,
        vinc: u16,
        hblank_set: u16,
        hblank_clear: u16,
        pixel_mclk: impl Fn(u16) -> i64,
    ) -> Self {
        let sequence: Vec<u16> = (0..=last).chain(first_high..0x200).collect();
        let start = sequence
            .iter()
            .position(|&h| h == vinc)
            .expect("vinc is in the sequence");
        let hvals: Vec<u16> = sequence[start..]
            .iter()
            .chain(&sequence[..start])
            .copied()
            .collect();
        let mut starts = Vec::with_capacity(hvals.len());
        let mut total = 0;
        for &h in &hvals {
            starts.push(total);
            total += pixel_mclk(h);
        }
        let mut offset_of = vec![-1; 0x200];
        for (&h, &s) in hvals.iter().zip(&starts) {
            offset_of[usize::from(h)] = s;
        }
        HMode {
            name,
            id,
            hvals,
            starts,
            length: total,
            offset_of,
            vinc,
            hblank_set,
            hblank_clear,
            last,
            first_high,
            slots_active: Vec::new(),
            no_read_fill: Vec::new(),
            slots_idle: Vec::new(),
            slots_first_idle: Vec::new(),
            dma_ticks: Vec::new(),
            display_at: 0,
            dot_edges: Vec::new(),
        }
    }

    /// The MCLK offset at which H counter value `h` starts, if the line has it.
    pub fn offset_of(&self, h: u16) -> Option<i64> {
        let offset = self.offset_of[usize::from(h & 0x1FF)];
        (offset >= 0).then_some(offset)
    }

    fn at(&self, h: u16) -> i64 {
        self.offset_of(h).expect("the line has this H")
    }

    /// The index of the pixel under way `offset` MCLK into the line (-1 before it).
    pub fn position(&self, offset: i64) -> i64 {
        bisect_right(&self.starts, offset) as i64 - 1
    }

    /// The first pixel boundary at or after `offset` MCLK into the line (the line's end
    /// when there is none left in it).
    pub fn next_edge(&self, offset: i64) -> i64 {
        let index = bisect_left(&self.starts, offset);
        self.starts.get(index).copied().unwrap_or(self.length)
    }

    pub fn hval(&self, index: i64) -> u16 {
        self.hvals[wrap(self.hvals.len(), index)]
    }
}

/// The dot clock the FC1004 feeds the VDP changes every 2 MCLK, but in each run of
/// long pixels it runs at MCLK/5: two windows of 30 edges, 3 and 2 MCLK apart by turns,
/// starting this far into the run (Nuked-MD, `scripts/run_refresh_tie.py`).
pub const DOT_CLOCK_SLOW_WINDOWS: [i64; 2] = [4, 87];
pub const DOT_CLOCK_SLOW_EDGES: i64 = 30;
/// Those offsets are on the VDP's internal H counter, which runs this far behind the H
/// the board's line follows. Die-derived.
pub const DOT_CLOCK_LINE_LAG_MCLK: i64 = 15;

/// The line offsets (MCLK from the line start) of the dot clock's edges.
fn dot_clock_edges(mode: &HMode) -> Vec<bool> {
    let ends: Vec<i64> = mode.starts[1..]
        .iter()
        .copied()
        .chain([mode.length])
        .collect();
    let mut windows = Vec::new();
    let mut i = 0;
    while i < mode.hvals.len() {
        if ends[i] - mode.starts[i] > 8 {
            // a run of long pixels
            let start = mode.starts[i];
            windows.extend(DOT_CLOCK_SLOW_WINDOWS.iter().map(|w| start + w));
            while i < mode.hvals.len() && ends[i] - mode.starts[i] > 8 {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    let mut edges = vec![false; mode.length as usize];
    let mut mark = |t: i64| {
        if (0..mode.length).contains(&t) {
            edges[t as usize] = true;
        }
    };
    let mut t = 0;
    for window in windows {
        while t < window {
            mark(t);
            t += 2;
        }
        t = window; // the windows start on the 2-MCLK grid
        for k in 0..DOT_CLOCK_SLOW_EDGES {
            mark(t);
            t += if k % 2 == 0 { 3 } else { 2 };
        }
        t -= if (DOT_CLOCK_SLOW_EDGES - 1) % 2 == 0 {
            3
        } else {
            2
        }; // back to the last edge
        t += 2;
    }
    while t < mode.length {
        mark(t);
        t += 2;
    }
    edges
}

/// Around VSYNC, H40's second run of long pixels ($1DE-$1ED) moves: on nine lines those
/// pixels last 8 MCLK and $09F-$0AE take their lengths instead (Nemesis's captures;
/// docs/timing.md, "H40 VSYNC lines").
fn h40_pixels(short_1de: bool, long_9f: bool) -> impl Fn(u16) -> i64 {
    move |h| {
        if long_9f && (0x09F..0x09F + 16).contains(&h) {
            return h40_sync(0x1DE + (h - 0x09F));
        }
        if short_1de && (0x1DE..=0x1ED).contains(&h) {
            return 8;
        }
        h40_sync(h)
    }
}

pub const VSYNC_LINES: usize = 9;

const ACTIVE_SLOTS_H32: [u16; 16] = [
    0x005, 0x015, 0x025, 0x045, 0x055, 0x065, 0x085, 0x095, 0x0A5, 0x0C5, 0x0D5, 0x0E5, 0x103,
    0x105, 0x121, 0x1E7,
];
const ACTIVE_SLOTS_H40: [u16; 18] = [
    0x002, 0x012, 0x022, 0x042, 0x052, 0x062, 0x082, 0x092, 0x0A2, 0x0C2, 0x0D2, 0x0E2, 0x102,
    0x112, 0x122, 0x140, 0x142, 0x1CE,
];
/// Where the first line of the vertical border begins its idle access slots: H $152 in
/// H40 (Nuked-MD; docs/worklog.md, "The DMA's read into room"). H32 is not measured.
pub const FIRST_IDLE_SLOT_H40: u16 = 0x152;
/// Active-line slots that fill no data-port read ([`READ_RELEASE_MCLK`]): H40's $142.
const NO_READ_FILL_H40: [u16; 1] = [0x142];
/// The refresh slots of an idle line, by H counter, from Exodus's own tables
/// ("Refresh cycle slots", Timing.cpp 1463-1520).
const REFRESH_H32: [u16; 5] = [0x035, 0x075, 0x0B5, 0x0F5, 0x1F5];
const REFRESH_H40: [u16; 6] = [0x032, 0x072, 0x0B2, 0x0F2, 0x132, 0x1F2];

fn refresh_of(name: HName) -> &'static [u16] {
    match name {
        HName::H32 => &REFRESH_H32,
        HName::H40 => &REFRESH_H40,
    }
}

/// Where the DMA reads' refresh gaps fall against the board's own H counter: 4 after
/// Exodus's positions in H40. Die-derived.
pub fn dma_refresh_shift(name: HName) -> u16 {
    if name == HName::H40 {
        4
    } else {
        0
    }
}
/// An idle line's write slots lose the slot two H after that. Die-derived.
pub fn idle_refresh_slot_shift(name: HName) -> u16 {
    if name == HName::H40 {
        6
    } else {
        0
    }
}

/// Slots on an idle line: every odd (H32) or even (H40) H counter value except the
/// refresh slots (Kabuto, "Memory access pattern"; one slot lost per refresh, as Kabuto
/// and the Manual's DMA table have it, not Exodus's two).
fn idle_slots(mode: &HMode) -> Vec<i64> {
    let parity = if mode.name == HName::H32 { 1 } else { 0 };
    let shift = idle_refresh_slot_shift(mode.name);
    let refresh: Vec<u16> = refresh_of(mode.name).iter().map(|h| h + shift).collect();
    let mut slots: Vec<i64> = mode
        .hvals
        .iter()
        .filter(|&&h| h % 2 == parity && !refresh.contains(&h))
        .map(|&h| mode.at(h))
        .collect();
    slots.sort_unstable();
    slots
}

/// When a 68000-to-VDP transfer can read, with the die's figures: every slot tick (two
/// pixels) of the line but each refresh's and the one after it.
fn dma_ticks(mode: &HMode) -> Vec<i64> {
    let parity = if mode.name == HName::H32 { 1 } else { 0 };
    let ticks: Vec<u16> = mode
        .hvals
        .iter()
        .copied()
        .filter(|h| h % 2 == parity)
        .collect();
    let shift = dma_refresh_shift(mode.name);
    let refresh: Vec<u16> = refresh_of(mode.name).iter().map(|h| h + shift).collect();
    let mut skip = Vec::new();
    for (i, &h) in ticks.iter().enumerate() {
        if refresh.contains(&h) {
            skip.push(h);
            skip.push(ticks[(i + 1) % ticks.len()]);
        }
    }
    let mut out: Vec<i64> = ticks
        .iter()
        .filter(|h| !skip.contains(h))
        .map(|&h| mode.at(h))
        .collect();
    out.sort_unstable();
    out
}

/// Where in a line the full-screen vertical scroll is latched, by internal H counter,
/// planes A and B (Nuked-MD's `l184`/`l185`; docs/worklog.md, "The vertical scroll
/// latch"). Die-derived.
pub fn vscroll_latch_h(name: HName) -> [u16; 2] {
    match name {
        HName::H40 => [0x149, 0x143],
        HName::H32 => [0x109, 0x103],
    }
}

/// Registers that say where or how the planes' tiles are fetched: a write to one
/// freezes every tile whose name has been fetched, as a name-table write does.
pub const PLANE_REGISTERS: [usize; 8] = [2, 3, 4, 11, 12, 16, 17, 18];

/// The command address is 17 bits, not 16 (Nuked-MD, `reg_addr`; Kabuto, "Precise
/// details of sprite rendering"); Overdrive 2's rain is drawn that way.
pub const ADDRESS_MASK: u32 = 0x1FFFF;

/// Where in its line each 2-cell column's vertical scroll is latched, as (the board's
/// internal H for column 0, the step per column), planes A and B (Nuked-MD;
/// docs/worklog.md, "The 2-cell column latch").
pub fn vscroll_column_h(name: HName) -> [(u16, u16); 2] {
    match name {
        HName::H40 => [(0x1FE, 16), (0x006, 16)],
        HName::H32 => [(0x001, 16), (0x009, 16)],
    }
}

// -- the vertical scan --------------------------------------------------------------

/// The V counter sequence: 0..`last`, then `jump`..$1FF; on an odd interlaced field
/// one extra line `jump - 1` (Exodus vertical tables). VBlank sets and VINT fires on
/// line `vblank_set`.
pub struct VMode {
    pub vsync: Option<i64>,
    pub last: i64,
    pub jump: i64,
    pub vblank_set: i64,
    pub active_last: i64,
    pub interlaced_last: i64,
    /// For (interlace, odd) as index 2 * interlace + odd: the lines whose $1DE-$1ED are
    /// short, and those whose $09F-$0AE are long.
    vsync_lines: [(Vec<bool>, Vec<bool>); 4],
}

impl VMode {
    fn new(
        last: i64,
        jump: i64,
        vblank_set: i64,
        vsync: Option<i64>,
        interlaced_last: Option<i64>,
    ) -> Self {
        let mut mode = VMode {
            vsync,
            last,
            jump,
            vblank_set,
            active_last: vblank_set - 1,
            interlaced_last: interlaced_last.unwrap_or(last),
            vsync_lines: Default::default(),
        };
        for key in 0..4 {
            let (interlace, odd) = (key >= 2, key % 2 == 1);
            let mut short = vec![false; 0x200];
            let mut long = vec![false; 0x200];
            if mode.vsync.is_some() {
                let mut sequence = Vec::new();
                let mut v = mode.vblank_set;
                while sequence.len() < 400 && v != mode.jump + 6 {
                    sequence.push(v);
                    v = mode.next(v, interlace, odd);
                }
                let n = sequence.len();
                for &v in &sequence[n - VSYNC_LINES..] {
                    short[v as usize] = true;
                }
                let long_end: i64 = if interlace && odd { -2 } else { -1 };
                let stop = (n as i64 + long_end + 1) as usize;
                for &v in &sequence[stop - VSYNC_LINES..stop] {
                    long[v as usize] = true;
                }
            }
            mode.vsync_lines[key] = (short, long);
        }
        mode
    }

    /// The H40 line table of line `v` in a field.
    pub fn h40_line(&self, v: i64, interlace: bool, odd: bool) -> &'static HMode {
        let (short, long) = &self.vsync_lines[usize::from(interlace) * 2 + usize::from(odd)];
        let t = tables();
        match (short[v as usize & 0x1FF], long[v as usize & 0x1FF]) {
            (false, false) => &t.h40,
            (true, true) => &t.h40_vsync,
            (false, true) => &t.h40_long,
            (true, false) => &t.h40_short,
        }
    }

    pub fn next(&self, v: i64, interlace: bool, odd: bool) -> i64 {
        if v == if interlace {
            self.interlaced_last
        } else {
            self.last
        } {
            if self.jump > 0x1FF {
                return 0;
            }
            return if interlace && odd {
                self.jump - 1
            } else {
                self.jump
            };
        }
        if v == 0x1FF {
            0
        } else {
            v + 1
        }
    }
}

/// The line and frame tables, built once.
pub struct Tables {
    pub h32: HMode,
    pub h40: HMode,
    pub h40_vsync: HMode,
    /// 3,450 MCLK.
    pub h40_long: HMode,
    /// 3,390 MCLK.
    pub h40_short: HMode,
    pub v28_ntsc: VMode,
    pub v30_ntsc: VMode,
    pub v28_pal: VMode,
    pub v30_pal: VMode,
}

pub fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut modes = [
            HMode::new(HName::H32, 0, 0x127, 0x1D2, 0x10A, 0x126, 0x00A, |_| 10),
            HMode::new(HName::H40, 1, 0x16C, 0x1C9, 0x14A, 0x166, 0x00B, h40_sync),
            HMode::new(
                HName::H40,
                2,
                0x16C,
                0x1C9,
                0x14A,
                0x166,
                0x00B,
                h40_pixels(true, true),
            ),
            HMode::new(
                HName::H40,
                3,
                0x16C,
                0x1C9,
                0x14A,
                0x166,
                0x00B,
                h40_pixels(false, true),
            ),
            HMode::new(
                HName::H40,
                4,
                0x16C,
                0x1C9,
                0x14A,
                0x166,
                0x00B,
                h40_pixels(true, false),
            ),
        ];
        for mode in &mut modes {
            let active: &[u16] = match mode.name {
                HName::H32 => &ACTIVE_SLOTS_H32,
                HName::H40 => &ACTIVE_SLOTS_H40,
            };
            let mut slots: Vec<i64> = active.iter().map(|&h| mode.at(h)).collect();
            slots.sort_unstable();
            mode.slots_active = slots;
            mode.slots_idle = idle_slots(mode);
            mode.slots_first_idle = if mode.name == HName::H40 {
                let first = mode.at(FIRST_IDLE_SLOT_H40);
                mode.slots_idle
                    .iter()
                    .copied()
                    .filter(|&o| o >= first)
                    .collect()
            } else {
                Vec::new()
            };
            mode.dma_ticks = dma_ticks(mode);
            mode.no_read_fill = if mode.name == HName::H40 {
                NO_READ_FILL_H40.iter().map(|&h| mode.at(h)).collect()
            } else {
                Vec::new()
            };
            mode.display_at = mode.hvals.iter().position(|&h| h == 0x000).expect("H 0");
            if mode.name == HName::H40 {
                mode.dot_edges = dot_clock_edges(mode);
            }
        }
        let [h32, h40, h40_vsync, h40_long, h40_short] = modes;
        Tables {
            h32,
            h40,
            h40_vsync,
            h40_long,
            h40_short,
            v28_ntsc: VMode::new(0x0EA, 0x1E5, 0x0E0, Some(0x0E8), None),
            v30_ntsc: VMode::new(0x1FF, 0x200, 0x0F0, None, None),
            v28_pal: VMode::new(0x102, 0x1CA, 0x0E0, Some(0x100), Some(0x101)),
            v30_pal: VMode::new(0x10A, 0x1D2, 0x0F0, Some(0x108), Some(0x109)),
        }
    })
}

/// Whether VINT fires on a frame whose vertical border never started (the V28/V30
/// trick). Not measured: an experiment switch while it is decided.
pub const VINT_WITHOUT_BORDER: bool = false;
/// The border tested against the live V28/V30 mode; false puts back the board's
/// earlier rule.
pub const LIVE_BORDER: bool = true;

/// Write targets (code bits 3-0) and read targets (bits 4-0).
pub const WRITE_VRAM: u8 = 0x01;
pub const WRITE_CRAM: u8 = 0x03;
pub const WRITE_VSRAM: u8 = 0x05;
pub const READ_VRAM: u8 = 0x00;
pub const READ_VSRAM: u8 = 0x04;
pub const READ_CRAM: u8 = 0x08;
pub const READ_VRAM8: u8 = 0x0C;

/// MCLK a DMA transfer takes to read one word from the 68000's bus (Exodus
/// `dmaTransferReadTimeInMclkCycles`).
pub const DMA_READ_MCLK: i64 = 16;

/// The data port's read-ahead, as the die model times it: a read that waits for the
/// buffer is let go this long after the active-line slot that fills it, counted as
/// [`READ_RELEASE_HALF_PIXELS`] half pixel clocks plus the rest in master clocks
/// (Nuked-MD; docs/worklog.md, "The read release in half pixels").
pub const READ_RELEASE_MCLK: i64 = 53;
pub const READ_RELEASE_HALF_PIXELS: i64 = 11;
/// On a blank line the die lets a waiting read go this long after the slot.
pub const READ_RELEASE_IDLE_MCLK: i64 = 26;
/// A read that starts in this window after the end-of-line slot at H $140 fills the
/// buffer is answered at once (Nuked-MD; docs/worklog.md, "The slot at $140").
pub const READ_EARLY_SLOT_H: u16 = 0x140;
pub const READ_EARLY_MCLK: (i64, i64) = (43, 47);
pub const READ_FILL_LEAD_MCLK: i64 = 21;
/// A 68000 bus cycle without wait states: 4 clocks.
pub const WRITE_CYCLE_MCLK: i64 = 28;
/// A 68000-memory transfer's fixed start-up, before its first word is read: Kabuto's
/// hardware-measured 5.6 cycles, 39 MCLK (the hardware source is the judge where it
/// and Nuked-MD's 8.0 disagree).
pub const DMA_STARTUP_MCLK: i64 = 39;
/// The die's figures (`Machine::dma_figures` "die"): the first read two pixel clocks'
/// lengths after the trigger write is over, less 7 MCLK; the 68000's next cycle on its
/// first clock edge 34 MCLK after the last read.
pub const DMA_FIRST_READ_DIE_MCLK: i64 = -7;
pub const DMA_FIRST_READ_DIE_PIXELS: i64 = 2;
pub const DMA_RESUME_DIE_MCLK: i64 = 34;
/// The 68000's write that starts a 68000-to-VDP transfer is itself held 3 clocks
/// longer (Nuked-MD, all 119 in the Overdrive 2 boot and logo traces).
pub const DMA_TRIGGER_HOLD_MCLK: i64 = 21;
/// With the die's figures, a transfer reads only into room in the FIFO.
pub const DMA_READS_INTO_ROOM: bool = true;
/// The refresh timer stops while the VDP holds the bus for a transfer
/// (docs/worklog.md, "The refresh timer stops for a transfer"), from
/// [`RAM_REFRESH_PAUSE_FROM_MCLK`] after the trigger's /AS to
/// [`RAM_REFRESH_PAUSE_TAIL_MCLK`] after the last read; a self-run that began up to
/// [`RAM_REFRESH_PAUSE_BEGAN_MCLK`] after the pause starts still runs. Die-derived.
pub const RAM_REFRESH_PAUSES: bool = true;
pub const RAM_REFRESH_PAUSE_FROM_MCLK: i64 = 35;
pub const RAM_REFRESH_PAUSE_TAIL_MCLK: i64 = 9;
pub const RAM_REFRESH_PAUSE_BEGAN_MCLK: i64 = 14;
/// On an active line the die's strobe comes this long after the board's external slot,
/// so the read waits for it. Die-derived.
pub const DMA_ACTIVE_WRITE_LAG_MCLK: i64 = 48;
/// The transfer's own bus cycles take the cartridge refresh ticks that fall due while
/// it runs. Die-derived.
pub const DMA_TAKES_REFRESH: bool = true;
/// A transfer's FIFO entry for VRAM reaches it no sooner than this long after it
/// enters the FIFO (Nuked-MD's VRAM write strobes; docs/worklog.md, "The FIFO's
/// delay"). Die-derived.
pub const FIFO_COMMIT_DELAY_MCLK: i64 = 44;
/// A 68000-to-VDP transfer reads the 68000's memory at its own pace, into the FIFO
/// while it has room, not only at an access slot. Die-derived.
pub const DMA_READS_BETWEEN_SLOTS: bool = true;
/// A read of the HV counter (or the status register) sees the counters as they stand
/// at the first pixel-clock edge at or after this many MCLK into the bus cycle. Fitted
/// to Nemesis's captures.
pub const HV_SAMPLE_DELAY: i64 = 3;
/// With the die's figures the status's VBLANK flag clears this many pixels into line
/// $1FF, not as it begins. Die-derived.
pub const VBLANK_CLEAR_DIE_PIXELS: usize = 1;

/// One FIFO entry: [code, address, data, half written, pending].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FifoEntry {
    pub code: u8,
    pub address: u32,
    pub data: u16,
    pub half: bool,
    pub pending: bool,
}

/// Which of the line's slot lists the VDP is working through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slots {
    Active,
    FirstIdle,
    Idle,
}

/// The predicates `_advance` keeps going for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Until {
    Never,
    ReadAvailable,
    FifoNotFull,
    TransferDone,
}

/// The phase `sample_point` finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    None,
    Edge,
    Late,
    Next,
}

/// Everything of the VDP but its picture.
pub struct VdpState {
    pub machine: Machine,
    pub pal: bool,
    pub registers: [u8; 0x20],
    /// Byte-swapped within each word, as the chip.
    pub vram: Vec<u8>,
    pub cram: [u16; 64],
    pub vsram_words: usize,
    pub hv_unit: &'static HVUnit,
    pub vsram: [u16; 64],
    /// Planes A and B's full-screen vertical scroll as the line being drawn took it.
    pub vscroll_latch: [u16; 2],
    vscroll_before: [Option<u16>; 2],
    column_before: [Option<u16>; 40],
    // Command state (Exodus _commandCode, _commandAddress, _originalCommandAddress).
    pub code: u8,
    pub address: u32,
    pub original_address: u32,
    pub pending: bool,
    pub modified: bool,
    pub fifo: [FifoEntry; 4],
    /// When each entry may first be committed ([`FIFO_COMMIT_DELAY_MCLK`]).
    pub fifo_ready: [i64; 4],
    pub fifo_read: usize,
    pub fifo_write: usize,
    pub read_buffer: u16,
    pub read_available: bool,
    pub read_half: bool,
    pub read_ready_at: i64,
    pub read_fill_after: i64,
    pub read_early: (i64, i64),
    // DMA.
    pub fill_running: bool,
    pub transfer_active: bool,
    pub transfer_cached: bool,
    pub transfer_value: u16,
    pub transfer_next_read: i64,
    pub dma_die: bool,
    pub read_inc: i64,
    // Interrupts and flags.
    pub vint_pending: bool,
    pub hint_pending: bool,
    pub exint_pending: bool,
    pub f_flag: bool,
    pub odd: bool,
    pub hint_counter: i64,
    pub hv_latched: u16,
    pub frame: u64,
    pub z80_irq_start: i64,
    pub z80_irq_end: i64,
    /// The debug register $C0001C: only its plane-forcing bits 8-6 are modelled.
    pub debug_register: u16,
    /// Rendering can be turned off for speed where only the ports and counters matter.
    pub rendering: bool,
    /// MCLK the last 68000 port access held the 68000.
    pub hold: i64,
    pub hmode: &'static HMode,
    pub vmode: &'static VMode,
    pub interlace: bool,
    pub interlace_double: bool,
    pub v: i64,
    pub previous_v: i64,
    /// The vertical border (Kabuto, "V28/V30 tricks"; the reference explains).
    pub border: bool,
    border_started: bool,
    pub line_start: i64,
    pub mclk: i64,
    pub slots: Slots,
    pub slot_index: usize,
    vint_done: bool,
    odd_done: bool,
    pub line_end: i64,
    pub hblank_at: i64,
    pub vint_at: i64,
    pub odd_at: i64,
    pub render_at: i64,
}

impl VdpState {
    pub fn display_enabled(&self) -> bool {
        self.registers[1] & 0x40 != 0
    }

    pub fn mode5(&self) -> bool {
        self.registers[1] & 0x04 != 0
    }

    pub fn dma_enabled(&self) -> bool {
        self.registers[1] & 0x10 != 0
    }

    pub fn dmd(&self) -> u8 {
        self.registers[23] >> 6
    }

    pub fn slot_list(&self) -> &'static [i64] {
        match self.slots {
            Slots::Active => &self.hmode.slots_active,
            Slots::FirstIdle => &self.hmode.slots_first_idle,
            Slots::Idle => &self.hmode.slots_idle,
        }
    }

    /// What a 2-cell column's latch took before a later write (for the trace).
    pub fn column_before_at(&self, index: usize) -> Option<u16> {
        self.column_before[index]
    }

    /// What a plane's full-screen latch took before a later write (for the trace).
    pub fn vscroll_before_at(&self, plane: usize) -> Option<u16> {
        self.vscroll_before[plane]
    }

    pub fn border_started_now(&self) -> bool {
        self.border_started
    }

    pub fn vint_done_now(&self) -> bool {
        self.vint_done
    }

    pub fn odd_done_now(&self) -> bool {
        self.odd_done
    }

    /// The 2-cell column scroll word `index` as the line being drawn latched it.
    pub fn column_scroll(&self, index: usize) -> u16 {
        let before = if index < 40 {
            self.column_before[index]
        } else {
            None
        };
        before.unwrap_or(self.vsram[index])
    }

    fn fifo_full(&self) -> bool {
        self.fifo[self.fifo_read].pending && self.fifo[(self.fifo_read + 3) & 3].pending
    }

    /// The internal 9-bit H counter at the VDP's current time.
    pub fn hcounter(&self) -> u16 {
        self.hmode
            .hval(self.hmode.position(self.mclk - self.line_start))
    }

    fn hv_now(&self) -> u16 {
        hv_word(
            self.v,
            self.hcounter(),
            self.interlace,
            self.interlace_double,
        )
    }

    /// The line the vertical border starts on for the V28/V30 mode register 1 holds
    /// now ($E0 or $F0).
    fn blank_line(&self) -> i64 {
        if self.registers[1] & 0x08 != 0 {
            0x0F0
        } else {
            0x0E0
        }
    }

    /// VINT 6, HINT 4, EXINT 2: the highest enabled and pending (Exodus
    /// `GetNewIPLLineState`, Ports.cpp 428-455).
    pub fn ipl(&self) -> u8 {
        if self.vint_pending && self.registers[1] & 0x20 != 0 {
            return 6;
        }
        if self.hint_pending && self.registers[0] & 0x10 != 0 {
            return 4;
        }
        if self.exint_pending && self.registers[11] & 0x08 != 0 {
            return 2;
        }
        0
    }

    /// The Z80's INT, from the VINT point for one line: CMD (gen-hw.txt 2.4) gives the
    /// line; the length is an open question.
    pub fn z80_irq_at(&self, mclk: i64) -> bool {
        self.z80_irq_start <= mclk && mclk < self.z80_irq_end
    }

    fn work_pending(&self) -> bool {
        let cd5 = self.code & 0x20 != 0;
        let dmd = self.registers[23] >> 6;
        self.fifo[self.fifo_read].pending
            || (cd5 && (dmd < 2 || dmd == 3 || self.fill_running))
            || ((self.code & 0x13) == 0 && !self.read_available)
    }

    fn until(&self, until: Until) -> bool {
        match until {
            Until::Never => false,
            Until::ReadAvailable => self.read_available,
            Until::FifoNotFull => !self.fifo_full(),
            Until::TransferDone => !self.transfer_active,
        }
    }

    /// The master clock `pixels` pixel clocks after `t` (a pixel edge), by the line's
    /// own pixel lengths.
    fn pixels_after(&self, t: i64, pixels: i64) -> i64 {
        let mode = self.hmode;
        let mut start = self.line_start;
        while t - start >= mode.length {
            start += mode.length;
        }
        let index = bisect_right(&mode.starts, t - start) as i64 - 1 + pixels;
        let n = mode.starts.len() as i64;
        let (lines, index) = (index.div_euclid(n), index.rem_euclid(n));
        start + lines * mode.length + mode.starts[index as usize]
    }

    /// The first DMA tick after `t` (the current line's, else the next's).
    fn dma_tick_after(&self, t: i64) -> i64 {
        let mode = self.hmode;
        let mut start = self.line_start;
        while t - start >= mode.length {
            start += mode.length;
        }
        loop {
            let i = bisect_right(&mode.dma_ticks, t - start);
            if i < mode.dma_ticks.len() {
                return start + mode.dma_ticks[i];
            }
            start += mode.length;
        }
    }

    /// The pixel clock regime of pixel `index` of the line (`HVUnit`).
    fn regime(&self, index: i64) -> Regime {
        let mode = self.hmode;
        if mode.name == HName::H32 {
            return Regime::H32;
        }
        let index = wrap(mode.starts.len(), index);
        let end = mode.starts.get(index + 1).copied().unwrap_or(mode.length);
        if end - mode.starts[index] == 8 {
            Regime::H40
        } else {
            Regime::H40Long
        }
    }

    /// The 10-bit status register (the Manual; Exodus's status bits). Bits 6 and 5 are
    /// the renderer's, passed in.
    fn status(&self, overflow: bool, collision: bool) -> u16 {
        let h = self.hcounter();
        let v = self.v;
        let mut vblank = (self.border && v != 0x1FF) || !self.display_enabled();
        if v == 0x1FF && self.dma_die && self.border {
            // The die's flag clears a pixel into the line (VBLANK_CLEAR_DIE_PIXELS).
            vblank =
                vblank || self.mclk - self.line_start < self.hmode.starts[VBLANK_CLEAR_DIE_PIXELS];
        }
        let hblank = h >= self.hmode.hblank_set || h < self.hmode.hblank_clear;
        (if self.fifo[self.fifo_read].pending {
            0
        } else {
            0x200
        }) | (if self.fifo_full() { 0x100 } else { 0 })
            | (if self.f_flag { 0x80 } else { 0 })
            | (if overflow { 0x40 } else { 0 })
            | (if collision { 0x20 } else { 0 })
            | (if self.odd && self.interlace { 0x10 } else { 0 })
            | (if vblank { 0x08 } else { 0 })
            | (if hblank { 0x04 } else { 0 })
            | (if self.code & 0x20 != 0 { 0x02 } else { 0 })
            | (if self.pal { 0x01 } else { 0 })
    }

    /// For a 2-cell VSRAM write that lands before its column's latch: that column's
    /// first pixel, which the line may not be drawn past yet.
    fn column_limit(&self, code: u8, address: u32) -> Option<i64> {
        if (code & 0x0F) != WRITE_VSRAM || self.registers[11] & 0x04 == 0 {
            return None;
        }
        let index = ((address & 0x7E) >> 1) as u16;
        if index >= 40 {
            return None;
        }
        let mode = self.hmode;
        let (first, per_column) = vscroll_column_h(mode.name)[usize::from(index & 1)];
        let at = mode.offset_of((first + (index >> 1) * per_column) & 0x1FF)?;
        if self.mclk - self.line_start > at {
            return None;
        }
        Some(i64::from(index >> 1) * 16)
    }

    /// What a read of VSRAM past its end gives: the last word the renderer read from
    /// VSRAM, which follows the H counter, or where that index is past the end too,
    /// entries $26 AND $27 (Exodus `UpdateInternalState`, General.cpp 2114-2129).
    fn vsram_last_render(&self) -> u16 {
        let mut index = usize::from((self.hcounter() >> 2) & 0x7E);
        if self.registers[11] & 0x04 == 0 {
            index &= 0x03;
        }
        if index < 0x50 {
            return self.vsram[index >> 1];
        }
        self.vsram[0x26] & self.vsram[0x27]
    }
}

/// The VDP: its state and its picture.
pub struct Vdp {
    pub s: VdpState,
    pub renderer: Renderer,
}

impl Vdp {
    pub fn new(machine: Machine) -> Self {
        let t = tables();
        let mut vdp = Vdp {
            s: VdpState {
                machine,
                pal: machine.pal,
                registers: [0; 0x20],
                vram: vec![0; 0x10000],
                cram: [0; 64],
                vsram_words: machine.vdp.vsram_bytes / 2,
                hv_unit: hv_unit(machine.hv_reads()),
                vsram: [0; 64],
                vscroll_latch: [0, 0],
                vscroll_before: [None, None],
                column_before: [None; 40],
                code: 0,
                address: 0,
                original_address: 0,
                pending: false,
                modified: true,
                fifo: [FifoEntry::default(); 4],
                fifo_ready: [0; 4],
                fifo_read: 0,
                fifo_write: 0,
                read_buffer: 0,
                read_available: false,
                read_half: false,
                read_ready_at: 0,
                read_fill_after: 0,
                read_early: (0, 0),
                fill_running: false,
                transfer_active: false,
                transfer_cached: false,
                transfer_value: 0,
                transfer_next_read: 0,
                dma_die: machine.dma_figures == "die",
                read_inc: DMA_READ_MCLK,
                vint_pending: false,
                hint_pending: false,
                exint_pending: false,
                f_flag: false,
                odd: false,
                hint_counter: 0,
                hv_latched: 0,
                frame: 0,
                z80_irq_start: -1,
                z80_irq_end: -1,
                debug_register: 0,
                rendering: true,
                hold: 0,
                hmode: &t.h32,
                vmode: if machine.pal { &t.v28_pal } else { &t.v28_ntsc },
                interlace: false,
                interlace_double: false,
                v: 0x1FF,
                previous_v: 0x1FE,
                border: false,
                border_started: false,
                line_start: 0,
                mclk: 0,
                slots: Slots::Idle,
                slot_index: 0,
                vint_done: false,
                odd_done: false,
                line_end: 0,
                hblank_at: 0,
                vint_at: 0,
                odd_at: 0,
                render_at: 0,
            },
            renderer: Renderer::new(),
        };
        vdp.line_events();
        vdp
    }

    // -- registers ------------------------------------------------------------------------

    fn write_register(&mut self, number: usize, value: u8) {
        // A register write reaches the VDP at once, not through the FIFO, so it too
        // lands part way along the line being drawn.
        if self.renderer.pending {
            // Tiles already fetched keep the old state; the output, behind the H
            // counter, takes the new one from where it has got to.
            let offset = self.s.mclk - self.s.line_start;
            self.renderer
                .catch_up(&self.s, offset, PLANE_REGISTERS.contains(&number), None);
        }
        let s = &mut self.s;
        let old = s.registers[number];
        s.registers[number] = value;
        if number == 0 && (value ^ old) & 0x02 != 0 && value & 0x02 != 0 {
            // 0 -> 1 on the HV latch bit latches the counter (Exodus Ports.cpp 1858-1871).
            s.hv_latched = s.hv_now();
        } else if number == 1 && (value ^ old) & 0x48 != 0 {
            if LIVE_BORDER && (value ^ old) & 0x08 != 0 && !s.border && s.v == s.blank_line() {
                // The mode that blanks from this line came in part way along it.
                s.border = true;
                s.border_started = true;
            }
            self.choose_slots();
        }
    }

    // -- time --------------------------------------------------------------------------------

    /// The slot list for the current line, and where in it the VDP is.
    fn choose_slots(&mut self) {
        let s = &mut self.s;
        let active = s.display_enabled() && (!s.border || s.v == 0x1FF);
        s.slots = if active {
            Slots::Active
        } else if s.display_enabled()
            && s.v == s.blank_line()
            && !s.hmode.slots_first_idle.is_empty()
        {
            Slots::FirstIdle
        } else {
            Slots::Idle
        };
        s.slot_index = bisect_right(s.slot_list(), s.mclk - s.line_start - 1);
    }

    fn line_events(&mut self) {
        let s = &mut self.s;
        let mode = s.hmode;
        let start = s.line_start;
        s.line_end = start + mode.length;
        s.hblank_at = start + mode.at(mode.hblank_set);
        let vint_line = if VINT_WITHOUT_BORDER {
            s.v == s.vmode.vblank_set
        } else {
            s.border_started
        };
        s.vint_at = if vint_line {
            start + mode.at(0x001)
        } else {
            -1
        };
        s.odd_at = if vint_line {
            start + mode.at(0x002)
        } else {
            -1
        };
        // The picture of a line is drawn where its active display begins (the HBlank
        // flag clearing), so an H interrupt handler's register writes are in place.
        s.render_at = if s.rendering && !s.border && s.v < 0x0F0 {
            start + mode.at(mode.hblank_clear)
        } else {
            -1
        };
        self.choose_slots();
    }

    /// Whether the dot clock changes at `mclk` ([`DOT_CLOCK_SLOW_WINDOWS`]), on the
    /// die-figures machine in H40; false otherwise.
    pub fn dot_clock_edge(&mut self, host: &mut impl VdpHost, mclk: i64) -> bool {
        if !self.s.dma_die {
            return false;
        }
        self.sync(host, mclk);
        let mode = self.s.hmode;
        if mode.name != HName::H40 {
            return false;
        }
        let offset = mclk - self.s.line_start - DOT_CLOCK_LINE_LAG_MCLK;
        let offset = if offset < 0 {
            offset.rem_euclid(mode.length)
        } else {
            offset
        };
        offset < mode.length && mode.dot_edges[offset as usize]
    }

    /// Bring the VDP to `mclk`: slot work, then line events on the way.
    pub fn sync(&mut self, host: &mut impl VdpHost, mclk: i64) {
        if mclk > self.s.mclk {
            self.advance(host, mclk, Until::Never);
        }
    }

    /// Run to `target`; with `until`, keep going past it slot by slot until the
    /// predicate holds (a port access held for the VDP).
    fn advance(&mut self, host: &mut impl VdpHost, target: i64, until: Until) {
        if until != Until::Never && self.s.until(until) {
            if self.s.mclk < target {
                self.advance(host, target, Until::Never);
            }
            return;
        }
        loop {
            let s = &mut self.s;
            let mut next_event = s.line_end;
            // 0 line, 1 hblank, 2 vint, 3 odd, 4 slot, 5 render, 6 DMA read
            let mut kind = 0;
            if s.mclk <= s.hblank_at && s.hblank_at < next_event {
                next_event = s.hblank_at;
                kind = 1;
            }
            if !s.vint_done && s.mclk <= s.vint_at && s.vint_at < next_event {
                next_event = s.vint_at;
                kind = 2;
            }
            if !s.odd_done && s.mclk <= s.odd_at && s.odd_at < next_event {
                next_event = s.odd_at;
                kind = 3;
            }
            if s.mclk <= s.render_at && s.render_at < next_event {
                next_event = s.render_at;
                kind = 5;
            }
            if let Some(read_at) = self.transfer_read_due() {
                if read_at < next_event {
                    next_event = read_at.max(self.s.mclk);
                    kind = 6;
                }
            }
            let s = &mut self.s;
            let slots = s.slot_list();
            if s.work_pending() && s.slot_index < slots.len() {
                let slot_at = s.line_start + slots[s.slot_index];
                if slot_at < s.mclk {
                    s.slot_index = bisect_right(slots, s.mclk - s.line_start - 1);
                    continue;
                }
                if slot_at <= next_event {
                    next_event = slot_at;
                    kind = 4;
                }
            }
            if until == Until::Never && next_event > target {
                s.mclk = s.mclk.max(target);
                return;
            }
            s.mclk = next_event;
            match kind {
                4 => {
                    s.slot_index += 1;
                    self.slot(host);
                    if until != Until::Never && self.s.until(until) {
                        if self.s.mclk < target {
                            self.advance(host, target, Until::Never);
                        }
                        return;
                    }
                }
                0 => self.next_line(),
                1 => {
                    s.hblank_at = -1;
                    self.latch_hmode();
                }
                2 => {
                    s.vint_done = true;
                    s.vint_pending = true;
                    s.f_flag = true;
                    s.z80_irq_start = s.mclk;
                    s.z80_irq_end = s.mclk + MCLK_PER_LINE;
                }
                5 => {
                    s.render_at = -1;
                    let v = s.v;
                    self.renderer.begin_line(&self.s, v);
                }
                6 => {
                    self.transfer_reads(host);
                    if until != Until::Never && self.s.until(until) {
                        if self.s.mclk < target {
                            self.advance(host, target, Until::Never);
                        }
                        return;
                    }
                }
                _ => {
                    s.odd_done = true;
                    if s.interlace {
                        s.odd = !s.odd;
                    }
                }
            }
        }
    }

    fn next_line(&mut self) {
        // The line is over: whatever is left of its picture is drawn from the state it
        // ends with, before the next line can change anything.
        if self.renderer.pending {
            self.renderer.finish_line(&self.s);
        }
        let s = &mut self.s;
        // The vertical scroll latches fell in the last few pixels of this line: they
        // took each word as it stood then, which is its value now unless a write landed
        // after the latch point (`vsram_store`).
        for plane in 0..2 {
            s.vscroll_latch[plane] = s.vscroll_before[plane].unwrap_or(s.vsram[plane]);
            s.vscroll_before[plane] = None;
        }
        // Every 2-cell column is latched afresh in the line now starting.
        s.column_before = [None; 40];
        let mut vmode = s.vmode;
        let new_v = vmode.next(s.v, s.interlace, s.odd);
        s.previous_v = s.v;
        if new_v == vmode.vblank_set {
            self.latch_vmode();
            vmode = self.s.vmode;
        }
        let s = &mut self.s;
        let mut was_border = s.border;
        if new_v == 0 {
            s.border = false;
            was_border = false; // line 0 counts the H interrupt down, as every line did
        }
        s.border_started = false;
        if !LIVE_BORDER {
            s.border = new_v >= vmode.vblank_set;
            s.border_started = new_v == vmode.vblank_set;
            was_border = new_v > vmode.vblank_set;
        } else if !s.border && new_v == s.blank_line() {
            s.border = true;
            s.border_started = true;
        }
        s.v = new_v;
        if new_v == 0 {
            s.frame += 1;
            self.renderer.start_frame(&self.s);
        }
        let s = &mut self.s;
        if s.hmode.name == HName::H40 {
            s.hmode = vmode.h40_line(new_v, s.interlace, s.odd);
        }
        // The HINT counter at the V increment point (Exodus General.cpp 2268-2289).
        if was_border {
            s.hint_counter = i64::from(s.registers[10]);
        } else if s.hint_counter == 0 {
            s.hint_counter = i64::from(s.registers[10]);
            s.hint_pending = true;
        } else {
            s.hint_counter -= 1;
        }
        s.line_start = s.line_end;
        s.vint_done = false;
        s.odd_done = false;
        self.line_events();
    }

    /// RS0/RS1 changes take effect at HBlank (Exodus General.cpp 1917-1935): the H
    /// counter keeps its value and counts on in the new mode's sequence.
    fn latch_hmode(&mut self) {
        let s = &mut self.s;
        let new: &'static HMode = if s.registers[12] & 0x81 == 0x81 {
            s.vmode.h40_line(s.v, s.interlace, s.odd)
        } else {
            &tables().h32
        };
        if new.name == s.hmode.name {
            return;
        }
        let h = s.hmode.hblank_set;
        s.hmode = new;
        s.line_start = s.mclk - new.offset_of(h).unwrap_or_else(|| new.at(new.hblank_set));
        self.line_events();
        self.s.hblank_at = -1;
    }

    /// V28/V30 and interlace changes latch at VBlank (Exodus General.cpp 2203-2215).
    fn latch_vmode(&mut self) {
        let s = &mut self.s;
        let t = tables();
        let v30 = s.registers[1] & 0x08 != 0;
        s.vmode = match (s.pal, v30) {
            (true, true) => &t.v30_pal,
            (true, false) => &t.v28_pal,
            (false, true) => &t.v30_ntsc,
            (false, false) => &t.v28_ntsc,
        };
        let lsm = (s.registers[12] >> 1) & 3;
        s.interlace = lsm & 1 != 0;
        s.interlace_double = lsm == 3;
    }

    // -- the counters and the status register ---------------------------------------------

    /// Bring the VDP to where a port read at `mclk` samples its counters: the first
    /// pixel-clock edge at or after `mclk` + HV_SAMPLE_DELAY.
    fn sample_point(&mut self, host: &mut impl VdpHost, mclk: i64) -> Phase {
        self.sync(host, mclk + HV_SAMPLE_DELAY);
        let offset = self.s.mclk - self.s.line_start;
        let edge = self.s.hmode.next_edge(offset);
        let at = self.s.line_start + edge;
        self.sync(host, at);
        if edge == offset {
            return Phase::Next;
        }
        for (back, phase) in [(1, Phase::Edge), (2, Phase::Late)] {
            let before = offset - back;
            if before >= 0 && self.s.hmode.next_edge(before) == before {
                return phase;
            }
        }
        Phase::None
    }

    /// The HV counter, as the machine's measured unit reads it at the phase
    /// `sample_point` finds.
    pub fn read_hv(&mut self, host: &mut impl VdpHost, mclk: i64) -> u16 {
        let phase = self.sample_point(host, mclk);
        let s = &self.s;
        if s.registers[0] & 0x02 != 0 {
            return s.hv_latched;
        }
        let mode = s.hmode;
        let index = mode.position(s.mclk - s.line_start);
        let (mut h, mut v) = (mode.hval(index), s.v);
        let n = mode.hvals.len() as i64;
        match phase {
            Phase::Edge => {
                let regime = s.regime(if index != 0 { index - 1 } else { n - 1 });
                let old_v = if index == 0 { s.previous_v } else { v };
                h = s.hv_unit.h_at_edge(regime, mode.hval(index - 1), h);
                v = s.hv_unit.v_at_edge(regime, old_v, v);
            }
            Phase::Late => {
                let regime = s.regime(if index != 0 { index - 1 } else { n - 1 });
                h = s.hv_unit.h_late(regime, mode.hval(index - 1), h);
            }
            Phase::Next => {
                let following = mode.hval((index + 1).rem_euclid(n));
                h = s.hv_unit.h_at_next(s.regime(index), h, following);
            }
            Phase::None => {}
        }
        hv_word(v, h, s.interlace, s.interlace_double)
    }

    /// The acknowledge clears the request it answers, and the F flag with VINT's
    /// (Exodus Ports.cpp 118-133).
    pub fn acknowledge(&mut self, _level: u8) {
        let s = &mut self.s;
        if s.vint_pending && s.registers[1] & 0x20 != 0 {
            s.vint_pending = false;
            s.f_flag = false;
        } else if s.hint_pending && s.registers[0] & 0x10 != 0 {
            s.hint_pending = false;
        } else if s.exint_pending && s.registers[11] & 0x08 != 0 {
            s.exint_pending = false;
        }
    }

    // -- the FIFO and the work at a slot --------------------------------------------------

    /// A 68000-to-VDP transfer's reads due by now, each into the FIFO if it has room.
    fn transfer_reads(&mut self, host: &mut impl VdpHost) {
        loop {
            let s = &self.s;
            let going = s.code & 0x20 != 0
                && s.registers[23] >> 6 < 2
                && s.transfer_active
                && (!s.transfer_cached || !s.fifo_full())
                && !(DMA_READS_INTO_ROOM && s.dma_die && s.fifo_full())
                && s.transfer_next_read + s.read_inc <= s.mclk;
            if !going {
                break;
            }
            if !self.s.transfer_cached {
                self.transfer_read(host);
            }
            self.next_read();
            if !self.s.fifo_full() {
                self.transfer_push();
                self.dma_step();
            }
        }
    }

    /// When the transfer's next read can happen between slots, if one can.
    fn transfer_read_due(&self) -> Option<i64> {
        let s = &self.s;
        if DMA_READS_BETWEEN_SLOTS
            && s.transfer_active
            && s.code & 0x20 != 0
            && s.registers[23] >> 6 < 2
            && !s.fifo_full()
        {
            return Some(s.transfer_next_read + s.read_inc);
        }
        None
    }

    /// The transfer's read pace: every [`DMA_READ_MCLK`], or with the die's figures at
    /// the line's next DMA tick.
    fn next_read(&mut self) {
        let s = &mut self.s;
        s.transfer_next_read += s.read_inc;
        if s.dma_die {
            s.read_inc = s.dma_tick_after(s.transfer_next_read) - s.transfer_next_read;
        }
    }

    /// One access slot (Exodus `UpdateInternalState`, General.cpp 1773-1878). Anything
    /// written here lands part way along the line being drawn, so the renderer is
    /// brought up to this slot's pixel first.
    fn slot(&mut self, host: &mut impl VdpHost) {
        if self.renderer.pending {
            let s = &self.s;
            let entry = s.fifo[s.fifo_read];
            let code = if entry.pending { entry.code } else { s.code };
            let address = if entry.pending {
                entry.address
            } else {
                s.address
            };
            let names = (code & 0x0F) == WRITE_VRAM && self.in_name_table(address);
            let limit = s.column_limit(code, address);
            let offset = s.mclk - s.line_start;
            self.renderer.catch_up(&self.s, offset, names, limit);
        }
        let dmd = self.s.registers[23] >> 6;
        self.transfer_reads(host);
        let s = &self.s;
        if s.code & 0x20 != 0 && dmd == 2 && s.fill_running && !s.fifo[s.fifo_read].pending {
            self.fill();
            self.dma_step();
        }
        if self.s.code & 0x20 != 0 && dmd == 3 {
            self.copy();
            self.dma_step();
        }
        let mut did_read = false;
        let s = &self.s;
        if !s.fifo[s.fifo_read].pending
            && !s.read_available
            && (s.code & 0x13) == 0
            && s.mclk >= s.read_fill_after
            && !self.no_read_fill()
        {
            self.read_cache();
            did_read = true;
            let s = &mut self.s;
            if s.read_available {
                if s.slots == Slots::Active {
                    // Counted in half pixel clocks, which run slower around the sync.
                    let half = READ_RELEASE_HALF_PIXELS;
                    let extra = READ_RELEASE_MCLK - 4 * half;
                    let mut after = s.pixels_after(s.mclk, half / 2);
                    if half % 2 != 0 {
                        // half of the next pixel
                        after += (s.pixels_after(after, 1) - after).div_euclid(2);
                    }
                    s.read_ready_at = after + extra;
                    let early = s.hmode.offset_of(READ_EARLY_SLOT_H);
                    if Some(s.mclk - s.line_start) == early {
                        let (lo, hi) = READ_EARLY_MCLK;
                        s.read_early = (s.mclk + lo, s.mclk + hi);
                    } else {
                        s.read_early = (0, 0);
                    }
                } else {
                    s.read_ready_at = s.mclk + READ_RELEASE_IDLE_MCLK;
                    s.read_early = (0, 0);
                }
            }
        }
        let s = &self.s;
        if s.fifo[s.fifo_read].pending && !did_read && s.mclk >= s.fifo_ready[s.fifo_read] {
            self.fifo_write();
        }
        let s = &self.s;
        if s.code & 0x20 != 0 && dmd < 2 && s.transfer_cached && !s.fifo_full() {
            self.transfer_push();
            self.dma_step();
        }
    }

    /// This slot fills no data-port read: H40's $142 in an active line.
    fn no_read_fill(&self) -> bool {
        let s = &self.s;
        s.slots == Slots::Active && s.hmode.no_read_fill.contains(&(s.mclk - s.line_start))
    }

    fn vram_write_byte(&mut self, address: u32, value: u8) {
        // VRAM holds each word byte-swapped, so a byte access reaches the other byte of
        // the word (Exodus M5WriteVRAM8Bit, General.cpp 3104-3140).
        self.s.vram[((address ^ 1) & 0xFFFF) as usize] = value;
        // The sprite cache is written through by every VRAM write that lands in the
        // sprite table, over the whole 128 KB address range (Kabuto).
        self.renderer
            .sprite_cache_write(&self.s, (address ^ 1) & 0x1FFFF, value);
    }

    /// One VRAM write in 128 KB mode (register 1 bit 7), on a console with the usual
    /// single 64 KB chip: only the lower byte of each word is stored, at Kabuto's
    /// rearranged address ("128 KB mode abuse"); odd addresses have no effect.
    fn vram_write_128k(&mut self, address: u32, data: u16) {
        if address & 1 != 0 {
            return;
        }
        let target = (((address & 2) >> 1) ^ 1)
            | ((address & 0x400) >> 9)
            | (address & 0x3FC)
            | ((address & 0x1F800) >> 1);
        self.s.vram[(target & 0xFFFF) as usize] = data as u8;
        self.renderer
            .sprite_cache_write(&self.s, target & 0x1FFFF, data as u8);
    }

    /// Commit the oldest FIFO entry, a byte at a time for VRAM (Exodus
    /// `PerformFIFOWriteOperation`, General.cpp 2479-2627).
    fn fifo_write(&mut self) {
        let index = self.s.fifo_read;
        let FifoEntry {
            code,
            address,
            data,
            half,
            ..
        } = self.s.fifo[index];
        let target = code & 0x0F;
        if target == WRITE_VRAM {
            if self.s.registers[1] & 0x80 != 0 {
                self.vram_write_128k(address, data);
                self.s.fifo[index].pending = false;
            } else {
                // The low byte first, then the high (Nuked-MD's own VRAM writes).
                let byte = if ((address & 1) ^ u32::from(half)) != 0 {
                    (data >> 8) as u8
                } else {
                    data as u8
                };
                self.vram_write_byte((address & (ADDRESS_MASK - 1)) | u32::from(half), byte);
                self.s.fifo[index].half = !half;
                self.s.fifo[index].pending = !half;
            }
        } else if target == WRITE_CRAM {
            self.s.cram[((address & 0x7E) >> 1) as usize] = data & 0x0EEE;
            self.renderer.cram_dirty = true;
            self.renderer.cram_dot(data & 0x0EEE);
            self.s.fifo[index].pending = false;
        } else if target == WRITE_VSRAM {
            let vsram_index = ((address & 0x7E) >> 1) as usize;
            if vsram_index < self.s.vsram_words {
                self.vsram_store(vsram_index, data & 0x07FF);
            }
            self.s.fifo[index].pending = false;
        } else {
            self.s.fifo[index].pending = false;
        }
        let s = &mut self.s;
        if !s.fifo[index].pending {
            s.fifo_read = (s.fifo_read + 1) & 3;
            let dmd = s.registers[23] >> 6;
            if s.code & 0x20 != 0 && dmd == 2 {
                s.fill_running = true;
            }
            if s.code & 0x20 != 0 && dmd < 2 && s.transfer_cached {
                s.transfer_next_read = s.mclk;
                if s.dma_die {
                    s.read_inc = s.dma_tick_after(s.mclk) - s.mclk;
                }
            } else if DMA_READS_INTO_ROOM
                && s.dma_die
                && s.code & 0x20 != 0
                && dmd < 2
                && s.transfer_active
                && s.transfer_next_read + s.read_inc <= s.mclk
            {
                // The read that waited for room follows the VRAM write that made it.
                let lag = if s.slots == Slots::Active {
                    DMA_ACTIVE_WRITE_LAG_MCLK
                } else {
                    0
                };
                let due = s.dma_tick_after(s.mclk + lag - 1);
                s.transfer_next_read = s.mclk;
                s.read_inc = due - s.mclk;
            }
        }
    }

    /// Store one VSRAM word, keeping what the vertical scroll latches took if the write
    /// lands after the latch point of its plane or, in 2-cell column mode, of its
    /// column.
    fn vsram_store(&mut self, index: usize, value: u16) {
        let s = &mut self.s;
        let offset = s.mclk - s.line_start;
        let mode = s.hmode;
        if index < 2 && s.vscroll_before[index].is_none() {
            let latch_h = vscroll_latch_h(mode.name)[index];
            if offset > mode.at(latch_h) {
                s.vscroll_before[index] = Some(s.vsram[index]);
            }
        }
        if index < 40 && s.column_before[index].is_none() {
            let (first, per_column) = vscroll_column_h(mode.name)[index & 1];
            let latch_h = (first + (index as u16 >> 1) * per_column) & 0x1FF;
            if let Some(at) = mode.offset_of(latch_h) {
                if offset > at {
                    s.column_before[index] = Some(s.vsram[index]);
                }
            }
        }
        s.vsram[index] = value;
    }

    /// Whether a VRAM byte address lies in plane A's, plane B's or the window's name
    /// table, whose entries are fetched ahead of the patterns they name.
    fn in_name_table(&self, address: u32) -> bool {
        let registers = &self.s.registers;
        let size = registers[16];
        let cells_of = [32u32, 64, 32, 128];
        let cells = cells_of[usize::from(size & 3)] * cells_of[usize::from((size >> 4) & 3)];
        let address = address & 0xFFFF;
        let within = |base: u32, length: u32| base <= address && address < base + length;
        // The window's table only where the line under way shows the window.
        within((u32::from(registers[2]) << 10) & 0xE000, cells * 2)
            || within((u32::from(registers[4]) << 13) & 0xE000, cells * 2)
            || (self.renderer.shows_window()
                && within((u32::from(registers[3]) << 10) & 0xF800, 64 * 32 * 2))
    }

    /// Fill the read buffer (Exodus `PerformReadCacheOperation`, General.cpp
    /// 2345-2476): bits CRAM and VSRAM do not drive keep the next FIFO entry's.
    fn read_cache(&mut self) {
        let s = &mut self.s;
        if !s.read_half {
            s.read_buffer = s.fifo[s.fifo_read].data;
        }
        let target = s.code & 0x1F;
        let address = s.address;
        match target {
            READ_VRAM => {
                let word = (address & 0xFFFE) as usize;
                if !s.read_half {
                    s.read_buffer = (s.read_buffer & 0xFF) | u16::from(s.vram[word]) << 8;
                    s.read_half = true;
                    return;
                }
                s.read_buffer = (s.read_buffer & 0xFF00) | u16::from(s.vram[word | 1]);
                s.read_half = false;
                s.read_available = true;
            }
            READ_VSRAM => {
                let index = ((address & 0x7E) >> 1) as usize;
                let value = if index < s.vsram_words {
                    s.vsram[index]
                } else {
                    s.vsram_last_render()
                };
                s.read_buffer = (s.read_buffer & 0xF800) | (value & 0x07FF);
                s.read_available = true;
            }
            READ_CRAM => {
                let value = s.cram[((address & 0x7E) >> 1) as usize];
                s.read_buffer = (s.read_buffer & 0xF111) | (value & 0x0EEE);
                s.read_available = true;
            }
            READ_VRAM8 => {
                s.read_buffer =
                    (s.read_buffer & 0xFF00) | u16::from(s.vram[((address ^ 1) & 0xFFFF) as usize]);
                s.read_available = true;
            }
            _ => return,
        }
        s.address = (s.address + u32::from(s.registers[15])) & ADDRESS_MASK;
    }

    /// One DMA fill write (Exodus `PerformDMAFillOperation`, General.cpp 2680-2825):
    /// VRAM takes the upper byte of the triggering write; CRAM and VSRAM take the word
    /// in the next FIFO entry.
    fn fill(&mut self) {
        let last_index = (self.s.fifo_read + 3) & 3;
        let increment = u32::from(self.s.registers[15]);
        let last = &mut self.s.fifo[last_index];
        last.address = (last.address + increment) & ADDRESS_MASK;
        let (last_address, last_data) = (last.address, last.data);
        let next_data = self.s.fifo[self.s.fifo_read].data;
        let target = self.s.code & 0x0F;
        if target == WRITE_VRAM {
            self.vram_write_byte(last_address, (last_data >> 8) as u8);
        } else if target == WRITE_CRAM {
            self.s.cram[((last_address & 0x7E) >> 1) as usize] = next_data & 0x0EEE;
            self.renderer.cram_dirty = true;
            self.renderer.cram_dot(next_data & 0x0EEE);
        } else if target == WRITE_VSRAM {
            let index = ((last_address & 0x7E) >> 1) as usize;
            if index < self.s.vsram_words {
                self.vsram_store(index, next_data & 0x07FF);
            }
        }
        self.s.address = (self.s.address + increment) & ADDRESS_MASK;
    }

    /// One DMA copy byte, VRAM to VRAM (Exodus `PerformDMACopyOperation`, General.cpp
    /// 2630-2677).
    fn copy(&mut self) {
        let source = u32::from(self.s.registers[21]) | u32::from(self.s.registers[22]) << 8;
        let byte = self.s.vram[((source ^ 1) & 0xFFFF) as usize];
        self.vram_write_byte(self.s.address, byte);
        self.s.address = (self.s.address + u32::from(self.s.registers[15])) & ADDRESS_MASK;
    }

    fn transfer_read(&mut self, host: &mut impl VdpHost) {
        let r = &self.s.registers;
        let source = u32::from(r[21]) << 1 | u32::from(r[22]) << 9 | u32::from(r[23] & 0x7F) << 17;
        self.s.transfer_value = host.dma_read_word(source & 0xFFFFFE);
        self.s.transfer_cached = true;
    }

    fn transfer_push(&mut self) {
        let value = self.s.transfer_value;
        self.push(value, true);
        self.s.transfer_cached = false;
    }

    /// A new FIFO entry (Exodus WriteInterface, Ports.cpp 1273-1300): its code and
    /// address are the command registers' if rewritten since the last entry, else the
    /// last entry's with the address incremented.
    fn push(&mut self, data: u16, dma: bool) {
        let s = &mut self.s;
        let write = s.fifo_write;
        if s.modified {
            s.fifo[write].code = s.code;
            s.fifo[write].address = s.address;
            s.modified = false;
        } else {
            let previous = s.fifo[(write + 3) & 3];
            s.fifo[write].code = previous.code;
            s.fifo[write].address = (previous.address + u32::from(s.registers[15])) & ADDRESS_MASK;
        }
        s.fifo[write].data = data;
        s.fifo[write].half = false;
        s.fifo[write].pending = true;
        let delay = if dma && s.fifo[write].code & 0x0F == WRITE_VRAM {
            FIFO_COMMIT_DELAY_MCLK
        } else {
            0
        };
        s.fifo_ready[write] = s.mclk + delay;
        s.fifo_write = (write + 1) & 3;
    }

    /// Count down the length and up the source (Exodus `AdvanceDMAState`, General.cpp
    /// 2890-2935): only registers 21 and 22 count, so a transfer wraps within 128 KB; a
    /// length of 0 means $10000.
    fn dma_step(&mut self) {
        let s = &mut self.s;
        let r = &mut s.registers;
        let length = ((u32::from(r[20]) << 8 | u32::from(r[19])).wrapping_sub(1)) & 0xFFFF;
        r[19] = length as u8;
        r[20] = (length >> 8) as u8;
        let source = (u32::from(r[21]) | u32::from(r[22]) << 8) + 1;
        r[21] = source as u8;
        r[22] = (source >> 8) as u8;
        if length == 0 {
            s.code &= !0x20;
            s.fill_running = false;
            s.transfer_active = false;
        }
    }

    // -- the 68000's ports -----------------------------------------------------------------

    pub fn read_word(&mut self, host: &mut impl VdpHost, offset: u32, mclk: i64) -> u16 {
        self.s.hold = 0;
        let port = (offset >> 2) & 7;
        if port >= 4 {
            return host.open_bus(); // the PSG (reading locks up: CMD), unused, test
        }
        self.sync(host, mclk);
        if port == 0 {
            self.s.pending = false;
            if (self.s.code & 0x13) != 0 {
                return self.s.read_buffer; // an invalid read target locks up the hardware
            }
            self.advance(host, mclk, Until::ReadAvailable);
            let s = &mut self.s;
            s.hold = (s.read_ready_at - mclk).max(0);
            if s.read_early.0 <= mclk && mclk <= s.read_early.1 {
                s.hold = 0;
            }
            s.read_fill_after = mclk + s.hold + READ_FILL_LEAD_MCLK;
            s.read_available = false;
            s.read_half = false;
            return s.read_buffer;
        }
        if port == 1 {
            self.s.pending = false;
            // The upper six lines float: the next opcode word (CMD 1.1 note 1).
            self.sample_point(host, mclk);
            let value = (host.open_bus() & 0xFC00)
                | self
                    .s
                    .status(self.renderer.overflow, self.renderer.collision);
            self.renderer.overflow = false;
            self.renderer.collision = false;
            return value;
        }
        self.read_hv(host, mclk)
    }

    pub fn read_byte(&mut self, host: &mut impl VdpHost, offset: u32, mclk: i64) -> u8 {
        let word = self.read_word(host, offset & 0x1E, mclk);
        if offset & 1 != 0 {
            word as u8
        } else {
            (word >> 8) as u8
        }
    }

    /// A word write; returns (and leaves in `hold`) the MCLK the 68000 is held.
    pub fn write_word(
        &mut self,
        host: &mut impl VdpHost,
        offset: u32,
        value: u16,
        mclk: i64,
    ) -> i64 {
        self.s.hold = 0;
        let port = (offset >> 2) & 7;
        if port == 7 {
            // $C0001C, the debug register (Kabuto; CMD `newreg.txt`): only the
            // plane-forcing bits, plus the PSG's three test bits (11-9).
            self.s.debug_register = value;
            host.psg_write_test(((value >> 9) & 7) as u8);
            return 0;
        }
        if port == 4 || port == 5 {
            // $C00011-$C00017: the PSG ("the data must be in the LSB", CMD 1 note 3).
            host.sound_sync(mclk);
            host.psg_write(value as u8);
            return 0;
        }
        if port >= 4 {
            return 0; // the unused addresses and the test register
        }
        self.sync(host, mclk);
        if port == 0 {
            self.s.hold = self.data_write(host, value, mclk);
        } else if port == 1 {
            self.s.hold = self.control_write(host, value, mclk);
        }
        self.s.hold
    }

    pub fn write_byte(
        &mut self,
        host: &mut impl VdpHost,
        offset: u32,
        value: u8,
        mclk: i64,
    ) -> i64 {
        let word = u16::from(value) << 8 | u16::from(value);
        if offset >= 0x10 {
            let port = (offset >> 2) & 7;
            if port == 7 {
                // the debug register takes a byte as a word
                return self.write_word(host, offset & 0x1E, word, mclk);
            }
            self.s.hold = 0;
            if (port == 4 || port == 5) && offset & 1 != 0 {
                // "Doing byte-wide writes to even PSG addresses has no effect" (CMD 1
                // note 3), so only the odd half reaches the chip.
                host.sound_sync(mclk);
                host.psg_write(value);
            }
            return 0;
        }
        self.write_word(host, offset & 0x1E, word, mclk)
    }

    fn data_write(&mut self, host: &mut impl VdpHost, value: u16, mclk: i64) -> i64 {
        self.s.pending = false;
        self.advance(host, mclk, Until::FifoNotFull);
        let held = (self.s.mclk - mclk).max(0);
        self.push(value, false);
        let s = &mut self.s;
        s.code |= 0x10; // a data port write sets CD4 (Exodus Ports.cpp 1318)
        s.read_available = false;
        s.read_half = false;
        held
    }

    fn control_write(&mut self, host: &mut impl VdpHost, value: u16, mclk: i64) -> i64 {
        let mut transfer = false;
        if !self.s.pending {
            let s = &mut self.s;
            s.pending = true;
            s.code = (s.code & 0x3C) | (value >> 14) as u8;
            s.original_address = (s.original_address & 0x1C000) | u32::from(value & 0x3FFF);
            s.code |= 0x10; // the first half sets CD4 (Exodus Ports.cpp 1385-1394)
            if s.code & 0x03 == 0x02 {
                // A register write, taken from the address bits just written.
                let number = ((s.original_address >> 8) & 0x1F) as usize;
                if !s.mode5() {
                    // In mode 4 a register write clears the F flag (Nemesis's captures;
                    // Nuked-MD probes for the mechanism; docs/worklog.md, "F flag").
                    s.f_flag = false;
                }
                if number < if s.mode5() { 0x18 } else { 0x0B } {
                    self.write_register(number, value as u8);
                }
                self.s.pending = false;
            }
        } else {
            let s = &mut self.s;
            s.pending = false;
            let bits: u8 = if s.dma_enabled() { 0x3C } else { 0x1C }; // CD5 only with DMA enabled
            s.code = (s.code & !bits & 0x3F) | ((value >> 2) as u8 & bits);
            // Bits 2-0 are address bits 16-14 in mode 5 (Nuked-MD: `reg_addr`).
            let upper = if s.mode5() { 7 } else { 3 };
            s.original_address = (s.original_address & 0x3FFF) | u32::from(value & upper) << 14;
            transfer = s.code & 0x20 != 0 && s.dmd() < 2;
        }
        let s = &mut self.s;
        s.address = s.original_address;
        s.modified = true;
        s.read_available = false;
        s.read_half = false;
        s.read_fill_after = s.read_fill_after.max(mclk + READ_FILL_LEAD_MCLK);
        // A 68000-to-VDP transfer starts once the whole command word is in place.
        if transfer {
            self.run_transfer(host, mclk)
        } else {
            0
        }
    }

    /// A 68000-to-VDP transfer: the VDP takes the bus once the write that started it is
    /// over and holds it until the last word is read (Nemesis's
    /// TestDMATransferBusLock); returns how long the 68000's next bus cycle waits.
    fn run_transfer(&mut self, host: &mut impl VdpHost, mclk: i64) -> i64 {
        self.s.transfer_active = true;
        self.s.transfer_cached = false;
        // The first word is fetched once the write that started the transfer is over,
        // and after the fixed start-up Kabuto measured.
        let written = mclk + WRITE_CYCLE_MCLK + DMA_TRIGGER_HOLD_MCLK; // the write is over
        self.s.read_inc = DMA_READ_MCLK;
        let first = if self.s.dma_die {
            // A pixel clock's length after the write is over (slower around the sync),
            // then master clocks.
            let mut start = written;
            for _ in 0..DMA_FIRST_READ_DIE_PIXELS {
                start += self.s.pixels_after(start, 1) - self.s.pixels_after(start, 0);
            }
            self.s.dma_tick_after(start + DMA_FIRST_READ_DIE_MCLK - 1)
        } else {
            written + DMA_STARTUP_MCLK
        };
        self.s.transfer_next_read = first - self.s.read_inc;
        self.advance(host, mclk, Until::TransferDone);
        let now = self.s.mclk;
        if DMA_TAKES_REFRESH {
            let due = host.cart_refresh_due();
            if *due <= now {
                *due += ((now - *due).div_euclid(CART_REFRESH_MCLK) + 1) * CART_REFRESH_MCLK;
            }
        }
        let back = if self.s.dma_die {
            let back = written.max(now + DMA_RESUME_DIE_MCLK) - (mclk + WRITE_CYCLE_MCLK);
            -((-back).div_euclid(7)) * 7 // on the 68000's clock
        } else {
            DMA_TRIGGER_HOLD_MCLK + (now - written).max(0)
        };
        if RAM_REFRESH_PAUSES {
            // While the VDP holds the bus: from just after the write that started the
            // transfer to just after its last read.
            let start = mclk + RAM_REFRESH_PAUSE_FROM_MCLK;
            let due = host.ram_refresh_due();
            *due = ram_refresh_paused(
                *due,
                start,
                now + RAM_REFRESH_PAUSE_TAIL_MCLK,
                start + RAM_REFRESH_PAUSE_BEGAN_MCLK,
            );
        }
        back
    }

    // -- the Z80's side (byte accesses through $7F00 or its bank) ----------------------------

    pub fn z80_read(&mut self, host: &mut impl VdpHost, offset: u32, mclk: i64) -> u8 {
        let at = mclk.max(self.s.mclk);
        self.read_byte(host, offset, at)
    }

    pub fn z80_write(&mut self, host: &mut impl VdpHost, offset: u32, value: u8, mclk: i64) {
        let at = mclk.max(self.s.mclk);
        self.write_byte(host, offset, value, at);
    }

    /// Through the bank undriven bits read 1: the unused addresses $FF and the status's
    /// top six bits set (CMD 1.1 notes 1 and 4).
    pub fn z80_bank_read(&mut self, host: &mut impl VdpHost, offset: u32, mclk: i64) -> u8 {
        let port = (offset >> 2) & 7;
        if port >= 4 {
            return 0xFF;
        }
        let at = mclk.max(self.s.mclk);
        let mut value = self.read_byte(host, offset, at);
        if port == 1 && offset & 1 == 0 {
            value |= 0xFC;
        }
        value
    }
}
