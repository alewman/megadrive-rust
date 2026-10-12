//! The lockstep trace both boards write (docs/handoff-brief.md, "The gate").
//!
//! `scripts/md_trace.py` writes the same text from megadrive-python at the pin, so two
//! traces of one run compare with `diff` or `cmp`. A frame is five to eight lines,
//! written after each `run_frame()`:
//!
//! ```text
//! F frame vdp-frame mclk z80-mclk 68000-instructions z80-instructions deferred
//! C the 68000's state (capture_state), its clock
//! Z the Z80's state (capture_state)
//! M CRC-32 of work RAM, Z80 RAM, VRAM, CRAM, VSRAM, the battery
//! V the VDP's registers and every field of its state
//! P the picture: its size, the CRC-32 of its CRAM indices and of its RGB
//! S the FM's and the PSG's samples since the last frame: count and CRC-32
//! B the board's clocks: the bank, the bus request, the two refreshes, the arbiter
//! ```
//!
//! and with `--steps A:B` one `T` line for every 68000 step whose index is in [A, B),
//! for narrowing a difference down. The first line is `# megadrive-trace 1`.
//!
//! CRC-32 is zlib's; integers are packed little-endian (the FM's samples as i32 pairs,
//! the PSG's as f64 pairs, CRAM and VSRAM as u16, the RGB picture as u32).

use std::fmt::Write as _;
use std::io;

use crate::cartridge::{load_image, Cartridge};
use crate::io::{Device, BUTTONS};
use crate::machine::machine;
use crate::system::System;

pub const TRACE_HEADER: &str = "# megadrive-trace 1";

pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (n, entry) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *entry = c;
        }
        table
    });
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn b(value: bool) -> u8 {
    u8::from(value)
}

fn opt(value: Option<u16>) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:X}"))
}

/// The one-line state of a 68000 step (`T`), after the step.
pub fn step_line(system: &System, index: u64, elapsed: i64) -> String {
    let cpu = system.m68k.capture_state();
    let md = &system.m68k.bus;
    let mut line = format!(
        "T {index} {} {} {elapsed} pc={:06X} sr={:04X}",
        md.mclk, md.z80_mclk, cpu.pc, cpu.sr
    );
    for d in cpu.d {
        let _ = write!(line, " {d:X}");
    }
    for a in cpu.a {
        let _ = write!(line, " {a:X}");
    }
    let _ = write!(line, " zpc={:04X}", md.z80.pc);
    line
}

