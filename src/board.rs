//! The board: both CPUs' memory maps, the Z80's bus and reset lines, and the devices.
//!
//! Transcribed from megadrive-python's `board.py`. Sources (its docstring and
//! docs/start-here.md have the tables): CMD `gen-hw.txt` sections 1-2 for the maps, the
//! byte/word rules, the lock-ups, the bus request and reset registers and the bank,
//! all "checked on the real thing"; the Manual for the version register and TMSS; MoD's
//! `gen_test_z80_bus` (forum t=3021, several machines) for the Z80's failed reads of
//! 68000 RAM; and Nuked-MD (die-derived) for the refresh and arbitration constants,
//! each with its measurement in the reference's comments.
//!
//! Rust's differences (docs/handoff-brief.md, "The shape Rust forces"):
//! - [`Board`] is the Z80's bus, and holds the VDP and [`Devices`], everything else.
//! - What the reference's board does through the system's callbacks (`catch_up_z80`,
//!   `z80_busack`, `on_z80_reset`, `on_z80_resume`) -- the 68000's side of the board --
//!   is in `system.rs`, on the 68000's bus, which owns the Z80 and this board.
//! - The 68000's IRC and `step_clocks`, which the reference reads from the CPU, are
//!   mirrored here by the 68000's bus at every access.
//! - The probe (`System.z80_probe`), which swaps the Z80's callables in the reference,
//!   is [`Devices::probe`], consulted first by the Z80's read and write.
//! - [`Lockup`] is recorded in [`Devices::lockup`] rather than raised.
//! - The Z80 writing $A11100 or $A11200 through its bank window, which the reference
//!   handles by probing and resetting the Z80 from inside its own step, stops the run
//!   with a message; a DMA whose source is the Z80's space or $A1xxxx does not run the
//!   Z80 forward first, as the reference's would.

use std::collections::HashMap;
use std::sync::Arc;

use sn76489_rust::chip::{SN76489, YM7101_PSG};
use ym2612_rust::chip::{IC_SLOT_LAG, TIMER_CONTROL_DELAY_CYCLES, YM2612};

use crate::arbiter::{z80_access_wait, CUTOFF_MCLK, IDLE_GRANT_MCLK, M68K_EDGE_PHASE};
use crate::cartridge::{BootRom, Cartridge};
use crate::io::{Device, IOChip};
use crate::machine::{Machine, M68K_DIVIDER, Z80_DIVIDER};
use crate::vdp::{Vdp, VdpHost};

/// The cartridge-side refresh: every 128 68000 clocks of real time, the next 68000 bus
/// cycle absorbs it, and if that cycle is a cartridge access it takes 2 clocks longer
/// (Kabuto, "The bus system"; made exact against Nuked-MD in four probe loops).
pub const CART_REFRESH_MCLK: i64 = 896;
pub const CART_REFRESH_STALL: i64 = 14;
/// ... unless the bus was free long enough for it to run by itself, from this long
/// after it fell due (Nuked-MD; docs/worklog.md, "The cartridge refresh when the bus is
/// free").
pub const CART_REFRESH_SELF_MCLK: i64 = 112;
/// The same refresh falling on a 68000 access to the Z80's space stretches it by one
/// 68000 clock, not two -- and by none if it fell due only in the last 68000 clock
/// before /AS (Nuked-MD, 519 refreshes; docs/worklog.md, "The one-frame slip").
pub const Z80_REFRESH_STALL: i64 = 7;
pub const Z80_REFRESH_LATE: i64 = 7;
/// A refresh that stretches a cartridge access pulses /REF from 39 to this long after
/// it fell due; an access to the Z80's space under the pulse waits a 68000 clock
/// (Nuked-MD; docs/worklog.md, "Under the /REF pulse").
pub const CART_REFRESH_PULSE_END_MCLK: i64 = 46;
/// The work-RAM refresh (Kabuto: "RAM accesses get their own further slowdowns"),
/// fitted to Nuked-MD's 68000 in Overdrive 2's 3D scene (docs/worklog.md "The work-RAM
/// refresh"). `RAM_REFRESH_HIDDEN` false keeps the old rule.
pub const RAM_REFRESH_STALL: i64 = 21;
pub const RAM_REFRESH_AFTER_MCLK: i64 = 819;
pub const RAM_REFRESH_TRIGGER_MCLK: i64 = 98;
pub const RAM_REFRESH_SELF_MCLK: i64 = 112;
pub const RAM_REFRESH_HIDDEN: bool = true;
/// The clocks the timer counts from a refresh to the next falling due, in MCLK.
pub const RAM_REFRESH_COUNT_MCLK: i64 = 112 * 7;
/// A refresh due this long after a work-RAM access's /AS is the die's tie, taken by
/// the nuked-md machine only (docs/worklog.md, "The tie at 112").
pub const RAM_REFRESH_TIE_MCLK: i64 = 7;
/// Every 68000 access to the Z80's space costs one extra 68000 clock (Nuked-MD and
/// MAME's `before_delay(1)` agreeing; the user's decision 1).
pub const Z80_SPACE_STALL: i64 = M68K_DIVIDER;
/// A Z80 access to the VDP ($7F00-$7F1F) goes over the 68000's bus, through the same
/// arbitration as one through the bank window (Nuked-MD; die-derived for reads).
pub const Z80_VDP_ARBITRATED: bool = true;
/// A Z80 access over the 68000's bus delays the 68000 only from the bus cycle it next
/// wants (Nuked-MD's 68000 trace; docs/worklog.md "The ninja's arm colours").
pub const Z80_GRANT_OVERLAPS_IDLE: bool = true;
/// The Z80 is brought up to each 68000 bus cycle before it starts (docs/worklog.md,
/// "The Z80's grant inside an instruction"). Die-derived.
pub const Z80_CATCH_UP_EACH_CYCLE: bool = true;
/// The cartridge refresh stretches a Z80 access over the 68000's bus only if it goes
/// to the cartridge. Die-derived.
pub const Z80_REFRESH_CARTRIDGE_ONLY: bool = true;
/// A 68000 write to the FM is taken this long after its /AS, and a 68000 read gets the
/// status as it stands this long after (Nuked-MD, `scripts/run_fm_busy.py`). The
/// nuked-md machine only.
pub const M68K_FM_WRITE_MCLK: i64 = 21;
pub const M68K_FM_READ_MCLK: i64 = 27;
/// Whatever it goes to, a Z80 cycle over the 68000's bus uses up a cartridge refresh
/// that fell due by its start (Nuked-MD; docs/worklog.md, "The refresh a Z80 cycle
/// uses up").
pub const Z80_REFRESH_USED_UP: bool = true;
/// The refresh tie is judged against the refresh due by the access, after the ones no
/// access took have run by themselves (docs/worklog.md, "The tie after the self-runs").
pub const RAM_REFRESH_TIE_AFTER_SELF_RUNS: bool = true;

