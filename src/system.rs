//! A whole console: m68000-rust's 68000 and z80-rust's Z80 on a [`Board`].
//!
//! Transcribed from megadrive-python's `system.py`, whose module docstring explains the
//! scheduler: one clock, in master clocks (MCLK) from power-on; the 68000's loop drives
//! it, each of its bus accesses placed at its own clock inside the instruction; any
//! stall a device imposes is added to that step; and the Z80 runs in the 68000's wake,
//! catching up before the 68000 changes anything it can observe, and before each 68000
//! bus cycle. The VDP and the sound chips are lazy.
//!
//! Rust's differences (docs/handoff-brief.md, "The shape Rust forces"): the reference's
//! `System` is split in two. [`Md`] is the 68000's bus: it owns the Z80 (whose bus is
//! the [`Board`]) and every piece of the scheduler's state, and it is where the board's
//! callbacks into the system (`catch_up_z80`, `z80_busack`, `on_z80_reset`,
//! `on_z80_resume`) and the board's 68000-side memory map live, since they need the
//! Z80 and the board at once. [`System`] owns the 68000 (`M68000<Md>`) and the audio
//! path, and steps them.

use std::collections::HashMap;
use std::sync::Arc;

use m68000_rust::{Bus as M68kBus, BusError, Cycle, AUTOVECTOR, M68000};
use z80_rust::timing::{instruction_timing, AccessKind};
use z80_rust::Z80;

use crate::arbiter::{
    busack_grant, z80_access_wait, AS_LOW_MCLK, BUSACK_PROPAGATION_MCLK, CUTOFF_MCLK,
};
use crate::audio::AudioPath;
use crate::board::{
    on_edge, ram_refresh, ram_refresh_self_runs, Board, Devices, Lockup, Probe, CART_REFRESH_MCLK,
    CART_REFRESH_PULSE_END_MCLK, CART_REFRESH_SELF_MCLK, CART_REFRESH_STALL,
    RAM_REFRESH_TIE_AFTER_SELF_RUNS, RAM_REFRESH_TIE_MCLK, Z80_CATCH_UP_EACH_CYCLE,
    Z80_REFRESH_LATE, Z80_REFRESH_STALL, ZRES_PIN_MCLK,
};
use crate::cartridge::Cartridge;
use crate::io::Device;
use crate::machine::{Machine, M68K_DIVIDER, Z80_DIVIDER};
use ym2612_rust::chip::{IC_SLOT_LAG, IC_WRITE_SLOT_LAG};

/// The longest Z80 step in T-states (an interrupt entry in IM 2 is 19, the longest
/// instruction 23).
pub const MAX_Z80_T: i64 = 23;

/// 68000 clocks the reset button holds the 68000 in reset: "68k RESET pulse is asserted
/// for 128 VCLK" (Eke, forum t=1262).
pub const RESET_PULSE_CLOCKS: i64 = 128;

/// The reset button restarts the 68000's refresh: a **supported finding, not a proven
/// model** (Nuked-MD, Overdrive 2 from power-on; docs/worklog.md, "The reset exception,
/// the console model, and the refresh contradiction"), adopted by Aubrey.
pub const RESET_RESTARTS_REFRESH: bool = true;
pub const RESET_RAM_REFRESH_MCLK: i64 = 2380;

/// The Z80's clock edges against the VDP's, mod 15 MCLK, in the board's time (the
/// die's relation; docs/worklog.md, "The Z80 after a hold", "The VDPs' offset").
pub const Z80_CLOCK_PHASE: i64 = 14;
/// Where the 68000's clock edges fall against the VDP's, mod 7 MCLK.
pub const M68K_POWER_ON_PHASE: i64 = 4;
/// The E clock's phase at power-on, in 68000 clocks.
pub const M68K_E_CLOCK_POWER_ON: u64 = 7;
/// How far past the 68000's write to $A11100 the Z80 takes a bus request: /BUSREQ falls
/// 18 MCLK after the write begins and the Z80 samples it 15 before a cycle's end
/// (docs/worklog.md, "The bus request, sampled"). Die-derived.
pub const Z80_BUSREQ_LAG_MCLK: i64 = 33;
/// A Z80 instruction left for a later catch-up has its accesses over the 68000's bus
/// before the 68000's clock arbitrated at their own time (docs/worklog.md, "Each access
/// arbitrated at its own time"). Die-derived.
pub const Z80_ARBITRATE_PER_ACCESS: bool = true;
/// A hold the arbiter owes the 68000 from a Z80 access settled after its instruction is
/// a time its next bus cycle waits for (docs/worklog.md, "The grant over the 68000's
/// own clocks"). Die-derived.
pub const OWED_AT_FIRST_CYCLE: bool = true;
/// /BUSAK falls this long after the Z80's last machine cycle ends, its wait states
/// included (Nuked-MD, all 29 grants of the probe). False keeps the 57.
pub const Z80_BUSACK_AT_CYCLE_END: bool = true;
pub const Z80_BUSACK_AFTER_MCLK: i64 = 3;
pub const Z80_RESUME_MCLK: i64 = 42;
pub const Z80_RESET_RESUME_MCLK: i64 = 57;
/// A bus request made while the Z80 is held in reset is granted only after its reset is
/// released and the M1 cycle it starts with has run (docs/worklog.md, "A second
/// title"). Die-derived.
pub const Z80_BUSACK_AFTER_RESET: bool = true;

/// What a stopped run reports: an access that locks up a real console, or a Z80
/// instruction the core does not execute.
pub type StepResult = Result<i64, Lockup>;