/// The lines of one frame (`F` to `B`), draining both sound chips' samples.
pub fn frame_lines(system: &mut System, index: u64) -> String {
    let mut out = String::new();
    let cpu = system.m68k.capture_state();
    let system_audio = &mut system.audio;
    let md = &mut system.m68k.bus;
    let z80 = md.z80.capture_state();
    let _ = writeln!(
        out,
        "F {index} {} {} {} {} {} {}",
        md.z80.bus.vdp.s.frame,
        md.mclk,
        md.z80_mclk,
        md.m68k_instructions,
        md.z80_instructions,
        md.z80_deferred
    );
    let _ = write!(
        out,
        "C pc={:06X} sr={:04X} ir={:04X} irc={:04X} d=",
        cpu.pc, cpu.sr, cpu.ir, cpu.irc
    );
    let d: Vec<String> = cpu.d.iter().map(|v| format!("{v:X}")).collect();
    let a: Vec<String> = cpu.a.iter().map(|v| format!("{v:X}")).collect();
    let _ = writeln!(
        out,
        "{} a={} usp={:X} ssp={:X} ipl={} nmi={} tr={} stop={} halt={} clk={}",
        d.join(","),
        a.join(","),
        cpu.usp,
        cpu.ssp,
        cpu.ipl,
        b(cpu.nmi_edge),
        b(cpu.trace_pending),
        b(cpu.stopped),
        b(cpu.halted),
        cpu.clock
    );
    let _ = writeln!(
        out,
        "Z pc={:04X} sp={:04X} af={:02X}{:02X} bc={:02X}{:02X} de={:02X}{:02X} hl={:02X}{:02X} \
         ix={:04X} iy={:04X} wz={:04X} i={:02X} r={:02X} iff={}{} im={} alt={:04X},{:04X},{:04X},{:04X} \
         q={:02X} halt={} ei={} rst={} int={} nmi={}",
        z80.pc,
        z80.sp,
        z80.a,
        z80.f,
        z80.b,
        z80.c,
        z80.d,
        z80.e,
        z80.h,
        z80.l,
        z80.ix,
        z80.iy,
        z80.wz,
        z80.i,
        z80.r,
        b(z80.iff1),
        b(z80.iff2),
        z80.im,
        z80.af_alt,
        z80.bc_alt,
        z80.de_alt,
        z80.hl_alt,
        z80.q,
        b(z80.halted),
        z80.ei_delay,
        b(z80.reset_pending),
        z80.maskable_interrupt_vector
            .map_or_else(|| "-".to_string(), |v| format!("{v:02X}")),
        b(z80.non_maskable_interrupt_pending)
    );
    let board = &mut md.z80.bus;
    let s = &board.vdp.s;
    let dev = &board.dev;
    let words =
        |values: &[u16]| -> Vec<u8> { values.iter().flat_map(|v| v.to_le_bytes()).collect() };
    let _ = writeln!(
        out,
        "M ram={:08X} zram={:08X} vram={:08X} cram={:08X} vsram={:08X} bat={:08X}",
        crc32(&dev.ram),
        crc32(&dev.z80_ram),
        crc32(&s.vram),
        crc32(&words(&s.cram)),
        crc32(&words(&s.vsram)),
        crc32(&dev.cartridge.save_bytes())
    );
    let mut regs = String::new();
    for r in s.registers {
        let _ = write!(regs, "{r:02X}");
    }
    let fifo: Vec<String> = s
        .fifo
        .iter()
        .zip(s.fifo_ready)
        .map(|(e, ready)| {
            format!(
                "{:X}:{:X}:{:X}:{}{}:{}",
                e.code,
                e.address,
                e.data,
                b(e.half),
                b(e.pending),
                ready
            )
        })
        .collect();
    let columns: Vec<String> = (0..40).map(|i| opt(s.column_before_at(i))).collect();
    let _ = writeln!(
        out,
        "V regs={regs} code={:X} addr={:X} oaddr={:X} pend={} mod={} fifo={} fr={} fw={} \
         rb={:X} ra={} rh={} ready={} fill={} early={},{} dma={}{}{}:{:X}:{}:{} \
         ints={}{}{}{} odd={} hint={} hv={:X} irq={},{} dbg={:X} hold={} h={} vm={:X} il={}{} \
         v={:X} pv={:X} border={}{} ls={} mclk={} slots={}:{} done={}{} at={},{},{},{},{} \
         vsl={:X},{:X} vsb={},{} col={}",
        s.code,
        s.address,
        s.original_address,
        b(s.pending),
        b(s.modified),
        fifo.join(","),
        s.fifo_read,
        s.fifo_write,
        s.read_buffer,
        b(s.read_available),
        b(s.read_half),
        s.read_ready_at,
        s.read_fill_after,
        s.read_early.0,
        s.read_early.1,
        b(s.fill_running),
        b(s.transfer_active),
        b(s.transfer_cached),
        s.transfer_value,
        s.transfer_next_read,
        s.read_inc,
        b(s.vint_pending),
        b(s.hint_pending),
        b(s.exint_pending),
        b(s.f_flag),
        b(s.odd),
        s.hint_counter,
        s.hv_latched,
        s.z80_irq_start,
        s.z80_irq_end,
        s.debug_register,
        s.hold,
        if s.hmode.name == crate::vdp::HName::H40 {
            40
        } else {
            32
        },
        s.vmode.last,
        b(s.interlace),
        b(s.interlace_double),
        s.v,
        s.previous_v,
        b(s.border),
        b(s.border_started_now()),
        s.line_start,
        s.mclk,
        match s.slots {
            crate::vdp::Slots::Active => "a",
            crate::vdp::Slots::FirstIdle => "f",
            crate::vdp::Slots::Idle => "i",
        },
        s.slot_index,
        b(s.vint_done_now()),
        b(s.odd_done_now()),
        s.line_end,
        s.hblank_at,
        s.vint_at,
        s.odd_at,
        s.render_at,
        s.vscroll_latch[0],
        s.vscroll_latch[1],
        opt(s.vscroll_before_at(0)),
        opt(s.vscroll_before_at(1)),
        columns.join(",")
    );
    let (width, height, rows) = board.vdp.renderer.frame(s);
    let mut pixels = Vec::with_capacity(height * 320);
    for row in rows {
        pixels.extend_from_slice(row);
    }
    let rgb: Vec<u8> = board
        .vdp
        .renderer
        .frame_rgb(s)
        .iter()
        .flat_map(|row| row.iter().flat_map(|p| p.to_le_bytes()))
        .collect();
    let _ = writeln!(
        out,
        "P {width}x{height} pix={:08X} rgb={:08X}",
        crc32(&pixels),
        crc32(&rgb)
    );
    let dev = &mut board.dev;
    let fm = dev.fm.take_samples();
    let fm_bytes: Vec<u8> = fm
        .iter()
        .flat_map(|(l, r)| l.to_le_bytes().into_iter().chain(r.to_le_bytes()))
        .collect();
    let psg = dev.psg.take_samples();
    let psg_bytes: Vec<u8> = psg
        .iter()
        .flat_map(|(l, r)| l.to_le_bytes().into_iter().chain(r.to_le_bytes()))
        .collect();
    // The analogue path (`audio`), fed what the chips finished, as `take_audio` does.
    let mono: Vec<f64> = psg.iter().map(|&(left, _)| left).collect();
    let mixed = system_audio.mix(&fm, &mono);
    let mixed_bytes: Vec<u8> = mixed
        .iter()
        .flat_map(|(l, r)| l.to_le_bytes().into_iter().chain(r.to_le_bytes()))
        .collect();
    let _ = writeln!(
        out,
        "S fm={}:{:08X} psg={}:{:08X} mix={}:{:08X} fmclk={},{} psgclk={},{}",
        fm.len(),
        crc32(&fm_bytes),
        psg.len(),
        crc32(&psg_bytes),
        mixed.len(),
        crc32(&mixed_bytes),
        dev.fm.mclk,
        dev.fm.cycles_run,
        dev.psg_mclk,
        dev.psg.clock
    );
    let hold = dev
        .z80_hold
        .map_or_else(|| "-".to_string(), |(a, h)| format!("{a},{h}"));
    let partial = md_partial(system.m68k.bus.z80_partial);
    let dev = &system.m68k.bus.z80.bus.dev;
    let _ = writeln!(
        out,
        "B bank={:X} zres={} breq={} busack={} cart={} pulse={} ztaken={} ram={} pend={} resume={} \
         end={} tmss={:X},{} banks={},{},{} zhold={hold} zpart={partial}",
        dev.bank,
        b(dev.z80_reset),
        b(dev.z80_bus_requested),
        dev.busack_at,
        dev.cart_refresh_due,
        dev.cart_refresh_pulse_end,
        dev.z80_refresh_taken,
        dev.ram_refresh_due,
        dev.m68k_pending_stall,
        dev.m68k_resume_at,
        dev.m68k_step_end,
        dev.tmss_register,
        b(dev.tmss_cart_selected),
        dev.bank_accesses,
        dev.bank_z80_cycles,
        dev.bank_m68k_cycles
    );
    out
}

