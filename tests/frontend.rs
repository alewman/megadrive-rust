//! The desktop frontend, as megadrive-python's `tests/test_frontend.py`.
//!
//! Its conversions need no window and are tested everywhere. The window runs headless
//! under SDL's dummy drivers with the `frontend` feature
//! (`cargo test --features frontend`); CI builds without it.

use megadrive_rust::frontend::{
    frame_bytes, machine_for, sound_samples, stamp, where_, CONTROLLER, KEYS,
};
use megadrive_rust::io::BUTTONS;

#[test]
fn frame_bytes_are_opaque_bgra() {
    let mut want = vec![0x56, 0x34, 0x12, 0xFF];
    want.extend([0xFF; 4]);
    assert_eq!(frame_bytes(&[vec![0x123456, 0xFFFFFF]]), want);
}

#[test]
fn sound_samples_interleave_and_clamp() {
    assert_eq!(
        sound_samples(&[(100, -100), (40_000, -40_000)]),
        [100, -100, 32767, -32768]
    );
}

fn header(country: &str) -> Vec<u8> {
    let mut image = vec![0u8; 0x200];
    image[0x1F0..0x1F0 + country.len()].copy_from_slice(country.as_bytes());
    image
}

#[test]
fn the_machine_follows_the_headers_country_codes() {
    assert_eq!(machine_for(&header("JUE")), "genesis"); // the USA's NTSC console first
    assert_eq!(machine_for(&header("E")), "megadrive");
    assert_eq!(machine_for(&header("J")), "megadrive-japan");
    assert_eq!(machine_for(&header("4")), "genesis"); // a later code the Manual lacks
    assert_eq!(machine_for(b""), "genesis");
}

#[test]
fn every_pad_button_has_a_key() {
    let mut keys: Vec<u16> = KEYS.iter().map(|(button, _)| *button).collect();
    let mut buttons: Vec<u16> = BUTTONS.iter().map(|(_, bit)| *bit).collect();
    keys.sort_unstable();
    buttons.sort_unstable();
    assert_eq!(keys, buttons);
    assert_eq!(CONTROLLER.len(), 8); // the d-pad comes from the controller's own
}

#[test]
fn the_title_bar_names_the_frame_and_the_emulated_time() {
    assert_eq!(where_(6800, 49.70), "frame 6,800 (2:16)");
    assert_eq!(where_(0, 59.92), "frame 0 (0:00)");
    assert_eq!(where_(1_234_567, 59.92), "frame 1,234,567 (343:23)");
}

#[test]
fn screenshots_are_stamped_in_utc() {
    assert_eq!(stamp(0), "19700101-000000");
    assert_eq!(stamp(1_791_806_400 + 3723), "20261012-130203");
}

/// The window, the loop, the pads and a change of mode, headless. One test, as SDL is
/// initialised once per process.
#[cfg(feature = "frontend")]
#[test]
fn the_window_runs_headless_and_survives_a_resolution_change() {
    use megadrive_rust::play::{Frontend, Options};

    std::env::set_var("SDL_VIDEODRIVER", "dummy");
    std::env::set_var("SDL_AUDIODRIVER", "dummy");
    // A cartridge that loops for ever: the window, the loop and the pads, not a game.
    let mut rom = vec![0u8; 0x10000];
    rom[0..4].copy_from_slice(&0x00FF_FE00u32.to_be_bytes());
    rom[4..8].copy_from_slice(&0x0000_0200u32.to_be_bytes());
    rom[0x100..0x110].copy_from_slice(b"SEGA GENESIS    ");
    rom[0x200..0x202].copy_from_slice(&[0x60, 0xFE]); // bra.s *
    let path = std::env::temp_dir().join(format!("md-play-loop-{}.bin", std::process::id()));
    std::fs::write(&path, &rom).unwrap();
    let sdl = sdl2::init().unwrap();
    let mut frontend = Frontend::new(
        &sdl,
        Options {
            rom: path.clone(),
            machine: String::from("genesis"),
            scale: 1,
            audio: true, // the dummy driver: the audio-paced path runs
            fullscreen: false,
            fm: true,
            skip: 0,
            pads: [String::from("3button"), String::from("3button")],
            save: None,
            tmss_rom: None,
        },
    )
    .unwrap();
    frontend.run(Some(3)).unwrap();
    frontend.fast = true; // Tab held: five console frames to each shown
    let before = frontend.system.board().vdp.s.frame;
    frontend.run(Some(2)).unwrap();
    frontend.fast = false;
    assert_eq!(frontend.system.board().vdp.s.frame - before, 10);
    assert_eq!(
        frontend.frame_size,
        (frontend.frame[0].len() as u32, frontend.frame.len() as u32)
    );
    frontend.apply_inputs(); // no key held: the pad reads nothing pressed
    let pad = frontend.system.board().dev.io.ports[0]
        .device
        .as_ref()
        .unwrap();
    assert_eq!(pad.pressed(), 0);
    // H32/H40, V28/V30 or interlace changing mid-game resizes the window.
    for (width, height) in [(256, 224), (352, 240), (320, 224), (320, 448), (256, 224)] {
        frontend.frame = vec![vec![0; width]; height];
        frontend.draw().unwrap();
        assert_eq!(frontend.frame_size, (width as u32, height as u32));
    }
    frontend.close().unwrap();
    let _ = std::fs::remove_file(&path);
}