/// The work-RAM refresh due at `t` once those nobody took since `due` have run by
/// themselves.
pub fn ram_refresh_self_runs(due: i64, t: i64) -> i64 {
    let ran = RAM_REFRESH_SELF_MCLK + RAM_REFRESH_STALL;
    if RAM_REFRESH_HIDDEN && t >= due + ran {
        let period = RAM_REFRESH_SELF_MCLK + RAM_REFRESH_AFTER_MCLK;
        return due + ((t - due - ran).div_euclid(period) + 1) * period;
    }
    due
}

/// The work-RAM refresh for a work-RAM access starting at `t` with the next refresh due
/// at `due`: the MCLK the access waits, and when the next falls due. `tie`: the access
/// takes a refresh due one clock after its /AS.
pub fn ram_refresh(due: i64, t: i64, tie: bool) -> (i64, i64) {
    let mut due = due;
    if t < due && !tie {
        return (0, due);
    }
    if !RAM_REFRESH_HIDDEN {
        return (RAM_REFRESH_STALL, t + 805);
    }
    let rolled = ram_refresh_self_runs(due, t);
    if rolled != due {
        due = rolled;
        if t < due && !tie {
            return (0, due);
        }
    }
    if t < due + RAM_REFRESH_TRIGGER_MCLK {
        return (RAM_REFRESH_STALL, t + RAM_REFRESH_AFTER_MCLK);
    }
    let began = due + RAM_REFRESH_SELF_MCLK;
    // Until its end, on the 68000's next clock edge (the die's waits are 7, 14, 28, 35).
    let wait = -((t - began - RAM_REFRESH_STALL).div_euclid(M68K_DIVIDER)) * M68K_DIVIDER;
    (wait, began + RAM_REFRESH_AFTER_MCLK)
}

/// The work-RAM refresh due time after the VDP held the bus from `start` to `end` for a
/// transfer: the refresh timer stops meanwhile and goes on from where it was. A
/// self-run that began by `began_by` goes ahead under the pause.
pub fn ram_refresh_paused(due: i64, start: i64, end: i64, began_by: i64) -> i64 {
    let (mut due, mut start) = (due, start);
    if began_by >= due + RAM_REFRESH_SELF_MCLK {
        let period = RAM_REFRESH_SELF_MCLK + RAM_REFRESH_AFTER_MCLK;
        due += ((began_by - due - RAM_REFRESH_SELF_MCLK).div_euclid(period) + 1) * period;
        start = start.max(due - RAM_REFRESH_COUNT_MCLK);
    }
    let edges_before = |t: i64| -((M68K_EDGE_PHASE - t).div_euclid(M68K_DIVIDER));
    due + M68K_DIVIDER * (edges_before(end) - edges_before(start)).max(0)
}

/// A held 68000 bus cycle lasts whole 68000 clocks (docs/worklog.md, "Handover by
/// handover"). False keeps the master clocks.
pub const HOLD_ON_68K_EDGE: bool = true;
/// How long after a write to $A11200 begins the YM2612 is given /IC (docs/worklog.md,
/// "The VDPs' offset"). 0.
pub const ZRES_PIN_MCLK: i64 = 0;

/// A hold of `held` MCLK as the 68000 sees it: rounded up to its clock.
pub fn on_edge(held: i64) -> i64 {
    if HOLD_ON_68K_EDGE && held % M68K_DIVIDER != 0 {
        return held + M68K_DIVIDER - held % M68K_DIVIDER;
    }
    held
}

/// An access that locks up a real console (CMD `gen-hw.txt` section 1); the system
/// stops and reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lockup(pub String);

impl std::fmt::Display for Lockup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Lockup {}

/// The probe's bus (`System.z80_probe`): what the Z80's next step would access.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    pub offsets: Vec<i64>,
    pub ends: Vec<i64>,
    /// Machine cycles over the 68000's bus: (end, start) in T-states ...
    pub bus_ends: Vec<(i64, i64)>,
    /// ... and whether each is to the cartridge.
    pub bus_cart: Vec<bool>,
    pub written: HashMap<u16, u8>,
    pub clock: i64,
    pub fetching: bool,
    pub next_fetch: u16,
    pub after_prefix: bool,
}