/// The 68000's bus: the Z80 and its board, and the scheduler's state.
pub struct Md {
    pub z80: Z80<Board>,
    /// The 68000's time (MCLK) at the start of its next step.
    pub mclk: i64,
    /// The Z80's time (MCLK) at the start of its next step.
    pub z80_mclk: i64,
    in_z80: bool,
    pub m68k_instructions: u64,
    /// Access offsets by instruction bytes (`z80_offsets`).
    z80_timings: HashMap<[u8; 4], Arc<[i64]>>,
    pub z80_instructions: u64,
    /// Instructions the Z80 left for later because the 68000 took its bus in the
    /// middle of them.
    pub z80_deferred: u64,
    /// The instruction the Z80 is part way through while its bus is held: its start and
    /// the clock from which its remaining accesses wait.
    pub z80_partial: Option<(i64, i64)>,
    /// Clocks of this instruction's holds already added to the 68000's clock.
    stall_credited: i64,
    /// The clocks the acknowledge just credited, for the core to collect.
    acknowledge_credit: u64,
    /// The reset button has been pressed and the 68000 not yet reset, and when.
    pub reset_pressed: bool,
    pub reset_at: i64,
    /// The last probe's machine cycles over the 68000's bus: (end, start) in T-states,
    /// and whether each is to the cartridge.
    probe_bus_ends: Vec<(i64, i64)>,
    probe_bus_cart: Vec<bool>,
}

impl Md {
    pub fn board(&mut self) -> &mut Board {
        &mut self.z80.bus
    }

    pub fn dev(&mut self) -> &mut Devices {
        &mut self.z80.bus.dev
    }

    fn note(&mut self, cycle: Cycle) {
        let dev = &mut self.z80.bus.dev;
        dev.m68k_irc = cycle.irc;
        dev.m68k_step_clocks = cycle.step_clocks;
    }

    // -- the CPUs' interrupt lines ---------------------------------------------------------

    pub fn z80_reset_line(&mut self, held: bool) {
        if held {
            self.z80_partial = None;
            self.z80.bus.dev.z80_hold = None;
            // The reset state is applied at once; the Z80 is not stepped while held.
            let z80 = &mut self.z80;
            z80.request_reset();
            let _ = z80.step();
            z80.clear_reset();
            // RESET "clears the Program Counter and registers I and R" (Zilog UM0080,
            // "Reset"); the reference's board does it itself, as its comment explains.
            z80.i = 0;
            z80.r = 0;
        }
    }

    // -- running ----------------------------------------------------------------------------

    /// Run the Z80 until its time reaches `mclk` (no-op inside a Z80 step). `stopping`:
    /// the caller is about to take the bus from the Z80 or reset it at `mclk`, and an
    /// instruction under way whose accesses are not all started by the time BUSREQ is
    /// sampled is left for when the Z80 has the bus again (Zilog UM0080).
    pub fn catch_up_z80(&mut self, mclk: i64, stopping: bool) {
        if self.in_z80 {
            return;
        }
        self.in_z80 = true;
        while self.z80_mclk < mclk {
            if !self.z80.bus.dev.z80_running() {
                if self.z80_partial.is_none() {
                    self.z80_mclk = mclk;
                }
                break;
            }
            let sampled = if stopping {
                mclk + Z80_BUSREQ_LAG_MCLK
            } else {
                mclk
            };
            if stopping && self.z80_mclk + MAX_Z80_T * Z80_DIVIDER >= sampled {
                let offsets = self.z80_offsets(); // the table's, as the horizon below
                if offsets.iter().any(|&t| self.z80_mclk + t >= sampled) {
                    // The machine cycle under way when the request is sampled finishes;
                    // the accesses after it wait for the bus (`z80_resume`).
                    let start = self.z80_mclk;
                    let ends = self.z80_machine_cycle_ends();
                    let let_go = ends
                        .iter()
                        .map(|t| start + t * Z80_DIVIDER)
                        .find(|&end| end >= sampled)
                        .unwrap_or(sampled);
                    self.z80_partial = Some((self.z80_mclk, let_go));
                    self.z80_deferred += 1;
                    break;
                }
            }
            let irq = self.z80.bus.vdp.s.z80_irq_at(self.z80_mclk);
            let offsets = if irq {
                self.z80.request_maskable_interrupt(0xFF);
                // z80-rust's acceptance test (`can_accept_maskable_interrupt`).
                if self.z80.iff1 && self.z80.capture_state().ei_delay == 0 {
                    // The acknowledge takes this step; the table times instructions.
                    None
                } else {
                    Some(self.z80_offsets())
                }
            } else {
                self.z80.clear_maskable_interrupt();
                Some(self.z80_offsets())
            };
            self.z80.bus.dev.z80_offsets = offsets.clone();
            if let Some(offsets) = &offsets {
                let last = *offsets.last().expect("an instruction accesses memory");
                if self.z80_mclk + last >= mclk && self.z80.bus.dev.z80_hold.is_none() {
                    // An access of this instruction would fall past the 68000's clock,
                    // where the arbiter cannot yet see the 68000's bus: it runs at the next
                    // catch-up. Its accesses over the 68000's bus before that are settled
                    // now.
                    if Z80_ARBITRATE_PER_ACCESS && self.z80.bus.dev.arbiter_enabled {
                        self.prearbitrate(mclk);
                    }
                    break;
                }
            }
            let dev = &mut self.z80.bus.dev;
            dev.z80_access = 0;
            dev.z80_bus_access = 0;
            dev.step_start = self.z80_mclk;
            dev.z80_stall = 0;
            let hold = dev.z80_hold.map_or(0, |(_, held)| held);
            let t_states = match self.z80.step() {
                Ok(t_states) => i64::from(t_states),
                Err(fault) => {
                    self.z80.bus.dev.lock_up(format!("Z80: {fault}"));
                    0
                }
            };
            let dev = &mut self.z80.bus.dev;
            self.z80_mclk += t_states * Z80_DIVIDER + dev.z80_stall + hold;
            dev.z80_hold = None;
            dev.z80_prearbitrated = None;
            self.z80_instructions += 1;
            if dev.lockup.is_some() {
                break;
            }
        }
        self.in_z80 = false;
        self.z80.bus.dev.step_start = self.mclk;
    }