fn md_partial(partial: Option<(i64, i64)>) -> String {
    partial.map_or_else(|| "-".to_string(), |(a, b)| format!("{a},{b}"))
}

/// Buttons held on port 1 from frame `from` up to (not including) `to`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Press {
    pub buttons: u16,
    pub from: u64,
    pub to: u64,
}

impl Press {
    /// `BUTTONS@FROM:TO`, buttons by name, comma-separated.
    pub fn parse(spec: &str) -> Result<Press, String> {
        let (names, range) = spec.split_once('@').ok_or("--press BUTTONS@FROM:TO")?;
        let (from, to) = range.split_once(':').ok_or("--press BUTTONS@FROM:TO")?;
        let mut buttons = 0;
        for name in names.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            let (_, bit) = BUTTONS
                .iter()
                .find(|(button, _)| *button == name.to_lowercase())
                .ok_or_else(|| format!("unknown button {name:?}"))?;
            buttons |= bit;
        }
        Ok(Press {
            buttons,
            from: from.parse().map_err(|_| "bad frame")?,
            to: to.parse().map_err(|_| "bad frame")?,
        })
    }
}

/// One run, as `md-trace` and `scripts/md_trace.py` take it.
#[derive(Clone, Debug)]
pub struct Run {
    pub cartridge: std::path::PathBuf,
    pub machine: String,
    pub frames: u64,
    pub presses: Vec<Press>,
    pub resets: Vec<u64>,
    pub no_fm: bool,
    pub tmss_rom: Option<std::path::PathBuf>,
    pub pads: [String; 2],
    pub steps: Option<(u64, u64)>,
}