/// What the Z80 settled before an instruction ran (`System._prearbitrate`): its start,
/// (the Z80's wait, the 68000's) for each access, and the accesses as (T-state offset,
/// to the cartridge).
pub type Prearbitrated = (i64, Vec<(i64, i64)>, Vec<(i64, bool)>);

/// Everything on the board but the VDP.
pub struct Devices {
    pub machine: Machine,
    /// The cartridge (the reference's `game_cartridge`), and the TMSS boot ROM that
    /// stands in its place while $A14101 bit 0 is clear.
    pub cartridge: Cartridge,
    pub tmss_rom: Option<BootRom>,
    pub tmss_cart_selected: bool,
    pub ram: Vec<u8>,
    pub z80_ram: Vec<u8>,
    pub io: IOChip,
    /// The two sound chips, lazy: they catch up to the master clock of an access
    /// before it. The FM shares /RESET with the Z80, held at power-on.
    pub fm: YM2612,
    pub fm_write_lag: i64,
    pub fm_read_lag: i64,
    /// The PSG lives inside the VDP and is written at $C00011 or $7F11.
    pub psg: SN76489,
    pub psg_mclk: i64,
    /// 68000 address bits 23-15 of the Z80's bank window; power-on 0.
    pub bank: u32,
    /// "On power-up, the Z80 will be reset and will have the bus" (CMD 2.2).
    pub z80_reset: bool,
    pub z80_bus_requested: bool,
    /// TMSS: the VDP is locked until "SEGA" is written to $A14000 (Manual p. 94).
    pub tmss_register: u32,
    /// The 68000's IRC and `step_clocks` as of its last bus access (or step), which the
    /// reference reads from the CPU.
    pub m68k_irc: u16,
    pub m68k_step_clocks: u32,
    /// The master clock at which the current CPU step started, set by the system.
    pub step_start: i64,
    /// The Z80 instruction under way: its memory accesses' offsets from `step_start`
    /// in master clocks (None for an interrupt acknowledge), and how many it has made.
    pub z80_offsets: Option<Arc<[i64]>>,
    pub z80_access: usize,
    pub z80_bus_access: usize,
    pub z80_prearbitrated: Option<Prearbitrated>,
    /// Master clocks the 68000 has been held during the current step.
    pub stall: i64,
    /// How much of `stall` is the Z80-space hold, for the MAME lockstep.
    pub z80_space_stall: i64,
    pub cart_refresh_due: i64,
    pub cart_refresh_pulse_end: i64,
    pub z80_refresh_taken: i64,
    pub ram_refresh_due: i64,
    pub refresh_enabled: bool,
    /// The arbiter's delays (milestone 5); MAME models neither, so the milestone-1
    /// lockstep turns this off with `refresh_enabled`.
    pub arbiter_enabled: bool,
    /// Each 68000 bus cycle started in the step under way, as `[/AS fell, it stays
    /// down]`.
    pub m68k_accesses: Vec<[i64; 2]>,
    pub z80_stall: i64,
    pub m68k_pending_stall: i64,
    pub grant_wait: i64,
    pub m68k_resume_at: i64,
    pub m68k_step_end: i64,
    pub bank_accesses: u64,
    pub bank_z80_cycles: i64,
    pub bank_m68k_cycles: i64,
    /// The master clock the Z80 lets go of the bus after the current request.
    pub busack_at: i64,
    /// For the Z80 step resuming after a hold: from which clock its accesses wait, and
    /// for how long.
    pub z80_hold: Option<(i64, i64)>,
    /// The probe's bus while the system probes the Z80.
    pub probe: Option<Probe>,
    /// An access that locks up a real console, recorded for the system to report.
    pub lockup: Option<Lockup>,
}

/// The board: the VDP and everything else; the Z80's bus.
pub struct Board {
    pub vdp: Vdp,
    pub dev: Devices,
}

impl Board {
    /// `pads` None is the reference's default, two 3-button pads.
    pub fn new(
        machine: Machine,
        cartridge: Option<Cartridge>,
        pads: Option<[Option<Device>; 2]>,
    ) -> Self {
        let mut fm = YM2612::new(machine.fm, true);
        if machine.dma_figures == "die" {
            fm.control_delay = TIMER_CONTROL_DELAY_CYCLES;
        }
        let (fm_write_lag, fm_read_lag) = if machine.dma_figures == "die" {
            (M68K_FM_WRITE_MCLK, M68K_FM_READ_MCLK)
        } else {
            (0, 0)
        };
        fm.reset_line(true, None, IC_SLOT_LAG);
        Board {
            vdp: Vdp::new(machine),
            dev: Devices {
                machine,
                cartridge: cartridge.unwrap_or_else(Cartridge::empty),
                tmss_rom: None,
                tmss_cart_selected: true,
                ram: vec![0; 0x10000],
                z80_ram: vec![0; 0x2000],
                io: IOChip::new(machine.version_register(), pads),
                fm,
                fm_write_lag,
                fm_read_lag,
                psg: SN76489::new(YM7101_PSG),
                psg_mclk: 0,
                bank: 0,
                z80_reset: true,
                z80_bus_requested: false,
                tmss_register: 0,
                m68k_irc: 0,
                m68k_step_clocks: 0,
                step_start: 0,
                z80_offsets: None,
                z80_access: 0,
                z80_bus_access: 0,
                z80_prearbitrated: None,
                stall: 0,
                z80_space_stall: 0,
                cart_refresh_due: CART_REFRESH_MCLK,
                cart_refresh_pulse_end: 0,
                z80_refresh_taken: -1,
                ram_refresh_due: RAM_REFRESH_AFTER_MCLK,
                refresh_enabled: true,
                arbiter_enabled: true,
                m68k_accesses: Vec::new(),
                z80_stall: 0,
                m68k_pending_stall: 0,
                grant_wait: 0,
                m68k_resume_at: 0,
                m68k_step_end: 0,
                bank_accesses: 0,
                bank_z80_cycles: 0,
                bank_m68k_cycles: 0,
                busack_at: 0,
                z80_hold: None,
                probe: None,
                lockup: None,
            },
        }
    }
}