    /// The Z80's next instruction is left for a later catch-up, but its accesses over
    /// the 68000's bus that begin before `mclk` are arbitrated now, at their own time;
    /// the instruction takes their waits when it runs (`Devices::bank_delay`).
    fn prearbitrate(&mut self, mclk: i64) {
        let dev = &self.z80.bus.dev;
        let Some(offsets) = dev.z80_offsets.clone() else {
            return; // an interrupt's acceptance
        };
        if dev.z80_hold.is_some() || self.z80_partial.is_some() {
            return; // a held or split instruction
        }
        let start = self.z80_mclk;
        let fresh = !matches!(&dev.z80_prearbitrated, Some(done) if done.0 == start);
        if fresh {
            // Nothing to settle unless an access past the opcode's own fetches comes
            // before mclk, or the code runs from the 68000's side of the bus.
            let before = offsets.iter().filter(|&&at| start + at < mclk).count();
            let pc = self.z80.pc;
            let first = self.z80.bus.dev.z80_peek(pc);
            let fetched = if matches!(first, 0xCB | 0xDD | 0xED | 0xFD) {
                2
            } else {
                1
            };
            if before <= fetched && pc < 0x4000 {
                return;
            }
            self.z80_probe();
            let mut bus: Vec<(i64, bool)> = self
                .probe_bus_ends
                .iter()
                .map(|&(_end, at)| at)
                .zip(self.probe_bus_cart.iter().copied())
                .collect();
            bus.sort_unstable();
            self.z80.bus.dev.z80_prearbitrated = Some((start, Vec::new(), bus));
        }
        let dev = &mut self.z80.bus.dev;
        let (mut waits, bus) = {
            let done = dev.z80_prearbitrated.as_mut().expect("set above");
            (std::mem::take(&mut done.1), done.2.clone())
        };
        let mut extra = waits.iter().map(|&(wait, _)| wait).sum::<i64>() * Z80_DIVIDER;
        for &(at, cartridge) in &bus[waits.len()..] {
            // The probe gives the machine cycle's first T-state; the access-timing table
            // puts the access a T-state on.
            let t = start + (at + 1) * Z80_DIVIDER + extra;
            if t >= mclk {
                break; // as an instruction runs only with every access before mclk
            }
            let settled = dev.arbitrate(t, cartridge);
            waits.push(settled);
            extra += settled.0 * Z80_DIVIDER;
        }
        dev.z80_prearbitrated.as_mut().expect("set above").1 = waits;
    }

    /// The 68000's write at `t` has given the Z80 its bus back, or released its reset.
    /// An instruction left part way through resumes with its remaining accesses moved
    /// to that edge on; otherwise the next instruction's opcode fetch falls there.
    pub fn z80_resume(&mut self, t: i64, reset: bool) {
        let wait = if reset {
            Z80_RESET_RESUME_MCLK
        } else {
            Z80_RESUME_MCLK
        };
        let edge = t + wait + (Z80_CLOCK_PHASE - t - wait).rem_euclid(Z80_DIVIDER);
        let dev = &mut self.z80.bus.dev;
        if reset && dev.z80_bus_requested {
            // The opcode fetch, T1 of a four-T-state M1 cycle, before the bus goes.
            let start = edge - Z80_DIVIDER;
            self.z80_mclk = start;
            self.z80_partial = Some((start, start + 4 * Z80_DIVIDER));
            if Z80_BUSACK_AFTER_RESET && dev.arbiter_enabled {
                // The grant asked for during the reset comes after that M1 cycle.
                dev.busack_at = dev
                    .busack_at
                    .max(start + 4 * Z80_DIVIDER + Z80_BUSACK_AFTER_MCLK);
            }
            return;
        }
        let Some((_start, let_go)) = self.z80_partial.take() else {
            self.z80_mclk = self.z80_mclk.max(edge - Z80_DIVIDER);
            return;
        };
        // Everything after the machine cycle it let go at moves by the same amount, so
        // the T-state that follows comes at the edge (docs/worklog.md "The bus holds").
        dev.z80_hold = Some((let_go, (edge - Z80_DIVIDER - let_go).max(0)));
    }

    /// Where each memory access of the Z80's next instruction falls, in master clocks
    /// from its start: z80-rust's access-timing table, read at PC with no side effects
    /// and kept by the instruction's first four bytes.
    fn z80_offsets(&mut self) -> Arc<[i64]> {
        let pc = self.z80.pc;
        let dev = &mut self.z80.bus.dev;
        let key = [
            dev.z80_peek(pc),
            dev.z80_peek(pc.wrapping_add(1)),
            dev.z80_peek(pc.wrapping_add(2)),
            dev.z80_peek(pc.wrapping_add(3)),
        ];
        self.z80_timings
            .entry(key)
            .or_insert_with(|| {
                let timing = instruction_timing(&key);
                timing
                    .accesses
                    .iter()
                    .filter(|a| matches!(a.kind, AccessKind::Read | AccessKind::Write))
                    .map(|a| i64::from(a.t) * Z80_DIVIDER)
                    .collect()
            })
            .clone()
    }

