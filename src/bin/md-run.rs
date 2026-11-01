//! Run a cartridge: `md-run CART --frames N --png out.png --wav out.wav`
//! (megadrive-python's `python -m megadrive_python`).

use std::path::PathBuf;
use std::time::Instant;

use megadrive_rust::cartridge::{load_image, Cartridge};
use megadrive_rust::io::{Device, BUTTONS};
use megadrive_rust::machine::MACHINE_NAMES;
use megadrive_rust::png::write_png;
use megadrive_rust::render::LEVELS;
use megadrive_rust::wav::write_wav;
use megadrive_rust::{machine, System};

const USAGE: &str = "md-run CART [--machine NAME] [--frames N] [--png FILE] [--png-dir DIR] \
[--raster-dir DIR] [--wav FILE] [--rate HZ] [--no-sound] [--press BUTTONS] [--press-at FRAME] [--pad KIND] [--pad2 KIND] \
[--save FILE] [--tmss-rom FILE] [--quiet]";

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut cart: Option<PathBuf> = None;
    let mut machine_name = String::from("genesis");
    let mut frames = 60u64;
    let (mut png, mut wav, mut save, mut tmss) = (None, None, None, None);
    let mut png_dir: Option<PathBuf> = None;
    let mut raster_dir: Option<PathBuf> = None;
    let mut rate = 44_100u32;
    let mut no_sound = false;
    let mut press = String::new();
    let mut press_at = 0u64;
    let mut pads = [String::from("3button"), String::from("3button")];
    let mut quiet = false;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--machine" => machine_name = value()?,
            "--frames" => frames = value()?.parse().map_err(|_| "bad --frames")?,
            "--png" => png = Some(PathBuf::from(value()?)),
            "--png-dir" => png_dir = Some(PathBuf::from(value()?)),
            "--raster-dir" => raster_dir = Some(PathBuf::from(value()?)),
            "--wav" => wav = Some(PathBuf::from(value()?)),
            "--rate" => rate = value()?.parse().map_err(|_| "bad --rate")?,
            "--no-sound" => no_sound = true,
            "--press" => press = value()?,
            "--press-at" => press_at = value()?.parse().map_err(|_| "bad --press-at")?,
            "--pad" => pads[0] = value()?,
            "--pad2" => pads[1] = value()?,
            "--save" => save = Some(PathBuf::from(value()?)),
            "--tmss-rom" => tmss = Some(PathBuf::from(value()?)),
            "--quiet" => quiet = true,
            "--help" | "-h" => {
                println!("{USAGE}\nmachines: {}", MACHINE_NAMES.join(", "));
                return Ok(());
            }
            _ if !arg.starts_with("--") && cart.is_none() => cart = Some(PathBuf::from(arg)),
            _ => return Err(format!("unknown argument {arg}\n{USAGE}")),
        }
    }
    let cart = cart.ok_or(USAGE)?;
    let machine = machine(&machine_name).ok_or(format!(
        "unknown machine {machine_name}: one of {}",
        MACHINE_NAMES.join(", ")
    ))?;
    // from_image: the board (a mapper, battery RAM) comes from MAME's software list.
    let mut cartridge = Cartridge::from_image(load_image(&cart)?);
    if let Some(path) = &save {
        if path.exists() {
            cartridge.load_bytes(&std::fs::read(path).map_err(|e| e.to_string())?);
        }
    }
    let boot = match &tmss {
        Some(path) => Some(load_image(path)?),
        None => None,
    };
    let pads = [Device::by_name(&pads[0])?, Device::by_name(&pads[1])?];
    let mut system = System::new(machine, Some(cartridge), Some(pads), boot);
    system.audio.sample_rate = rate;
    if no_sound {
        system.board().dev.fm.enabled = false;
    }
    let mut pressed = 0;
    for name in press.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        let (_, bit) = BUTTONS
            .iter()
            .find(|(button, _)| *button == name.to_lowercase())
            .ok_or_else(|| format!("unknown button {name:?}"))?;
        pressed |= bit;
    }
    if let Some(dir) = &png_dir {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    if let Some(dir) = &raster_dir {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        system.board().vdp.enable_raster();
    }
    // The lines above line 0, which lead the next frame on the screen.
    let mut above: Vec<(usize, Vec<u32>)> = Vec::new();
    let started = Instant::now();
    let mut audio = Vec::new();
    for frame in 0..frames {
        if pressed != 0 && frame == press_at {
            if let Some(pad) = &mut system.board().dev.io.ports[0].device {
                pad.set_pressed(pressed);
            }
        }
        if let Err(lockup) = system.run_frame() {
            eprintln!("frame {frame}: {lockup}");
            break;
        }
        if let Some(dir) = &png_dir {
            // Every frame as `fNNNNN.png`, the VDP's count since power-on: the numbering
            // of megadrive-python's `scripts/board_frames.py`.
            let vdp = &system.board().vdp;
            let rows = vdp.renderer.frame_rgb(&vdp.s);
            if !rows.is_empty() {
                write_png(&dir.join(format!("f{:05}.png", vdp.s.frame)), &rows)
                    .map_err(|e| e.to_string())?;
            }
        }
        if let Some(dir) = &raster_dir {
            // Every frame as `rNNNNN.png`, 347 wide: lines 470-511, then 0-257. Lines no
            // border was opened over show the backdrop, as the border does.
            let vdp = &system.board().vdp;
            let lines = vdp.renderer.frame_raster();
            let backdrop = u32::from(vdp.s.registers[7] & 0x3F);
            let colour = vdp.s.cram[backdrop as usize];
            let level = |shift: u32| LEVELS[0][usize::from((colour >> shift) & 7)];
            let fill = level(1) << 16 | level(5) << 8 | level(9);
            let mut rows = vec![vec![fill; 347]; 300];
            let at = |row: usize| if row >= 470 { row - 470 } else { row + 42 };
            for (row, line) in above.iter().chain(lines.iter().filter(|l| l.0 < 258)) {
                rows[at(*row)] = line.clone();
            }
            above = lines.into_iter().filter(|l| l.0 >= 470).collect();
            write_png(&dir.join(format!("r{:05}.png", vdp.s.frame)), &rows)
                .map_err(|e| e.to_string())?;
        }
        if wav.is_some() {
            audio.extend(system.take_audio());
        } else {
            // Keep the sample queues from growing over a long run.
            let dev = &mut system.board().dev;
            dev.fm.take_samples();
            dev.psg.take_samples();
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    let dev = &mut system.board().dev;
    if let (Some(path), true) = (&save, !dev.cartridge.battery().is_empty()) {
        std::fs::write(path, dev.cartridge.save_bytes()).map_err(|e| e.to_string())?;
    }
    if let Some(path) = &wav {
        write_wav(path, &audio, rate).map_err(|e| e.to_string())?;
    }
    let vdp = &system.board().vdp;
    if let Some(path) = &png {
        write_png(path, &vdp.renderer.frame_rgb(&vdp.s)).map_err(|e| e.to_string())?;
    }
    if !quiet {
        let (width, height, _) = vdp.renderer.frame(&vdp.s);
        let md = &system.m68k.bus;
        println!(
            "{frames} frames of {machine_name}, {width}x{height}, {} 68000 and {} Z80 \
             instructions in {seconds:.2} s ({:.1} fps){}",
            md.m68k_instructions,
            md.z80_instructions,
            frames as f64 / seconds,
            if wav.is_some() {
                format!("; {} samples at {rate} Hz", audio.len())
            } else {
                String::new()
            }
        );
    }
    Ok(())
}