impl Devices {
    pub fn lock_up(&mut self, message: String) {
        if self.lockup.is_none() {
            self.lockup = Some(Lockup(message));
        }
    }

    // -- TMSS --------------------------------------------------------------------------

    /// Fit the TMSS boot ROM, which a TMSS console powers up running (the reference's
    /// docstring: a board without one costs nothing, and a caller asks for it).
    pub fn install_tmss_rom(&mut self, image: Vec<u8>) {
        self.tmss_rom = Some(BootRom::new(image));
        self.tmss_cart_selected = false;
    }

    /// Bit 0 of $A14101: 1 puts the cartridge on the bus, 0 the boot ROM.
    fn tmss_select(&mut self, cartridge: bool) {
        if self.tmss_rom.is_none() || cartridge == self.tmss_cart_selected {
            return;
        }
        self.tmss_cart_selected = cartridge;
    }

    /// The boot ROM is on the bus in the cartridge's place.
    fn boot(&self) -> Option<&BootRom> {
        if self.tmss_cart_selected {
            None
        } else {
            self.tmss_rom.as_ref()
        }
    }

    pub fn cart_read_byte(&mut self, address: u32) -> u8 {
        match self.boot() {
            Some(boot) => boot.read_byte(address),
            None => self.cartridge.read_byte(address),
        }
    }

    pub fn cart_read_word(&mut self, address: u32) -> u16 {
        match self.boot() {
            Some(boot) => boot.read_word(address),
            None => self.cartridge.read_word(address),
        }
    }

    pub fn cart_write_byte(&mut self, address: u32, value: u8) {
        if self.boot().is_none() {
            self.cartridge.write_byte(address, value);
        }
    }

    pub fn cart_write_word(&mut self, address: u32, value: u16) {
        if self.boot().is_none() {
            self.cartridge.write_word(address, value);
        }
    }

    pub fn cart_write_time(&mut self, address: u32, value: u8) {
        if self.boot().is_none() {
            self.cartridge.write_time(address, value);
        }
    }

    pub fn cart_read_time(&self, address: u32) -> Option<u8> {
        match self.boot() {
            Some(_) => None,
            None => self.cartridge.read_time(address),
        }
    }

    // -- time ----------------------------------------------------------------------------

    /// The master clock at which the 68000's current bus access starts: m68000-rust
    /// counts an instruction's clocks as it goes, adding 4 for each bus access before
    /// calling the host, so `step_clocks - 4` is where the access began.
    pub fn m68k_access_clock(&self) -> i64 {
        self.step_start + self.stall + (i64::from(self.m68k_step_clocks) - 4) * M68K_DIVIDER
    }

    /// The Z80 runs unless it is held in reset or has granted its bus.
    pub fn z80_running(&self) -> bool {
        !self.z80_reset && !self.z80_bus_requested
    }

    /// "The Z80 bus can only be accessed by the 68000 when the Z80 is running and the
    /// 68000 has the bus" (CMD 2.2); with the arbiter modelled an access at `t` before
    /// the grant is stalled to it.
    pub fn m68k_has_z80_bus(&mut self, t: Option<i64>) -> bool {
        if !self.z80_bus_requested || self.z80_reset {
            return false;
        }
        if let Some(t) = t {
            if self.arbiter_enabled && t < self.busack_at {
                self.stall += on_edge(self.busack_at - t);
            }
        }
        true
    }

    /// The one extra 68000 clock every access to $A00000-$A0FFFF costs, counted apart
    /// as well as charged.
    pub fn z80_space_cost(&mut self) {
        self.stall += Z80_SPACE_STALL;
        self.z80_space_stall += Z80_SPACE_STALL;
        if self.arbiter_enabled {
            if let Some(last) = self.m68k_accesses.last_mut() {
                // The bus cycle itself is that much longer, as for a refresh stretch.
                last[1] += Z80_SPACE_STALL;
            }
        }
    }

    // -- the arbiter -----------------------------------------------------------------------

    /// The master clock of the Z80 access under way: its instruction's start, its
    /// offset in the instruction, and what the arbiter has held the Z80 for so far.
    pub fn z80_access_time(&self) -> i64 {
        let k = self.z80_access as i64 - 1;
        let t = match &self.z80_offsets {
            Some(offsets) if 0 <= k && (k as usize) < offsets.len() => {
                self.step_start + offsets[k as usize] + self.z80_stall
            }
            _ => self.step_start + self.z80_stall,
        };
        match self.z80_hold {
            Some((from, held)) if t >= from => t + held,
            _ => t,
        }
    }