    /// The master clock /BUSAK reaches the 68000, for a request made at `mclk`: the Z80
    /// lets go at the end of the machine cycle in progress (Zilog UM0080), the arbiter
    /// then takes its own time to report it (fitted to Nuked-MD).
    pub fn z80_busack(&mut self, mclk: i64) -> i64 {
        self.catch_up_z80(mclk, true);
        let sampled = mclk + Z80_BUSREQ_LAG_MCLK;
        let running = self.z80.bus.dev.z80_running();
        if !running || !Z80_BUSACK_AT_CYCLE_END {
            if self.z80_mclk >= mclk || !running {
                return mclk.max(self.z80_mclk) + BUSACK_PROPAGATION_MCLK;
            }
            let start = self.z80_mclk;
            let ends: Vec<i64> = self
                .z80_machine_cycle_ends()
                .iter()
                .map(|t| start + t * Z80_DIVIDER)
                .collect();
            return busack_grant(sampled, &ends);
        }
        if self.z80_mclk >= sampled && self.z80_partial.is_none() {
            // The instruction the request fell in has run to its end.
            return self.z80_mclk + Z80_BUSACK_AFTER_MCLK;
        }
        if let Some((_, let_go)) = self.z80_partial {
            return let_go + Z80_BUSACK_AFTER_MCLK;
        }
        let start = self.z80_mclk;
        let mut ends: Vec<i64> = self
            .z80_machine_cycle_ends()
            .iter()
            .map(|t| start + t * Z80_DIVIDER)
            .collect();
        // A machine cycle over the 68000's bus is as long as the arbiter makes it: its
        // wait states push it and every later one back.
        let dev = &self.z80.bus.dev;
        for &(end_t, at_t) in &self.probe_bus_ends {
            let (wait, _, _) =
                z80_access_wait(start + at_t * Z80_DIVIDER, &dev.m68k_accesses, None);
            let extra = wait * Z80_DIVIDER;
            let cut = start + end_t * Z80_DIVIDER;
            for end in &mut ends {
                if *end >= cut {
                    *end += extra;
                }
            }
        }
        let let_go = ends
            .iter()
            .copied()
            .find(|&e| e >= sampled)
            .unwrap_or(sampled);
        let_go + Z80_BUSACK_AFTER_MCLK
    }

    /// Where the next Z80 step's machine cycles end, in T-states from its start.
    pub fn z80_machine_cycle_ends(&mut self) -> Vec<i64> {
        self.z80_probe().2
    }

    /// When the Z80's next step would start each of its data accesses, in T-states from
    /// the step's start: an estimate, with no effect on the machine. The step is run
    /// once on the probe bus and the CPU's state restored; returns the offsets, the
    /// writes the step would make, and where each machine cycle ends.
    pub fn z80_probe(&mut self) -> (Vec<i64>, HashMap<u16, u8>, Vec<i64>) {
        let state = self.z80.capture_state();
        self.z80.bus.dev.probe = Some(Probe {
            fetching: true,
            next_fetch: self.z80.pc,
            after_prefix: true, // the first byte is an M1 fetch
            ..Probe::default()
        });
        let total = match self.z80.step() {
            Ok(total) => i64::from(total),
            Err(fault) => {
                self.z80.bus.dev.lock_up(format!("Z80: {fault}"));
                0
            }
        };
        let probe = self.z80.bus.dev.probe.take().expect("probing");
        self.z80.restore_state(&state);
        self.probe_bus_ends = probe.bus_ends;
        self.probe_bus_cart = probe.bus_cart;
        let mut ends: Vec<i64> = probe.ends.iter().copied().filter(|&e| e < total).collect();
        ends.push(total);
        ends.sort_unstable();
        ends.dedup();
        (probe.offsets, probe.written, ends)
    }

    /// The console's reset button: not a power cycle (forum t=1262: VRES# to the 68000
    /// and the cartridge, ZRES held until the game releases it; SRES# only on a cold
    /// boot). ZRES is asserted now; the 68000 is held and reset at its next step. `at`:
    /// the master clock the button is pressed at, if not now.
    pub fn press_reset(&mut self, at: Option<i64>) {
        self.reset_at = match at {
            None => self.mclk,
            Some(at) => at.max(self.mclk),
        };
        self.set_z80_reset(true, self.reset_at, None, false);
        self.reset_pressed = true;
    }

    // -- the board's side that needs the Z80 ----------------------------------------------

    /// Drive ZRES, the Z80's and YM2612's shared reset line, at MCLK `t`: from a write to
    /// $A11200 (`write`), or from the console's reset button. `fm_t`: when the YM2612's
    /// /IC sees it, if not at `t`.
    pub fn set_z80_reset(&mut self, held: bool, t: i64, fm_t: Option<i64>, write: bool) {
        let running = self.z80.bus.dev.z80_running();
        self.catch_up_z80(t, held && running);
        let dev = &mut self.z80.bus.dev;
        if held != dev.z80_reset {
            dev.z80_reset = held;
            let fm_t = fm_t.unwrap_or(t);
            dev.sound_sync(fm_t);
            dev.fm.reset_line(
                held,
                Some(fm_t),
                if write {
                    IC_WRITE_SLOT_LAG
                } else {
                    IC_SLOT_LAG
                },
            );
            self.z80_reset_line(held);
            if !held {
                self.z80_resume(t, true);
            }
        }
    }

