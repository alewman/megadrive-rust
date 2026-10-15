//! The desktop window's pure parts: the key and controller maps, the conversions, the
//! machine a header suggests and the title bar's position. megadrive-python's
//! `frontend.py` (milestone 9), the part of it its tests run without a window.
//!
//! The window itself is [`crate::play`], built with the `frontend` feature (SDL2);
//! these need no SDL, so `cargo test` checks them everywhere.

use crate::io::{A, B, C, DOWN, LEFT, MODE, RIGHT, START, UP, X, Y, Z};

pub const SAMPLE_RATE: u32 = 44_100;
/// Frames of sound kept queued ahead of the speaker: the latency, and the margin a
/// slow frame can use before the sound gaps.
pub const BUFFER_FRAMES: u32 = 3;

pub const HELP: &str = "\
Arrows       D-pad              Z X C / J K L   A B C          A S D   X Y Z
Enter        Start              Q               Mode           Backspace   reset button
Tab (hold)   fast forward       F3              pause          F5      power cycle
F11          fullscreen         F12             screenshot     Esc     quit
- =          volume down / up";

/// The keyboard, by pad button (SDL's key names, looked up when the window opens).
pub const KEYS: [(u16, &[&str]); 12] = [
    (UP, &["Up"]),
    (DOWN, &["Down"]),
    (LEFT, &["Left"]),
    (RIGHT, &["Right"]),
    (A, &["Z", "J"]),
    (B, &["X", "K"]),
    (C, &["C", "L"]),
    (X, &["A"]),
    (Y, &["S"]),
    (Z, &["D"]),
    (START, &["Return", "Keypad Enter"]),
    (MODE, &["Q"]),
];

/// A game controller, by pad button (SDL's controller button names), in the Mega
/// Drive's own layout: A, B, C along the bottom row as the left, bottom and right face
/// buttons, and the 6-button pad's X, Y, Z as left shoulder, the top face button and
/// right shoulder. SDL's names are positions, Xbox-style -- `a` bottom, `b` right, `x`
/// left, `y` top -- once [`crate::play`] turns off its relabelling of Nintendo pads.
pub const CONTROLLER: [(u16, &str); 8] = [
    (A, "x"),
    (B, "a"),
    (C, "b"),
    (X, "leftshoulder"),
    (Y, "y"),
    (Z, "rightshoulder"),
    (START, "start"),
    (MODE, "back"),
];

/// The Super Nintendo fighting layout, for 6-button games such as Street Fighter II:
/// the top row X, Y, Z (light, medium, heavy punch) on the left and top face buttons and
/// left shoulder, the bottom row A, B, C (the kicks) on the bottom and right face
/// buttons and right shoulder.
pub const CONTROLLER_SNES: [(u16, &str); 8] = [
    (X, "x"),
    (Y, "y"),
    (Z, "leftshoulder"),
    (A, "a"),
    (B, "b"),
    (C, "rightshoulder"),
    (START, "start"),
    (MODE, "back"),
];

/// The controller layouts `md-play --layout` takes, the default first.
pub const LAYOUTS: [(&str, &[(u16, &str); 8]); 2] =
    [("sega", &CONTROLLER), ("snes", &CONTROLLER_SNES)];

/// A controller layout by name.
pub fn layout(name: &str) -> Result<&'static [(u16, &'static str); 8], String> {
    LAYOUTS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, l)| *l)
        .ok_or(format!("unknown layout {name:?} (sega, snes)"))
}

/// Rows of 0xRRGGBB as opaque BGRA bytes (little-endian 0xFFRRGGBB words), SDL's
/// ARGB8888 on this side of the wire.
pub fn frame_bytes(frame: &[Vec<u32>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.iter().map(|row| row.len() * 4).sum());
    for &pixel in frame.iter().flatten() {
        out.extend_from_slice(&(0xFF00_0000 | pixel).to_le_bytes());
    }
    out
}

/// md-play's master volume, in dB over the board's mix (`--volume`; `-` and `=` step
/// it in the window). The mix leaves room for every channel at full volume at once,
/// which no game comes near: fourteen titles measured peak 10.5 to 23 dB below full
/// scale, so the window plays 12 dB up and lets [`sound_samples`] round off the rare
/// peak that would go over.
pub const VOLUME_DB: f64 = 12.0;
pub const VOLUME_STEP_DB: f64 = 3.0;
pub const VOLUME_RANGE_DB: (f64, f64) = (-30.0, 24.0);

/// Where the soft clamp starts bending, as a fraction of full scale (-3 dBFS).
const KNEE: f64 = 0.708;

/// Stereo pairs, as the board's audio path mixes them, raised by `volume_db` and
/// softly clamped, as interleaved signed 16-bit values for SDL. The clamp leaves all
/// below -3 dBFS as it is and bends louder values toward full scale, never past it.
/// Only the window uses this: md-run's WAVs and the traces take the mix unchanged.
pub fn sound_samples(samples: &[(i32, i32)], volume_db: f64) -> Vec<i16> {
    let gain = 10f64.powf(volume_db / 20.0);
    let shape = |v: i32| {
        let x = f64::from(v) * gain / 32768.0;
        let a = x.abs();
        let bent = if a <= KNEE {
            a
        } else {
            KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh()
        };
        (bent.copysign(x) * 32768.0)
            .round()
            .clamp(-32768.0, 32767.0) as i16
    };
    samples
        .iter()
        .flat_map(|&(left, right)| [shape(left), shape(right)])
        .collect()
}

/// The machine a cartridge's header suggests: its "Country in which the product can be
/// released" at $1F0 -- "Japan: J, USA: U, Europe: E" (the Manual, PDF page 130). An
/// NTSC Genesis if it names the USA, a PAL Mega Drive if Europe but not the USA, a
/// Japanese one if only Japan, and the Genesis for anything else (the later hexadecimal
/// codes, which the Manual does not describe).
pub fn machine_for(image: &[u8]) -> &'static str {
    let field: String = image
        .get(0x1F0..image.len().min(0x200))
        .unwrap_or(&[])
        .iter()
        .map(|&b| (b as char).to_ascii_uppercase())
        .collect();
    if field.contains('U') {
        "genesis"
    } else if field.contains('E') {
        "megadrive"
    } else if field.contains('J') {
        "megadrive-japan"
    } else {
        "genesis"
    }
}

/// The pad a cartridge's header asks for, from its "I/O support" field at $190: "J"
/// is the control pad and "6" the 6-button pad, which later games list (Super Street
/// Fighter II and Comix Zone have "J6", Mortal Kombat 3 "J64"). A 6-button pad if the
/// header names one, else the 3-button pad.
pub fn pad_for(image: &[u8]) -> &'static str {
    let field = image.get(0x190..image.len().min(0x1A0)).unwrap_or(&[]);
    if field.contains(&b'6') {
        "6button"
    } else {
        "3button"
    }
}

/// The console's frame count since power-on and the emulated time it makes, for the
/// title bar: the VDP's own count, which a reset leaves alone, and the number the
/// board's tools call a frame. (`where` in the reference; a keyword in Rust.)
pub fn where_(frame: u64, rate: f64) -> String {
    let seconds = (frame as f64 / rate) as u64;
    format!(
        "frame {} ({}:{:02})",
        thousands(frame),
        seconds / 60,
        seconds % 60
    )
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A screenshot's time stamp, `YYYYmmdd-HHMMSS` in UTC (the reference's
/// `time.strftime`, which is local time; there is no time-zone database here).
pub fn stamp(unix_seconds: u64) -> String {
    let (days, rest) = (unix_seconds / 86_400, unix_seconds % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}