    /// A byte of the Z80's space for looking at code: RAM and cartridge ROM; anything
    /// else reads $FF.
    pub fn z80_peek(&mut self, address: u16) -> u8 {
        if address < 0x4000 {
            return self.z80_ram[usize::from(address & 0x1FFF)];
        }
        if address >= 0x8000 {
            let full = self.bank | u32::from(address & 0x7FFF);
            if full < 0x400000 {
                return self.cart_read_byte(full);
            }
        }
        0xFF
    }

    /// What one Z80 access over the 68000's bus -- the bank window, or the VDP -- costs
    /// both CPUs (`arbiter::z80_access_wait`); the 68000's share is owed.
    fn bank_delay(&mut self, cartridge: bool) {
        let index = self.z80_bus_access;
        self.z80_bus_access += 1;
        let settled = match &self.z80_prearbitrated {
            // Settled before the instruction ran, at its own time: only the Z80's
            // wait remains to be taken.
            Some(done) if done.0 == self.step_start && index < done.1.len() => Some(done.1[index]),
            _ => None,
        };
        let (wait, m68k_cycles) = match settled {
            Some(pair) => pair,
            None => self.arbitrate(self.z80_access_time(), cartridge),
        };
        self.z80_stall += wait * Z80_DIVIDER;
        self.bank_accesses += 1;
        self.bank_z80_cycles += wait;
        self.bank_m68k_cycles += m68k_cycles;
    }

    /// A Z80 access over the 68000's bus at `t`: the 68000's share charged (owed, and
    /// its later cycles in the grid moved past the grant), the refresh it takes; returns
    /// the Z80's wait states and the 68000's, in their own clocks.
    pub fn arbitrate(&mut self, t: i64, cartridge: bool) -> (i64, i64) {
        // The cartridge refresh grid: the last tick due by the end of the Z80's grant
        // window, owed if no 68000 cycle since took it and the Z80 has not already.
        let due = self.cart_refresh_due;
        let tick = due
            + (t + CUTOFF_MCLK + IDLE_GRANT_MCLK - due).div_euclid(CART_REFRESH_MCLK)
                * CART_REFRESH_MCLK;
        let taken = tick == self.z80_refresh_taken
            || self
                .m68k_accesses
                .iter()
                .any(|&[begin, _]| tick <= begin && begin <= t + CUTOFF_MCLK);
        let stretched = !taken && (cartridge || !Z80_REFRESH_CARTRIDGE_ONLY);
        let (wait, mut resume, own) = z80_access_wait(
            t,
            &self.m68k_accesses,
            if stretched { Some(tick) } else { None },
        );
        if !taken && tick <= own {
            self.z80_refresh_taken = tick;
            if Z80_REFRESH_USED_UP && self.cart_refresh_due <= tick {
                self.cart_refresh_due = tick + CART_REFRESH_MCLK;
            }
        }
        let mut wanted = t + CUTOFF_MCLK + 1;
        let mut next = None;
        for &[begin, _] in &self.m68k_accesses {
            if begin > t + CUTOFF_MCLK {
                next = Some(begin);
                break;
            }
            wanted = wanted.max(begin + 4 * M68K_DIVIDER); // its next, four clocks on
        }
        if Z80_GRANT_OVERLAPS_IDLE {
            // The 68000 waits only from when it wants the bus again: its next bus cycle
            // in this instruction, or else where its next instruction starts.
            let next = next.unwrap_or(self.m68k_step_end + self.m68k_pending_stall);
            wanted = wanted.max(next);
        }
        resume += (wanted - resume).rem_euclid(M68K_DIVIDER);
        let m68k_cycles = (resume - wanted).max(0) / M68K_DIVIDER;
        if m68k_cycles != 0 {
            // The 68000's cycles past the cut-off happen that much later.
            for cycle in &mut self.m68k_accesses {
                if cycle[0] > t + CUTOFF_MCLK {
                    cycle[0] += m68k_cycles * M68K_DIVIDER;
                }
            }
        }
        self.m68k_pending_stall += m68k_cycles * M68K_DIVIDER;
        (wait, m68k_cycles)
    }

    // -- the sound chips -------------------------------------------------------------------

    /// Run both sound chips up to `mclk`.
    pub fn sound_sync(&mut self, mclk: i64) {
        self.fm.run(mclk);
        let clocks = (mclk - self.psg_mclk).div_euclid(Z80_DIVIDER);
        if clocks > 0 {
            self.psg_mclk += clocks * Z80_DIVIDER;
            self.psg.run(clocks as u64);
        }
    }

    /// The YM2612's status byte at $A04000-$A04003 (or $4000-$4003): the discrete chip
    /// answers on port 0 only and its other three read 0 (Genesis Plus GX, labelled
    /// emulator-derived); the ASIC answers on all four (Eke, forum t=386 start=810).
    pub fn fm_read(&mut self, port: u8, mclk: i64) -> u8 {
        self.sound_sync(mclk);
        if port != 0 && !self.machine.fm.busy_on_all_ports {
            return 0x00;
        }
        self.fm.read(port)
    }

    // -- open bus ----------------------------------------------------------------------------

    /// The 68000's undriven data bus: the next opcode word it will fetch (CMD 1 note 4
    /// and Kabuto, "The VDP ports").
    pub fn open_bus(&self) -> u16 {
        self.m68k_irc
    }

