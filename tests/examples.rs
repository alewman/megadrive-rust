//! The example cartridges (`tests/examples/`, programs of our own written by
//! `scripts/make_examples.py`) against the traces megadrive-python wrote for them at
//! the pin: the whole board, end to end, without any ROM or Python.

use std::path::Path;

use megadrive_rust::trace::Run;

fn check(name: &str, cartridge: &str, frames: u64, options: &[(&str, &str)]) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples");
    let mut run = Run::new(dir.join(cartridge));
    run.frames = frames;
    for (option, value) in options {
        let mut value = Some(value.to_string()).into_iter();
        run.option(option, &mut || {
            value.next().ok_or_else(|| "no value".to_string())
        })
        .unwrap();
    }
    let mut ours = Vec::new();
    run.trace(&mut ours).unwrap();
    let ours = String::from_utf8(ours).unwrap();
    let theirs = std::fs::read_to_string(dir.join(format!("{name}.trace"))).unwrap();
    // The header names the cartridge by path-independent digest; compare every line.
    for (number, (a, b)) in ours.lines().zip(theirs.lines()).enumerate() {
        assert_eq!(a, b, "{name}: line {} differs", number + 1);
    }
    assert_eq!(
        ours.lines().count(),
        theirs.lines().count(),
        "{name}: length"
    );
}

#[test]
fn board_tour() {
    check("board-tour", "board-tour.bin", 120, &[]);
}

#[test]
fn board_tour_on_the_die_figures_machine() {
    check(
        "board-tour-nuked",
        "board-tour.bin",
        120,
        &[("--machine", "nuked-md")],
    );
}

#[test]
fn h32_sprites_pal() {
    check(
        "h32-sprites",
        "h32-sprites.bin",
        90,
        &[("--machine", "megadrive")],
    );
}

#[test]
fn h32_raster() {
    check("h32-raster", "h32-raster.bin", 60, &[]);
}
