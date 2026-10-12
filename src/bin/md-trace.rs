//! Run a cartridge and write the lockstep trace (`megadrive_rust::trace`).
//!
//! ```text
//! md-trace CART [--machine NAME] [--frames N] [--press BUTTONS@FROM:TO ...]
//!          [--reset-at FRAME ...] [--no-fm] [--tmss-rom PATH] [--pad KIND] [--pad2 KIND]
//!          [--steps A:B] [--out FILE]
//! ```
//!
//! `scripts/md_trace.py` takes the same arguments and runs megadrive-python.

use std::io::Write;
use std::path::PathBuf;

use megadrive_rust::cartridge::{load_image, Cartridge};
use megadrive_rust::io::{Device, BUTTONS};
use megadrive_rust::trace::{frame_lines, step_line, TRACE_HEADER};
use megadrive_rust::{machine, System};

struct Press {
    buttons: u16,
    from: u64,
    to: u64,
}

fn parse_press(spec: &str) -> Result<Press, String> {
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

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut cart: Option<PathBuf> = None;
    let mut machine_name = String::from("genesis");
    let mut frames = 60u64;
    let mut presses = Vec::new();
    let mut resets = Vec::new();
    let mut no_fm = false;
    let mut tmss: Option<PathBuf> = None;
    let mut pads = [String::from("3button"), String::from("3button")];
    let mut steps: Option<(u64, u64)> = None;
    let mut out_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--machine" => machine_name = value()?,
            "--frames" => frames = value()?.parse().map_err(|_| "bad --frames")?,
            "--press" => presses.push(parse_press(&value()?)?),
            "--reset-at" => resets.push(value()?.parse::<u64>().map_err(|_| "bad --reset-at")?),
            "--no-fm" => no_fm = true,
            "--tmss-rom" => tmss = Some(PathBuf::from(value()?)),
            "--pad" => pads[0] = value()?,
            "--pad2" => pads[1] = value()?,
            "--steps" => {
                let spec = value()?;
                let (a, b) = spec.split_once(':').ok_or("--steps A:B")?;
                steps = Some((
                    a.parse().map_err(|_| "bad --steps")?,
                    b.parse().map_err(|_| "bad --steps")?,
                ));
            }
            "--out" => out_path = Some(PathBuf::from(value()?)),
            _ if !arg.starts_with("--") && cart.is_none() => cart = Some(PathBuf::from(arg)),
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    let cart = cart.ok_or("md-trace CART [options]")?;
    let machine = machine(&machine_name).ok_or(format!("unknown machine {machine_name}"))?;
    let image = load_image(&cart)?;
    let digest = sha1_smol::Sha1::from(&image).digest().to_string();
    let cartridge = Cartridge::from_image(image);
    let slot = cartridge.slot.clone();
    let pads = [Device::by_name(&pads[0])?, Device::by_name(&pads[1])?];
    let boot = match tmss {
        Some(path) => Some(load_image(&path)?),
        None => None,
    };
    let mut system = System::new(machine, Some(cartridge), Some(pads), boot);
    if no_fm {
        system.board().dev.fm.enabled = false;
    }
    let mut out: Box<dyn Write> = match out_path {
        Some(path) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        )),
        None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
    };
    let w = |out: &mut Box<dyn Write>, text: &str| {
        out.write_all(text.as_bytes()).map_err(|e| e.to_string())
    };
    w(
        &mut out,
        &format!("{TRACE_HEADER}\n# {digest} slot={slot} machine={machine_name}\n"),
    )?;
    let mut step_index = 0u64;
    for frame in 0..frames {
        let held = presses
            .iter()
            .filter(|p| p.from <= frame && frame < p.to)
            .fold(0, |held, p| held | p.buttons);
        if let Some(device) = &mut system.board().dev.io.ports[0].device {
            device.set_pressed(held);
        }
        if resets.contains(&frame) {
            system.press_reset(None);
        }
        let start = system.m68k.bus.z80.bus.vdp.s.frame;
        let result = loop {
            if system.m68k.bus.z80.bus.vdp.s.frame != start {
                break Ok(());
            }
            match system.step() {
                Ok(elapsed) => {
                    if let Some((a, b)) = steps {
                        if (a..b).contains(&step_index) {
                            w(&mut out, &(step_line(&system, step_index, elapsed) + "\n"))?;
                        }
                    }
                    step_index += 1;
                }
                Err(lockup) => break Err(lockup),
            }
        };
        if let Err(lockup) = result {
            w(&mut out, &format!("L {frame} {lockup}\n"))?;
            break;
        }
        let mclk = system.mclk();
        system.board().dev.sound_sync(mclk);
        w(&mut out, &frame_lines(&mut system, frame))?;
    }
    out.flush().map_err(|e| e.to_string())
}