    /// "Reads return the MSB of the next instruction to be fetched, with the LSB set to
    /// zero" (CMD 1 note 1).
    pub fn open_bus_msb(&self) -> u16 {
        self.open_bus() & 0xFF00
    }

    /// One bit into the bank register: bits 15-23 of the window's 68000 address arrive
    /// in that order, each write shifting the others down (CMD 2.3).
    pub fn write_bank(&mut self, value: u8) {
        self.bank = ((self.bank >> 1) | (u32::from(value & 1) << 23)) & 0xFF8000;
    }

    /// The VDP decodes 110n n000 nnnn nnnn 000m mmmm (CMD 1 note 5); anything else in
    /// $C00000-$DFFFFF locks up, and so does a VDP access on a TMSS console before
    /// "SEGA" is written to $A14000 (Manual PDF page 94). True if the access goes ahead.
    pub fn vdp_access(&mut self, address: u32) -> bool {
        if address & 0xE700E0 != 0xC00000 {
            self.lock_up(format!(
                "68000 access to ${address:06X}: outside the VDP's mirror pattern"
            ));
            return false;
        }
        if self.machine.tmss && self.tmss_register != 0x53454741 {
            self.lock_up(format!(
                "68000 access to the VDP at ${address:06X} before TMSS was unlocked"
            ));
            return false;
        }
        true
    }

    /// The parts of a write to $A1xxxx that need nothing of the Z80: the I/O chip,
    /// /TIME, TMSS. Returns false for the bus request and reset registers, which the
    /// caller handles. `high` is what D15-D8 carry, `low` D7-D0.
    pub fn system_write_devices(
        &mut self,
        address: u32,
        high: u8,
        low: u8,
        word: bool,
        mclk: i64,
    ) -> bool {
        let offset = address & 0xFFFF;
        if offset < 0x20 {
            self.io.write(address, low, mclk);
        } else if offset & 0xFF00 == 0x1100 || offset & 0xFF00 == 0x1200 {
            return false;
        } else if offset & 0xFF00 == 0x3000 {
            self.cart_write_time(address | 1, low);
        } else if offset & 0xFF00 == 0x4100 {
            // $A14101 bit 0: the cartridge (1) or the TMSS boot ROM (0); only bit 0 of
            // the byte written matters (the disassembly's `RAM_Code`; MAME `tmss_swap_w`).
            self.tmss_select(low & 1 != 0);
        } else if offset & 0xFFFC == 0x4000 {
            // The TMSS register takes "SEGA" as a long, two words or four bytes.
            let (shift, value, mask) = if word {
                (
                    16 - (offset & 2) * 8,
                    u32::from(high) << 8 | u32::from(low),
                    0xFFFF,
                )
            } else {
                ((3 - (offset & 3)) * 8, u32::from(low), 0xFF)
            };
            self.tmss_register = (self.tmss_register & !(mask << shift)) | (value << shift);
        }
        true
    }

    /// $A10000-$A1FFFF read as a word, but for the bus request register (which needs
    /// the Z80 brought up first): the I/O chip, /TIME, open bus.
    pub fn system_read_devices(&mut self, address: u32, mclk: i64) -> Option<u16> {
        let offset = address & 0xFFFF;
        if offset < 0x20 {
            let value = u16::from(self.io.read(address | 1, mclk));
            return Some(value | (value << 8));
        }
        if offset & 0xFF00 == 0x1100 {
            return None;
        }
        if offset & 0xFF00 == 0x3000 {
            if let Some(value) = self.cart_read_time(address | 1) {
                let value = u16::from(value);
                return Some(value | (value << 8));
            }
        }
        Some(self.open_bus_msb())
    }

    /// The bus request register's read, once the Z80 has been brought up to `t`: bit 0
    /// of the MSB is 1 while the 68000 cannot use the Z80's bus; the rest is the next
    /// instruction's MSB and a zero LSB (CMD 1 note 4, 2.2). The poll itself is not
    /// held: it reads "busy" until BUSACK.
    pub fn busreq_read(&self, t: i64) -> u16 {
        let mut granted = self.z80_bus_requested && !self.z80_reset;
        if granted && self.arbiter_enabled {
            granted = t >= self.busack_at;
        }
        let busy = if granted { 0 } else { 0x100 };
        (self.open_bus() & 0xFE00) | busy
    }

    /// The Z80's space as the 68000 reads it, the bus already granted: Z80 RAM, the FM's
    /// status, $FF for the bank register and $6100-$7EFF (CMD 2.1). None for the VDP
    /// through it, which locks up.
    pub fn z80_area_byte(&mut self, address: u32, t: i64) -> Option<u8> {
        let offset = address & 0x7FFF; // "A08000-A0FFFFh mirror A00000-A07FFFh" (CMD 1)
        if offset < 0x4000 {
            return Some(self.z80_ram[(offset & 0x1FFF) as usize]);
        }
        if offset < 0x6000 {
            let lag = self.fm_read_lag;
            return Some(self.fm_read((offset & 3) as u8, t + lag));
        }
        if offset >= 0x7F00 {
            self.lock_up(format!(
                "68000 access to ${address:06X}: the VDP through the Z80's space"
            ));
            return None;
        }
        Some(0xFF)
    }