    /// One 68000 bus cycle about to start at `address`: its refresh stalls, and its place
    /// in the grid the arbiter reads.
    fn bus_cycle(&mut self, address: u32) {
        let dev = &mut self.z80.bus.dev;
        let mut t = dev.m68k_access_clock();
        let grid = dev.arbiter_enabled;
        dev.grant_wait = 0;
        if dev.m68k_resume_at != 0 {
            // A grant to the Z80 settled after the last instruction (`System::step`).
            if dev.m68k_resume_at > t {
                let waited = dev.m68k_resume_at - t;
                dev.stall += waited;
                t += waited;
                dev.grant_wait = waited;
            }
            dev.m68k_resume_at = 0;
        }
        if grid {
            dev.m68k_accesses.push([t, AS_LOW_MCLK]);
            if Z80_CATCH_UP_EACH_CYCLE && dev.z80_running() {
                // The Z80's accesses that win the bus before this cycle, and what they
                // hold it for; one asked for within the cut-off loses to it.
                self.catch_up_z80(t - CUTOFF_MCLK, false);
                let dev = &mut self.z80.bus.dev;
                let owed = dev.m68k_pending_stall;
                if owed != 0 {
                    dev.m68k_pending_stall = 0;
                    dev.stall += owed;
                    t += owed;
                }
                // The 68000 drives /AS only once it has the bus back.
                dev.grant_wait += owed;
            }
        }
        let Board { vdp, dev } = &mut self.z80.bus;
        if t >= dev.cart_refresh_due {
            let late = t - dev.cart_refresh_due;
            dev.cart_refresh_due +=
                ((t - dev.cart_refresh_due).div_euclid(CART_REFRESH_MCLK) + 1) * CART_REFRESH_MCLK;
            let mut stretch = 0;
            if late >= CART_REFRESH_SELF_MCLK + CART_REFRESH_STALL {
                // it ran by itself while the bus was free
            } else if address < 0x800000 {
                stretch = CART_REFRESH_STALL;
                dev.cart_refresh_pulse_end = t - late + CART_REFRESH_PULSE_END_MCLK;
                if late >= CART_REFRESH_SELF_MCLK {
                    // Under way when the access came: it waits for the end, on the
                    // 68000's clock.
                    let left = CART_REFRESH_SELF_MCLK + CART_REFRESH_STALL - late;
                    stretch = -((-left).div_euclid(M68K_DIVIDER)) * M68K_DIVIDER;
                }
            } else if address >> 16 == 0xA0 && late >= Z80_REFRESH_LATE {
                stretch = Z80_REFRESH_STALL;
            }
            if stretch != 0 {
                dev.stall += stretch;
                t += stretch;
                if grid {
                    if let Some(last) = dev.m68k_accesses.last_mut() {
                        last[1] += stretch;
                    }
                }
            }
        } else if address >> 16 == 0xA0 && t < dev.cart_refresh_pulse_end {
            // Started under the /REF pulse of the refresh that stretched the access before.
            dev.stall += Z80_REFRESH_STALL;
            if grid {
                if let Some(last) = dev.m68k_accesses.last_mut() {
                    last[1] += Z80_REFRESH_STALL;
                }
            }
        }
        if address >= 0xE00000 {
            let mut due = dev.ram_refresh_due;
            if RAM_REFRESH_TIE_AFTER_SELF_RUNS {
                due = ram_refresh_self_runs(due, t);
            }
            let tie = due - t == RAM_REFRESH_TIE_MCLK && vdp.dot_clock_edge(dev, t);
            let (wait, next) = ram_refresh(due, t, tie);
            dev.ram_refresh_due = next;
            if wait != 0 {
                dev.stall += wait;
                if grid {
                    if let Some(last) = dev.m68k_accesses.last_mut() {
                        last[1] += wait;
                    }
                }
            }
        }
    }

    /// A byte of the Z80's space, or None when the 68000 does not hold the bus
    /// ("reading $A00000-A0FFFF will return the MSB of the next instruction to be
    /// fetched, and the LSB will be set to zero. Writes are ignored.", CMD 2.2).
    fn z80_area_read(&mut self, address: u32) -> Option<u8> {
        let t = self.z80.bus.dev.m68k_access_clock();
        if !self.z80.bus.dev.m68k_has_z80_bus(Some(t)) {
            return None;
        }
        self.catch_up_z80(t, false);
        self.z80.bus.dev.z80_area_byte(address, t)
    }

    fn z80_area_write(&mut self, address: u32, value: u8) {
        let t = self.z80.bus.dev.m68k_access_clock();
        if !self.z80.bus.dev.m68k_has_z80_bus(Some(t)) {
            return;
        }
        self.catch_up_z80(t, false);
        self.z80.bus.dev.z80_area_store(address, value, t);
    }

    fn system_read_word(&mut self, address: u32) -> u16 {
        let t = self.z80.bus.dev.m68k_access_clock();
        if let Some(value) = self.z80.bus.dev.system_read_devices(address, t) {
            return value;
        }
        self.catch_up_z80(t, false);
        self.z80.bus.dev.busreq_read(t)
    }

    /// A write to $A1xxxx: `high` is what D15-D8 carry, `low` D7-D0.
    fn system_write(&mut self, address: u32, high: u8, low: u8, word: bool) {
        let t = self.z80.bus.dev.m68k_access_clock();
        if self
            .z80
            .bus
            .dev
            .system_write_devices(address, high, low, word, t)
        {
            return;
        }
        let offset = address & 0xFFFF;
        if offset & 0xFF00 == 0x1100 {
            let request = high & 1 != 0;
            let dev = &self.z80.bus.dev;
            let running = dev.z80_running();
            let busack_at = if !dev.arbiter_enabled {
                self.catch_up_z80(t, request && running);
                t
            } else if request && running {
                // BUSACK: the Z80 samples BUSREQ at the end of the machine cycle it is
                // in and lets go after it; the system probes it for that clock.
                self.z80_busack(t)
            } else {
                // Held in reset or already let go: no machine cycle to wait for, but
                // the arbiter still takes its propagation to answer.
                let at = t + BUSACK_PROPAGATION_MCLK;
                self.catch_up_z80(t, false);
                at
            };
            let dev = &mut self.z80.bus.dev;
            dev.busack_at = busack_at;
            let released = dev.z80_bus_requested && !request && !dev.z80_reset;
            dev.z80_bus_requested = request;
            if released {
                self.z80_resume(t, false);
            }
        } else if offset & 0xFF00 == 0x1200 {
            self.set_z80_reset(high & 1 == 0, t, Some(t + ZRES_PIN_MCLK), true);
        }
    }

