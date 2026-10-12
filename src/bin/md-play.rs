//! Play a cartridge in a window: `md-play CART [--machine NAME] [--scale 3] ...`
//! (megadrive-python's `megadrive-python-play`). Needs the `frontend` feature:
//! `cargo build --release --features frontend`.

use std::path::PathBuf;

use megadrive_rust::cartridge::load_image;
use megadrive_rust::frontend::{machine_for, HELP};
use megadrive_rust::machine::MACHINE_NAMES;
use megadrive_rust::play::{Frontend, Options};

const USAGE: &str = "md-play CART [--machine NAME] [--scale N] [--no-audio] [--no-fm] \
[--skip N] [--pad KIND] [--pad2 KIND] [--save FILE] [--tmss-rom FILE] [--fullscreen] \
[--frames N]";

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut rom: Option<PathBuf> = None;
    let mut machine_name: Option<String> = None;
    let (mut scale, mut skip) = (3u32, 0u32);
    let (mut audio, mut fm, mut fullscreen) = (true, true, false);
    let mut pads = [String::from("3button"), String::from("3button")];
    let (mut save, mut tmss_rom, mut frames) = (None, None, None);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--machine" => machine_name = Some(value()?),
            "--scale" => scale = value()?.parse().map_err(|_| "bad --scale")?,
            "--no-audio" => audio = false,
            "--no-fm" => fm = false,
            "--skip" => skip = value()?.parse().map_err(|_| "bad --skip")?,
            "--pad" => pads[0] = value()?,
            "--pad2" => pads[1] = value()?,
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
    let machine = match machine_name {
        Some(name) => name,
        None => machine_for(&load_image(&rom)?).to_string(),
    };
    if !MACHINE_NAMES.contains(&machine.as_str()) {
        return Err(format!(
            "unknown machine {machine}: one of {}",
            MACHINE_NAMES.join(", ")
        ));
    }
    let sdl = sdl2::init()?;
    println!("{HELP}");
    println!(
        "{} on {machine}",
        rom.file_name().unwrap_or_default().to_string_lossy()
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
            save,
            tmss_rom,
        },
    )?;
    let result = frontend.run(frames);
    frontend.close()?;
    result
}
