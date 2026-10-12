//! The bus arbiter (YM6045): what the Z80 and the 68000 cost each other.
//!
//! Transcribed from megadrive-python's `arbiter.py`, whose module docstring carries the
//! sources at length: Kabuto, "The bus system" (hardware-measured research) and his
//! footnote 1's arithmetic; MoD's logic analyser on a Genesis 2 (forum t=1661;
//! hardware-captured), which judges the model; Zilog UM0080 for when the Z80 lets go
//! of its bus; and Nuked-MD (die-derived) for everything the hardware sources leave
//! open. None of this is modelled by MAME, so `Board::arbiter_enabled` turns the whole
//! of it off for the milestone-1 lockstep, as `refresh_enabled` does the refresh.

/// How long the 68000 actually holds the bus for one bus cycle, in master clocks.
///
/// A 68000 bus cycle is four clocks (28 MCLK), but the bus belongs to the 68000 only
/// while /AS is asserted (68000 UM, "Bus Arbitration"). Nuked-MD's bus log: in a
/// `jmp (a0)` loop 1,706 of its 1,766 68000 cycles hold /AS low for exactly 18 MCLK
/// with a 10-MCLK gap to the next, and the other 60 hold it 32 -- 18 plus the
/// cartridge refresh's 14 (docs/worklog.md, the timing session).
pub const AS_LOW_MCLK: i64 = 18;
/// The arbiter's own propagation, in master clocks, from the Z80 letting go of the bus
/// to /BUSAK being seen on the 68000's side: the difference of Nuked-MD's mean grant
/// (12.34 68000 cycles over 122 grants) and the machine-cycle rule's (4.15), 8.19
/// cycles rounded to 57 MCLK; **die-derived and fitted** (docs/worklog.md, the timing
/// session).
pub const BUSACK_PROPAGATION_MCLK: i64 = 57;

/// The arbitration of one Z80 access over the 68000's bus, as the die model times it
/// (Nuked-MD, all 24,343 bank-window and VDP accesses of Overdrive 2's first 80
/// frames; docs/worklog.md, "The arbiter, measured"), from the Z80's /MREQ at `r`: a
/// 68000 cycle that begins by `r` + [`CUTOFF_MCLK`] still goes first; the Z80's own
/// cycle on the 68000's bus begins [`GRANT_GAP_MCLK`] after the last of them ends and
/// no sooner than [`IDLE_GRANT_MCLK`] after `r`, on the next of the 68000's clock
/// edges; it ends on the first Z80 clock edge `r` + [`RELEASE_MCLK`] + 15w (w >= 2
/// wait states) at least [`OWN_CYCLE_MCLK`] after it began; and the 68000's next cycle
/// waits for the first of its clock edges [`RESUME_MCLK`] after that. Die-derived;
/// MoD's logic-analyser figures (hardware-captured) judge it.
pub const CUTOFF_MCLK: i64 = 21;
pub const GRANT_GAP_MCLK: i64 = 17;
pub const IDLE_GRANT_MCLK: i64 = 29;
/// 29 from 30: the Z80's wait runs to the first of its clocks at or past the end of
/// its own cycle on the 68000's bus, so a cycle a master clock too long cost a whole
/// Z80 clock (+16,155,626 and +16,099,290 both want 24-29). Die-derived;
/// docs/worklog.md, "The Z80's cycle a master clock shorter".
pub const OWN_CYCLE_MCLK: i64 = 29;
pub const RELEASE_MCLK: i64 = 37;
pub const RESUME_MCLK: i64 = 25;
pub const MIN_WAIT: i64 = 2;
pub const REFRESH_MCLK: i64 = 14;
/// Where the 68000's clock edges fall, mod 7 MCLK: where its cycles begin
/// (`system::M68K_POWER_ON_PHASE`, the die's relation in the board's time).
pub const M68K_EDGE_PHASE: i64 = 4;

/// For a Z80 access over the 68000's bus with /MREQ at `request` (MCLK), given the
/// 68000's bus cycles as `[start, span]` and the cartridge refresh still owed
/// (`refresh`, the clock it fell due, or None): the Z80's wait states, the master
/// clock from which the 68000 may start its next cycle, and when the Z80's own cycle
/// began. A refresh owed by then is taken by the Z80's own cycle, which takes
/// [`REFRESH_MCLK`] longer -- Kabuto's `b`, "The Z80 bus access itself can be delayed
/// as well by what seems to be refresh logic (up to 2 more cycles)".
pub fn z80_access_wait(
    request: i64,
    m68k_cycles: &[[i64; 2]],
    refresh: Option<i64>,
) -> (i64, i64, i64) {
    let mut start = request + IDLE_GRANT_MCLK;
    for &[begin, span] in m68k_cycles {
        if begin > request + CUTOFF_MCLK {
            break;
        }
        if begin + span > request {
            start = start.max(begin + span + GRANT_GAP_MCLK);
        }
    }
    start += (M68K_EDGE_PHASE - start).rem_euclid(7); // on one of the 68000's clock edges
    let mut length = OWN_CYCLE_MCLK;
    if let Some(refresh) = refresh {
        if refresh <= start {
            length += REFRESH_MCLK;
        }
    }
    let mut wait = MIN_WAIT;
    while request + RELEASE_MCLK + 15 * wait < start + length {
        wait += 1;
    }
    let end = request + RELEASE_MCLK + 15 * wait;
    (wait, end + RESUME_MCLK, start)
}

/// The master clock /BUSAK reaches the 68000, given the request's clock and the ends
/// of the Z80 step's machine cycles: Zilog UM0080's rule for *when* the Z80 lets go
/// (the end of the machine cycle in progress; specification tier) and the arbiter's
/// own propagation for how long the board takes to say so
/// ([`BUSACK_PROPAGATION_MCLK`]; fitted to Nuked-MD, die-derived).
pub fn busack_grant(request: i64, machine_cycle_ends: &[i64]) -> i64 {
    for &end in machine_cycle_ends {
        if end >= request {
            return end + BUSACK_PROPAGATION_MCLK;
        }
    }
    request + BUSACK_PROPAGATION_MCLK
}