    /// The Z80's space as the 68000 writes it, the bus already granted.
    pub fn z80_area_store(&mut self, address: u32, value: u8, t: i64) {
        let offset = address & 0x7FFF;
        if offset < 0x4000 {
            self.z80_ram[(offset & 0x1FFF) as usize] = value;
        } else if offset < 0x6000 {
            let lag = self.fm_write_lag;
            self.sound_sync(t + lag);
            self.fm.write((offset & 3) as u8, value);
        } else if offset < 0x6100 {
            self.write_bank(value);
        } else if offset >= 0x7F00 {
            self.lock_up(format!(
                "68000 access to ${address:06X}: the VDP through the Z80's space"
            ));
        }
    }
}

impl VdpHost for Devices {
    /// A word the VDP's 68000-to-VDP DMA reads from the 68000's bus (the 68000 is held
    /// meanwhile): cartridge, work RAM, or anything else as the 68000 would read it. A
    /// source in the VDP's own range reads $FFFF (not claimed).
    fn dma_read_word(&mut self, address: u32) -> u16 {
        let top = address >> 16;
        if top < 0x40 {
            return self.cart_read_word(address);
        }
        if top >= 0xE0 {
            let offset = (address & 0xFFFF) as usize;
            return u16::from(self.ram[offset]) << 8 | u16::from(self.ram[offset + 1]);
        }
        if top >= 0xC0 {
            return 0xFFFF;
        }
        // The 68000's own read without the refresh (the reference's `read_word` with
        // `refresh_enabled` off), but for the Z80's catch-up (docs/handoff-brief.md).
        let t = self.m68k_access_clock();
        if top == 0xA0 {
            let value = if self.m68k_has_z80_bus(Some(t)) {
                self.z80_area_byte(address, t)
            } else {
                None
            };
            self.z80_space_cost();
            return match value {
                Some(value) => u16::from(value) | u16::from(value) << 8,
                None => self.open_bus_msb(),
            };
        }
        if top == 0xA1 {
            return match self.system_read_devices(address, t) {
                Some(value) => value,
                None => self.busreq_read(t),
            };
        }
        if (0x80..0xA0).contains(&top) {
            self.lock_up(format!(
                "68000 read of ${address:06X} (32X area) without a 32X"
            ));
            return 0xFFFF;
        }
        self.open_bus_msb()
    }

    fn open_bus(&self) -> u16 {
        Devices::open_bus(self)
    }

    fn sound_sync(&mut self, mclk: i64) {
        Devices::sound_sync(self, mclk);
    }

    fn psg_write(&mut self, value: u8) {
        self.psg.write(value);
    }

    fn psg_write_test(&mut self, bits: u8) {
        self.psg.write_test(bits);
    }

    fn cart_refresh_due(&mut self) -> &mut i64 {
        &mut self.cart_refresh_due
    }

    fn ram_refresh_due(&mut self) -> &mut i64 {
        &mut self.ram_refresh_due
    }
}

// -- the Z80's bus ----------------------------------------------------------------------------

impl Board {
    fn z80_read(&mut self, address: u16) -> u8 {
        let dev = &mut self.dev;
        dev.z80_access += 1;
        if address < 0x4000 {
            return dev.z80_ram[usize::from(address & 0x1FFF)];
        }
        if address >= 0x8000 {
            let full = dev.bank | u32::from(address & 0x7FFF);
            if dev.arbiter_enabled {
                // the arbiter holds both CPUs for this access
                dev.bank_delay(full < 0x800000);
            }
            return self.z80_bank_read(full);
        }
        if address < 0x6000 {
            let t = dev.z80_access_time();
            return dev.fm_read((address & 3) as u8, t);
        }
        if address >= 0x7F00 {
            if address >= 0x7F20 {
                dev.lock_up(format!(
                    "Z80 access to ${address:04X} (beyond the VDP's $7F00-$7F1F)"
                ));
                return 0xFF;
            }
            if dev.arbiter_enabled && Z80_VDP_ARBITRATED {
                dev.bank_delay(false); // the VDP is on the 68000's bus
            }
            let t = dev.z80_access_time();
            return self
                .vdp
                .z80_read(&mut self.dev, u32::from(address & 0x1F), t);
        }
        0xFF // the bank register and $6100-$7EFF (CMD 2.1)
    }

    fn z80_write(&mut self, address: u16, value: u8) {
        let dev = &mut self.dev;
        dev.z80_access += 1;
        if address < 0x4000 {
            dev.z80_ram[usize::from(address & 0x1FFF)] = value;
        } else if address >= 0x8000 {
            let full = dev.bank | u32::from(address & 0x7FFF);
            if dev.arbiter_enabled {
                dev.bank_delay(full < 0x800000);
            }
            self.z80_bank_write(full, value);
        } else if address < 0x6000 {
            let t = dev.z80_access_time();
            dev.sound_sync(t);
            dev.fm.write((address & 3) as u8, value);
        } else if address < 0x6100 {
            dev.write_bank(value);
        } else if address >= 0x7F00 {
            if address >= 0x7F20 {
                dev.lock_up(format!(
                    "Z80 access to ${address:04X} (beyond the VDP's $7F00-$7F1F)"
                ));
                return;
            }
            if dev.arbiter_enabled && Z80_VDP_ARBITRATED {
                dev.bank_delay(false);
            }
            let t = dev.z80_access_time();
            self.vdp
                .z80_write(&mut self.dev, u32::from(address & 0x1F), value, t);
        }
    }