    // -- the 68000's bus --------------------------------------------------------------------

    fn read_word_68k(&mut self, address: u32) -> u16 {
        if self.z80.bus.dev.refresh_enabled {
            self.bus_cycle(address);
        }
        let top = address >> 16;
        let Board { vdp, dev } = &mut self.z80.bus;
        if top < 0x40 {
            return dev.cart_read_word(address);
        }
        if top >= 0xE0 {
            let offset = (address & 0xFFFF) as usize;
            return u16::from(dev.ram[offset]) << 8 | u16::from(dev.ram[offset + 1]);
        }
        if top >= 0xC0 {
            if !dev.vdp_access(address) {
                return 0;
            }
            let t = dev.m68k_access_clock();
            let value = vdp.read_word(dev, address & 0x1F, t);
            dev.stall += on_edge(vdp.s.hold);
            return value;
        }
        if top == 0xA0 {
            let value = self.z80_area_read(address);
            let dev = &mut self.z80.bus.dev;
            dev.z80_space_cost();
            return match value {
                Some(value) => u16::from(value) | u16::from(value) << 8,
                None => dev.open_bus_msb(),
            };
        }
        if top == 0xA1 {
            return self.system_read_word(address);
        }
        if (0x80..0xA0).contains(&top) {
            dev.lock_up(format!(
                "68000 read of ${address:06X} (32X area) without a 32X"
            ));
            return 0;
        }
        dev.open_bus_msb()
    }

    fn read_byte_68k(&mut self, address: u32) -> u8 {
        if self.z80.bus.dev.refresh_enabled {
            self.bus_cycle(address);
        }
        let top = address >> 16;
        let Board { vdp, dev } = &mut self.z80.bus;
        if top < 0x40 {
            return dev.cart_read_byte(address);
        }
        if top >= 0xE0 {
            return dev.ram[(address & 0xFFFF) as usize];
        }
        if top >= 0xC0 {
            if !dev.vdp_access(address) {
                return 0;
            }
            let t = dev.m68k_access_clock();
            let value = vdp.read_byte(dev, address & 0x1F, t);
            dev.stall += on_edge(vdp.s.hold);
            return value;
        }
        if top == 0xA0 {
            let value = self.z80_area_read(address);
            let dev = &mut self.z80.bus.dev;
            dev.z80_space_cost();
            return match value {
                Some(value) => value,
                None if address & 1 == 0 => (dev.open_bus() >> 8) as u8,
                None => 0,
            };
        }
        if top == 0xA1 {
            let word = self.system_read_word(address & !1);
            return if address & 1 != 0 {
                word as u8
            } else {
                (word >> 8) as u8
            };
        }
        if (0x80..0xA0).contains(&top) {
            dev.lock_up(format!(
                "68000 read of ${address:06X} (32X area) without a 32X"
            ));
            return 0;
        }
        if address & 1 != 0 {
            0
        } else {
            (dev.open_bus() >> 8) as u8
        }
    }

    fn write_word_68k(&mut self, address: u32, value: u16) {
        if self.z80.bus.dev.refresh_enabled {
            self.bus_cycle(address);
        }
        let top = address >> 16;
        let Board { vdp, dev } = &mut self.z80.bus;
        if top >= 0xE0 {
            let offset = (address & 0xFFFF) as usize;
            dev.ram[offset] = (value >> 8) as u8;
            dev.ram[offset + 1] = value as u8;
        } else if top < 0x40 {
            dev.cart_write_word(address, value);
        } else if top >= 0xC0 {
            if dev.vdp_access(address) {
                let t = dev.m68k_access_clock();
                let held = vdp.write_word(dev, address & 0x1F, value, t);
                dev.stall += on_edge(held);
            }
        } else if top == 0xA0 {
            // "When doing word-wide writes to Z80 RAM, only the MSB is written" (CMD 1.2).
            self.z80_area_write(address & !1, (value >> 8) as u8);
            self.z80.bus.dev.z80_space_cost();
        } else if top == 0xA1 {
            self.system_write(address, (value >> 8) as u8, value as u8, true);
        } else if (0x80..0xA0).contains(&top) {
            dev.lock_up(format!(
                "68000 write to ${address:06X} (32X area) without a 32X"
            ));
        }
    }

    fn write_byte_68k(&mut self, address: u32, value: u8) {
        if self.z80.bus.dev.refresh_enabled {
            self.bus_cycle(address);
        }
        let top = address >> 16;
        let Board { vdp, dev } = &mut self.z80.bus;
        if top >= 0xE0 {
            dev.ram[(address & 0xFFFF) as usize] = value;
        } else if top < 0x40 {
            dev.cart_write_byte(address, value);
        } else if top >= 0xC0 {
            // "Writing to the VDP control or data ports is interpreted as a 16-bit
            // write, with the LSB duplicated in the MSB" (CMD 1.2).
            if dev.vdp_access(address) {
                let t = dev.m68k_access_clock();
                let held = vdp.write_byte(dev, address & 0x1F, value, t);
                dev.stall += on_edge(held);
            }
        } else if top == 0xA0 {
            self.z80_area_write(address, value);
            self.z80.bus.dev.z80_space_cost();
        } else if top == 0xA1 {
            // The 68000 drives a byte on both halves of the data bus (UM 5.1.2).
            self.system_write(address, value, value, false);
        } else if (0x80..0xA0).contains(&top) {
            dev.lock_up(format!(
                "68000 write to ${address:06X} (32X area) without a 32X"
            ));
        }
    }
}

