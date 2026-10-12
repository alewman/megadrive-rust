//! The serial EEPROM against the reference's, update by update
//! (`scripts/make_eeprom_vectors.py`): bit-banged writes longer than a page, reads,
//! abandoned transactions, in each of Eke's three addressing modes.

use megadrive_rust::eeprom::{SerialEEPROM, MODE1, MODE2, MODE3};

fn replay(name: &str, mode: u8, size_mask: usize, page_mask: usize) {
    let path = format!(
        "{}/tests/examples/eeprom-{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let (updates, memory) = lines.split_at(lines.len() - 1);
    let mut chip = SerialEEPROM::new(mode, size_mask, page_mask, None);
    for (number, line) in updates.iter().enumerate() {
        let v: Vec<u8> = line.split(' ').map(|x| x.parse().unwrap()).collect();
        chip.update(v[0], v[1]);
        assert_eq!(chip.read_sda(), v[2], "{name}: update {}", number + 1);
    }
    let ours: String = chip.memory.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(ours, memory[0], "{name}: memory");
}

#[test]
fn mode1_x24c01() {
    replay("mode1", MODE1, 0x7F, 0x03);
}

#[test]
fn mode2_24c16() {
    replay("mode2", MODE2, 0x7FF, 0x0F);
}

#[test]
fn mode3_24c64() {
    replay("mode3", MODE3, 0x1FFF, 0x07);
}
