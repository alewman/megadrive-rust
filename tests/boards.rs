//! Every cartridge board against the reference's, access by access
//! (`scripts/make_board_vectors.py`): the 42 slots the SVP is not, in three image sizes,
//! including the boards no local dump reaches and the paths no game of the gate does.

use megadrive_rust::cartridge::Cartridge;
use megadrive_rust::mappers::Mapper;
use megadrive_rust::trace::crc32;

fn image(size: usize, multiplier: u64) -> Vec<u8> {
    (0..size as u64)
        .map(|i| ((i * multiplier) >> 16) as u8)
        .collect()
}

fn hex(field: Option<&str>) -> u32 {
    u32::from_str_radix(field.unwrap(), 16).unwrap()
}

fn board(slot: &str, size: usize) -> Box<Mapper> {
    let mut cart = Cartridge::with_board(image(size, 0x9E37_79B1), slot, "");
    let mut mapper = cart.board.take().unwrap();
    if mapper.has_socket() {
        let patch = if size == 0x400000 && slot == "rom_sk" {
            image(0x40000, 0xC2B2_AE35)
        } else {
            Vec::new()
        };
        mapper.attach(Cartridge::from_image(image(0x400000, 0x85EB_CA6B)), patch);
    }
    mapper
}

#[test]
fn every_board_against_the_reference() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/examples/boards.txt");
    let text = std::fs::read_to_string(path).unwrap();
    let mut mapper: Option<Box<Mapper>> = None;
    let mut name = String::new();
    let mut boards = 0;
    for (number, line) in text.lines().enumerate() {
        let mut fields = line.split(' ');
        let op = fields.next().unwrap();
        if op == "S" {
            let slot = fields.next().unwrap();
            let size = hex(fields.next()) as usize;
            mapper = Some(board(slot, size));
            name = format!("{slot} at {size:X}");
            boards += 1;
            continue;
        }
        let m = mapper.as_mut().unwrap();
        let at = || format!("{name}, line {}: {line}", number + 1);
        match op {
            "R" => assert_eq!(u32::from(m.read_word(hex(fields.next()))), hex(fields.next()), "{}", at()),
            "B" => assert_eq!(u32::from(m.read_byte(hex(fields.next()))), hex(fields.next()), "{}", at()),
            "D" => assert_eq!(u32::from(m.dma_read_word(hex(fields.next()))), hex(fields.next()), "{}", at()),
            "W" => m.write_word(hex(fields.next()), hex(fields.next()) as u16),
            "Y" => m.write_byte(hex(fields.next()), hex(fields.next()) as u8),
            "T" => {
                let address = hex(fields.next());
                let want = fields.next().unwrap();
                let got = m.read_time_word(address).map_or("-".to_string(), |w| format!("{w:X}"));
                assert_eq!(got, want, "{}", at());
            }
            "L" => {
                let address = hex(fields.next());
                let (high, low) = (hex(fields.next()) as u8, hex(fields.next()) as u8);
                m.write_time_lanes(address, high, low, fields.next() == Some("1"));
            }
            "X" => m.reset(),
            "C" => assert_eq!(crc32(&m.save_bytes()), hex(fields.next()), "{}", at()),
            other => panic!("{other}"),
        }
    }
    assert_eq!(boards, 42 * 3);
}