    /// A byte of the 68000's space through the Z80's window: reads of 68000 RAM fail
    /// (MoD's `gen_test_z80_bus`; CMD "returns FFh"); the Z80's own space locks up; the
    /// VDP's unused addresses read $FF and its status's top six bits 1 (CMD 1.1).
    fn z80_bank_read(&mut self, address: u32) -> u8 {
        let top = address >> 16;
        let dev = &mut self.dev;
        if top < 0x40 {
            return dev.cart_read_byte(address);
        }
        if top >= 0xE0 {
            return 0xFF;
        }
        if top >= 0xC0 {
            if address & 0xE700E0 != 0xC00000 {
                dev.lock_up(format!(
                    "Z80 bank access to ${address:06X}: outside the VDP's pattern"
                ));
                return 0xFF;
            }
            let t = dev.z80_access_time();
            return self.vdp.z80_bank_read(&mut self.dev, address & 0x1F, t);
        }
        if top == 0xA0 {
            dev.lock_up(format!(
                "Z80 bank access to ${address:06X}: the Z80's own space"
            ));
            return 0xFF;
        }
        if top == 0xA1 && address & 0xFFFF < 0x20 {
            let t = dev.z80_access_time();
            return dev.io.read(address, t);
        }
        if (0x80..0xA0).contains(&top) {
            dev.lock_up(format!(
                "Z80 bank access to ${address:06X} (32X area) without a 32X"
            ));
            return 0xFF;
        }
        0xFF
    }

    fn z80_bank_write(&mut self, address: u32, value: u8) {
        let top = address >> 16;
        let dev = &mut self.dev;
        if top >= 0xE0 {
            dev.ram[(address & 0xFFFF) as usize] = value;
        } else if top < 0x40 {
            dev.cart_write_byte(address, value);
        } else if top >= 0xC0 {
            if address & 0xE700E0 != 0xC00000 {
                dev.lock_up(format!(
                    "Z80 bank access to ${address:06X}: outside the VDP's pattern"
                ));
                return;
            }
            let t = dev.z80_access_time();
            self.vdp.write_byte(&mut self.dev, address & 0x1F, value, t);
        } else if top == 0xA0 {
            dev.lock_up(format!(
                "Z80 bank access to ${address:06X}: the Z80's own space"
            ));
        } else if top == 0xA1 {
            let t = dev.z80_access_time();
            if !dev.system_write_devices(address, value, value, false, t) {
                dev.lock_up(format!(
                    "Z80 write to ${address:06X} through its bank window: the bus request \
                     and reset registers from the Z80 are not supported by this port"
                ));
            }
        } else if (0x80..0xA0).contains(&top) {
            dev.lock_up(format!(
                "Z80 bank access to ${address:06X} (32X area) without a 32X"
            ));
        }
    }

    // -- the probe (`System.z80_probe`'s bus) --------------------------------------------------

    fn probe_read(&mut self, address: u16) -> u8 {
        let dev = &mut self.dev;
        let mut probe = dev.probe.take().expect("probing");
        let value = if let Some(&value) = probe.written.get(&address) {
            value
        } else if address < 0x4000 {
            dev.z80_ram[usize::from(address & 0x1FFF)]
        } else if address >= 0x8000 && (dev.bank | u32::from(address & 0x7FFF)) < 0x400000 {
            dev.cart_read_byte(dev.bank | u32::from(address & 0x7FFF))
        } else {
            0xFF
        };
        if probe.fetching && address == probe.next_fetch {
            probe.clock += if probe.after_prefix { 4 } else { 3 };
            probe.after_prefix = matches!(value, 0xCB | 0xDD | 0xED | 0xFD) && probe.after_prefix;
            probe.next_fetch = address.wrapping_add(1);
        } else {
            probe.fetching = false;
            probe.offsets.push(probe.clock);
            probe.clock += 3;
            if address >= 0x8000 || (0x7F00..0x7F20).contains(&address) {
                let start = *probe.offsets.last().expect("pushed");
                probe.bus_ends.push((probe.clock, start));
                probe
                    .bus_cart
                    .push(address >= 0x8000 && (dev.bank | u32::from(address & 0x7FFF)) < 0x800000);
            }
        }
        probe.ends.push(probe.clock);
        dev.probe = Some(probe);
        value
    }

    fn probe_write(&mut self, address: u16, value: u8) {
        let dev = &mut self.dev;
        let probe = dev.probe.as_mut().expect("probing");
        probe.fetching = false;
        probe.written.insert(address, value);
        probe.offsets.push(probe.clock);
        probe.clock += 3;
        if address >= 0x8000 || (0x7F00..0x7F20).contains(&address) {
            let start = *probe.offsets.last().expect("pushed");
            probe.bus_ends.push((probe.clock, start));
            probe
                .bus_cart
                .push(address >= 0x8000 && (dev.bank | u32::from(address & 0x7FFF)) < 0x800000);
        }
        probe.ends.push(probe.clock);
    }
}

impl z80_rust::Bus for Board {
    fn read_byte(&mut self, addr: u16) -> u8 {
        if self.dev.probe.is_some() {
            return self.probe_read(addr);
        }
        self.z80_read(addr)
    }

    fn write_byte(&mut self, addr: u16, value: u8) {
        if self.dev.probe.is_some() {
            return self.probe_write(addr, value);
        }
        self.z80_write(addr, value);
    }

    /// Nothing is on the Z80's I/O bus: z80-python's default reads $FF.
    fn read_port(&mut self, _addr: u16) -> u8 {
        0xFF
    }

    /// ... and discards writes.
    fn write_port(&mut self, _addr: u16, _value: u8) {}
}
