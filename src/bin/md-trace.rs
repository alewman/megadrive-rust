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

use megadrive_rust::trace::Run;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut run: Option<Run> = None;
    let mut options: Vec<(String, Option<String>)> = Vec::new();
    let mut out_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        if arg == "--out" {
            out_path = Some(PathBuf::from(args.next().ok_or("--out needs a value")?));
        } else if arg == "--no-fm" {
            options.push((arg, None));
        } else if arg.starts_with("--") {
            let value = args.next().ok_or(format!("{arg} needs a value"))?;
            options.push((arg, Some(value)));
        } else if run.is_none() {
            run = Some(Run::new(arg));
        } else {
            return Err(format!("unknown argument {arg}"));
        }
    }
    let mut run = run.ok_or("md-trace CART [options]")?;
    for (name, value) in options {
        let mut value = value.into_iter();
        run.option(&name, &mut || {
            value.next().ok_or(format!("{name} needs a value"))
        })?;
    }
    let mut out: Box<dyn Write> = match out_path {
        Some(path) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        )),
        None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
    };
    run.trace(&mut out)
}
