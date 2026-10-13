//! Play a cartridge in a window: `md-play CART [--machine NAME] [--scale 3] ...`
//! (megadrive-python's `megadrive-python-play`). Needs the `frontend` feature:
//! `cargo build --release --features frontend`.

use std::path::PathBuf;

use megadrive_rust::cartridge::load_image;
use megadrive_rust::frontend::{layout, machine_for, pad_for, HELP};
use megadrive_rust::machine::MACHINE_NAMES;
use megadrive_rust::play::{Frontend, Options};

const USAGE: &str = "md-play CART [--machine NAME] [--scale N] [--no-audio] [--no-fm] \
[--skip N] [--pad KIND] [--pad2 KIND] [--layout sega|snes] [--save FILE] \
[--tmss-rom FILE] [--fullscreen] [--frames N]";

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut rom: Option<PathBuf> = None;
    let mut machine_name: Option<String> = None;
    let (mut scale, mut skip) = (3u32, 0u32);
    let (mut audio, mut fm, mut fullscreen) = (true, true, false);
    // Port 1's pad defaults to the one the header asks for.
    let (mut pad, mut pad2) = (None, String::from("3button"));
    let mut layout_name = String::from("sega");
    let (mut save, mut tmss_rom, mut frames) = (None, None, None);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--machine" => machine_name = Some(value()?),
            "--scale" => scale = value()?.parse().map_err(|_| "bad --scale")?,
            "--no-audio" => audio = false,
            "--no-fm" => fm = false,
            "--skip" => skip = value()?.parse().map_err(|_| "bad --skip")?,
            "--pad" => pad = Some(value()?),
            "--pad2" => pad2 = value()?,
            "--layout" => layout_name = value()?,
            "--save" => save = Some(PathBuf::from(value()?)),
            "--tmss-rom" => tmss_rom = Some(PathBuf::from(value()?)),
            "--fullscreen" => fullscreen = true,
            // Quit after N frames have been shown: for checking the window runs.
            "--frames" => frames = Some(value()?.parse().map_err(|_| "bad --frames")?),
            "--help" | "-h" => {
                println!("{USAGE}\nmachines: {}\n\n{HELP}", MACHINE_NAMES.join(", "));
                return Ok(());
            }
            _ if !arg.starts_with("--") && rom.is_none() => rom = Some(PathBuf::from(arg)),
            _ => return Err(format!("unknown argument {arg}\n{USAGE}")),
        }
    }
    let rom = rom.ok_or(USAGE)?;
    layout(&layout_name)?;
    let image = load_image(&rom)?;
    let machine = machine_name.unwrap_or_else(|| machine_for(&image).to_string());
    let pads = [pad.unwrap_or_else(|| pad_for(&image).to_string()), pad2];
    if !MACHINE_NAMES.contains(&machine.as_str()) {
        return Err(format!(
            "unknown machine {machine}: one of {}",
            MACHINE_NAMES.join(", ")
        ));
    }
    let sdl = sdl2::init()?;
    println!("{HELP}");
    println!(
        "{} on {machine}, {} pad, {layout_name} layout",
        rom.file_name().unwrap_or_default().to_string_lossy(),
        pads[0]
    );
    let mut frontend = Frontend::new(
        &sdl,
        Options {
            rom,
            machine,
            scale: scale.max(1),
            audio,
            fullscreen,
            fm,
            skip,
            pads,
            layout: layout_name,
            save,
            tmss_rom,
        },
    )?;
    let result = frontend.run(frames);
    frontend.close()?;
    result
}