impl Run {
    pub fn new(cartridge: impl Into<std::path::PathBuf>) -> Self {
        Run {
            cartridge: cartridge.into(),
            machine: String::from("genesis"),
            frames: 60,
            presses: Vec::new(),
            resets: Vec::new(),
            no_fm: false,
            tmss_rom: None,
            pads: [String::from("3button"), String::from("3button")],
            steps: None,
        }
    }

    /// Parse `md-trace`'s options (everything but the cartridge and `--out`).
    pub fn option(
        &mut self,
        name: &str,
        value: &mut dyn FnMut() -> Result<String, String>,
    ) -> Result<(), String> {
        match name {
            "--machine" => self.machine = value()?,
            "--frames" => self.frames = value()?.parse().map_err(|_| "bad --frames")?,
            "--press" => self.presses.push(Press::parse(&value()?)?),
            "--reset-at" => self
                .resets
                .push(value()?.parse().map_err(|_| "bad --reset-at")?),
            "--no-fm" => self.no_fm = true,
            "--tmss-rom" => self.tmss_rom = Some(value()?.into()),
            "--pad" => self.pads[0] = value()?,
            "--pad2" => self.pads[1] = value()?,
            "--steps" => {
                let spec = value()?;
                let (a, b) = spec.split_once(':').ok_or("--steps A:B")?;
                self.steps = Some((
                    a.parse().map_err(|_| "bad --steps")?,
                    b.parse().map_err(|_| "bad --steps")?,
                ));
            }
            _ => return Err(format!("unknown argument {name}")),
        }
        Ok(())
    }

    /// Run it and write the trace.
    pub fn trace(&self, out: &mut impl io::Write) -> Result<(), String> {
        let io_err = |e: io::Error| e.to_string();
        let machine = machine(&self.machine).ok_or(format!("unknown machine {}", self.machine))?;
        let image = load_image(&self.cartridge)?;
        let digest = sha1_smol::Sha1::from(&image).digest().to_string();
        let cartridge = Cartridge::from_image(image);
        let slot = cartridge.slot.clone();
        let pads = [
            Device::by_name(&self.pads[0])?,
            Device::by_name(&self.pads[1])?,
        ];
        let boot = match &self.tmss_rom {
            Some(path) => Some(load_image(path)?),
            None => None,
        };
        let mut system = System::new(machine, Some(cartridge), Some(pads), boot);
        if self.no_fm {
            system.board().dev.fm.enabled = false;
        }
        writeln!(
            out,
            "{TRACE_HEADER}\n# {digest} slot={slot} machine={}",
            self.machine
        )
        .map_err(io_err)?;
        let mut step_index = 0u64;
        for frame in 0..self.frames {
            let held = self
                .presses
                .iter()
                .filter(|p| p.from <= frame && frame < p.to)
                .fold(0, |held, p| held | p.buttons);
            if let Some(device) = &mut system.board().dev.io.ports[0].device {
                device.set_pressed(held);
            }
            if self.resets.contains(&frame) {
                system.press_reset(None);
            }
            let start = system.board().vdp.s.frame;
            while system.board().vdp.s.frame == start {
                match system.step() {
                    Ok(elapsed) => {
                        if let Some((a, b)) = self.steps {
                            if (a..b).contains(&step_index) {
                                writeln!(out, "{}", step_line(&system, step_index, elapsed))
                                    .map_err(io_err)?;
                            }
                        }
                        step_index += 1;
                    }
                    Err(lockup) => {
                        writeln!(out, "L {frame} {lockup}").map_err(io_err)?;
                        return out.flush().map_err(io_err);
                    }
                }
            }
            let mclk = system.mclk();
            system.board().dev.sound_sync(mclk);
            out.write_all(frame_lines(&mut system, frame).as_bytes())
                .map_err(io_err)?;
        }
        out.flush().map_err(io_err)
    }
}