impl M68kBus for Md {
    fn read_byte(&mut self, address: u32, cycle: Cycle) -> Result<u8, BusError> {
        self.note(cycle);
        Ok(self.read_byte_68k(address))
    }

    fn read_word(&mut self, address: u32, cycle: Cycle) -> Result<u16, BusError> {
        self.note(cycle);
        Ok(self.read_word_68k(address))
    }

    fn write_byte(&mut self, address: u32, value: u8, cycle: Cycle) -> Result<(), BusError> {
        self.note(cycle);
        self.write_byte_68k(address, value);
        Ok(())
    }

    fn write_word(&mut self, address: u32, value: u16, cycle: Cycle) -> Result<(), BusError> {
        self.note(cycle);
        self.write_word_68k(address, value);
        Ok(())
    }

    /// The 68000's interrupt-acknowledge cycle: the VDP clears the request it answers;
    /// every level is autovectored (MAME's `cpu_space_map` as a detector; the Manual,
    /// PDF page 19). The acknowledge waits for the E clock, which the core reads from
    /// its clock count: the holds of this instruction so far are brought into it
    /// (Nuked-MD; docs/worklog.md, "The E clock after a held push").
    fn acknowledge(&mut self, level: u8, cycle: Cycle) -> i32 {
        self.note(cycle);
        self.z80.bus.vdp.acknowledge(level);
        let credit = self.z80.bus.dev.stall.div_euclid(M68K_DIVIDER) - self.stall_credited;
        self.stall_credited += credit;
        self.acknowledge_credit = credit as u64;
        AUTOVECTOR
    }

    fn acknowledge_clock_credit(&mut self) -> u64 {
        std::mem::take(&mut self.acknowledge_credit)
    }

    /// TAS's write half: the Mega Drive's bus never completes it (m68000-python
    /// docs/undocumented-behavior.md; MAME `megadriv_tas_callback`).
    fn tas_write(&mut self, _address: u32, _value: u8, cycle: Cycle) -> Result<(), BusError> {
        self.note(cycle);
        Ok(())
    }
}

/// Machine + cartridge, ready to run.
pub struct System {
    pub machine: Machine,
    pub m68k: M68000<Md>,
    /// The analogue path the two sound chips meet in.
    pub audio: AudioPath,
}

impl System {
    /// `pads` None is the reference's default, two 3-button pads.
    pub fn new(
        machine: Machine,
        cartridge: Option<Cartridge>,
        pads: Option<[Option<Device>; 2]>,
        tmss_rom: Option<Vec<u8>>,
    ) -> Self {
        let mut board = Board::new(machine, cartridge, pads);
        if let Some(image) = tmss_rom {
            board.dev.install_tmss_rom(image);
        }
        let md = Md {
            z80: Z80::new(board),
            mclk: 0,
            z80_mclk: 0,
            in_z80: false,
            m68k_instructions: 0,
            z80_timings: HashMap::new(),
            z80_instructions: 0,
            z80_deferred: 0,
            z80_partial: None,
            stall_credited: 0,
            acknowledge_credit: 0,
            reset_pressed: false,
            reset_at: 0,
            probe_bus_ends: Vec::new(),
            probe_bus_cart: Vec::new(),
        };
        let mut system = System {
            machine,
            m68k: M68000::new(md),
            audio: AudioPath::new(machine, 44_100),
        };
        // Power-on: the 68000 takes its reset vector; the Z80 is held in reset.
        system.m68k.clock = M68K_E_CLOCK_POWER_ON;
        // The YM2612 runs on the 68000's clock (MCLK/7): its edges are the 68000's.
        system.m68k.bus.z80.bus.dev.fm.mclk = M68K_POWER_ON_PHASE;
        let clocks = i64::from(system.m68k.reset());
        system.mirror();
        system.m68k.bus.mclk = M68K_POWER_ON_PHASE + clocks * M68K_DIVIDER;
        system.m68k.bus.z80_reset_line(true);
        system
    }

    pub fn md(&mut self) -> &mut Md {
        &mut self.m68k.bus
    }

    pub fn board(&mut self) -> &mut Board {
        &mut self.m68k.bus.z80.bus
    }

    pub fn mclk(&self) -> i64 {
        self.m68k.bus.mclk
    }

    /// The 68000's IRC and step clocks, as the reference's board reads them between
    /// steps.
    fn mirror(&mut self) {
        let (irc, clocks) = (self.m68k.irc, self.m68k.step_clocks());
        let dev = &mut self.m68k.bus.z80.bus.dev;
        dev.m68k_irc = irc;
        dev.m68k_step_clocks = clocks;
    }

    fn stopped(&mut self) -> Result<(), Lockup> {
        match self.m68k.bus.z80.bus.dev.lockup.clone() {
            Some(lockup) => Err(lockup),
            None => Ok(()),
        }
    }

    /// The console's reset button (`Md::press_reset`).
    pub fn press_reset(&mut self, at: Option<i64>) {
        self.m68k.bus.press_reset(at);
    }

    /// One 68000 instruction, then the Z80 up to the same time; returns MCLK.
    pub fn step(&mut self) -> StepResult {
        self.stopped()?;
        if self.m68k.bus.reset_pressed {
            // The 68000 is held in reset for the pulse, the rest of the machine running
            // on, then takes the reset exception; its bus cycles come after the pulse,
            // stamped at their own clocks (Nuked-MD; docs/worklog.md "The reset
            // exception").
            let md = &mut self.m68k.bus;
            md.reset_pressed = false;
            let start = md.mclk;
            let Board { vdp, dev } = &mut md.z80.bus;
            vdp.sync(dev, start);
            // Held from the press, on the 68000's next clock edge at or after it.
            let held = -((start - md.reset_at).div_euclid(M68K_DIVIDER)) * M68K_DIVIDER;
            self.m68k.clock += (RESET_PULSE_CLOCKS + held / M68K_DIVIDER) as u64;
            let dev = &mut self.m68k.bus.z80.bus.dev;
            dev.step_start = start + held + RESET_PULSE_CLOCKS * M68K_DIVIDER;
            if RESET_RESTARTS_REFRESH {
                dev.cart_refresh_due = start + held + CART_REFRESH_MCLK;
                dev.ram_refresh_due = start + held + RESET_RAM_REFRESH_MCLK;
            }
            dev.stall = 0;
            // What the arbiter charged the 68000 for the Z80's bank accesses before the
            // press is spent in the pulse.
            dev.m68k_pending_stall = 0;
            dev.m68k_accesses.clear();
            let clocks = i64::from(self.m68k.reset());
            self.mirror();
            let md = &mut self.m68k.bus;
            let elapsed =
                held + (RESET_PULSE_CLOCKS + clocks) * M68K_DIVIDER + md.z80.bus.dev.stall;
            md.mclk = start + elapsed;
            let mclk = md.mclk;
            md.catch_up_z80(mclk, false);
            self.stopped()?;
            return Ok(elapsed);
        }
        let md = &mut self.m68k.bus;
        let dev = &mut md.z80.bus.dev;
        let owed = dev.m68k_pending_stall;
        if owed != 0 && OWED_AT_FIRST_CYCLE && Z80_CATCH_UP_EACH_CYCLE && dev.arbiter_enabled {
            // The bus is the Z80's until then: the 68000's first bus cycle waits for it.
            dev.m68k_pending_stall = 0;
            dev.m68k_resume_at = md.mclk + owed;
        } else if owed != 0 {
            // Owed for Z80 bank accesses made in the wake of the last instruction.
            dev.m68k_pending_stall = 0;
            md.mclk += owed;
            self.m68k.clock += (owed / M68K_DIVIDER) as u64; // keeps the E-clock phase in step
        }
        let md = &mut self.m68k.bus;
        let start = md.mclk;
        md.stall_credited = 0;
        let Board { vdp, dev } = &mut md.z80.bus;
        dev.step_start = start;
        dev.stall = 0;
        dev.z80_space_stall = 0;
        dev.m68k_accesses.clear();
        vdp.sync(dev, start);
        let ipl = vdp.s.ipl();
        self.m68k.set_ipl(ipl);
        let clocks = i64::from(self.m68k.step());
        self.mirror();
        let credited = self.m68k.bus.stall_credited;
        let stall = self.m68k.bus.z80.bus.dev.stall;
        let elapsed = clocks * M68K_DIVIDER + stall;
        if stall != 0 {
            // Keeps the E-clock phase in step (what an acknowledge already took aside).
            self.m68k.clock = (self.m68k.clock as i64 + stall / M68K_DIVIDER - credited) as u64;
        }
        let md = &mut self.m68k.bus;
        md.mclk = start + elapsed;
        md.m68k_instructions += 1;
        let dev = &mut md.z80.bus.dev;
        dev.m68k_step_end = md.mclk;
        self.stopped()?;
        // Not into the arbiter's cut-off before the 68000's next cycle.
        let md = &mut self.m68k.bus;
        let lead = if Z80_CATCH_UP_EACH_CYCLE && md.z80.bus.dev.arbiter_enabled {
            CUTOFF_MCLK
        } else {
            0
        };
        let target = md.mclk - lead;
        md.catch_up_z80(target, false);
        self.stopped()?;
        Ok(elapsed)
    }

    /// Run until the VDP starts its next frame, then bring the sound chips up to the
    /// same clock so their sample queues fill a frame at a time.
    pub fn run_frame(&mut self) -> Result<(), Lockup> {
        let frame = self.m68k.bus.z80.bus.vdp.s.frame;
        while self.m68k.bus.z80.bus.vdp.s.frame == frame {
            self.step()?;
        }
        let mclk = self.mclk();
        self.m68k.bus.z80.bus.dev.sound_sync(mclk);
        Ok(())
    }

    /// The stereo samples finished since the last call, at the host rate. Both chips
    /// are brought up to the 68000's clock first.
    pub fn take_audio(&mut self) -> Vec<(i32, i32)> {
        let mclk = self.mclk();
        let dev = &mut self.m68k.bus.z80.bus.dev;
        dev.sound_sync(mclk);
        // The PSG is mono here: both sides of its samples are the same level.
        let psg: Vec<f64> = dev
            .psg
            .take_samples()
            .into_iter()
            .map(|(left, _)| left)
            .collect();
        let fm = dev.fm.take_samples();
        self.audio.mix(&fm, &psg)
    }

    pub fn run_mclk(&mut self, mclk: i64) -> Result<(), Lockup> {
        let target = self.mclk() + mclk;
        while self.mclk() < target {
            self.step()?;
        }
        Ok(())
    }
}
