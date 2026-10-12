//! Cartridges: ROM images, their boards, and the registers at $A130F1-$A130FF.
//!
//! Transcribed from megadrive-python's `cartridge.py`, whose module docstring lists the
//! boards modelled and the sources for each: plain ROM (padded and mirrored as MAME
//! 0.285 loads it; a detector), battery RAM (`rom_sram`, `rom_fram`, `rom_hardbl95`;
//! CMD `gen-hw.txt` 4.3), the Sega mapper (`rom_ssf2`; Trzynadlowski's `ssf2tnc.txt`),
//! Titan's Overdrive 2 board (`rom_titan`, the same mapper), Krikzz's Extended SSF
//! (`ssf`, `ssf_sram`), the serial EEPROM boards ([`EEPROM_BOARDS`]) and the J-Cart.
//! The board comes from MAME's software list, read at runtime by SHA-1; an unlisted
//! image falls back to its header.
//!
//! Rust's differences: the reference reads zip archives with `zipfile` and the list
//! with `xml.etree`; here [`load_image`] reads the archive itself (stored and deflated
//! members, with flate2) and [`software_list`] scans the XML for the five elements it
//! needs. The reference's `BootROM` is [`BootRom`], which the board selects rather
//! than swapping objects.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;

use crate::eeprom::{SerialEEPROM, MODE1, MODE2, MODE3};
use crate::io::Device;

pub const MAME_HASH: &str = "/usr/share/games/mame/hash/megadriv.xml";
pub const IMAGE_SUFFIXES: [&str; 4] = [".md", ".bin", ".gen", ".smd"];
pub const ROM_WINDOW: usize = 0x400000;

/// Boards that are ROM plus battery RAM.
pub const SRAM_SLOTS: [&str; 3] = ["rom_sram", "rom_fram", "rom_hardbl95"];
/// Boards on Krikzz's Extended SSF, with and without battery RAM (MAME's names,
/// `bus/megadrive/cart/options.cpp`).
pub const SSF_SLOTS: [&str; 2] = ["ssf", "ssf_sram"];
/// Boards on a Sega-mapper-shaped board: the SSF2 mapper, Titan's Overdrive 2 board,
/// and Krikzz's Extended SSF.
pub const MAPPER_SLOTS: [&str; 4] = ["rom_ssf2", "rom_titan", "ssf", "ssf_sram"];

/// The bank number a board's page registers take, as a mask: six bits on the Sega
/// mapper ("A page is specified with 6 bits (bits 7 and 6 are always 0)",
/// `ssf2tnc.txt`) and **five** on Krikzz's Extended SSF (`...RRRRR`,
/// `extended_ssf-v2.txt`; Demons of Asteborg writes page $1C).
pub fn bank_mask(slot: &str) -> u8 {
    if SSF_SLOTS.contains(&slot) {
        0x1F
    } else {
        DEFAULT_BANK_MASK
    }
}
pub const DEFAULT_BANK_MASK: u8 = 0x3F;

/// What a cartridge on Krikzz's Extended SSF says at $100 instead of "SEGA GENESIS"
/// ("Mapper will be activated in case if ROM header contains "SEGA SSF" string";
/// `SEGA DOA` is Demons of Asteborg's own variant).
pub const SSF_HEADERS: [&[u8; 8]; 2] = [b"SEGA SSF", b"SEGA DOA"];

/// Where one board puts the EEPROM's two lines, and which chip it carries.
///
/// `sda_in` is the byte address and bit the 68000 *reads* the chip's /SDA at;
/// `sda_out` and `scl` the byte address and bit it *writes* them at. `mode`,
/// `size_mask` and `page_mask` are Eke's fields for the chip itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EEPROMBoard {
    pub sda_in: (u32, u8),
    pub sda_out: (u32, u8),
    pub scl: (u32, u8),
    pub mode: u8,
    pub size_mask: usize,
    pub page_mask: usize,
    pub chip: &'static str,
    pub source: &'static str,
}

#[allow(clippy::too_many_arguments)]
const fn board(
    sda_in: (u32, u8),
    sda_out: (u32, u8),
    scl: (u32, u8),
    mode: u8,
    size_mask: usize,
    page_mask: usize,
    chip: &'static str,
    source: &'static str,
) -> EEPROMBoard {
    EEPROMBoard {
        sda_in,
        sda_out,
        scl,
        mode,
        size_mask,
        page_mask,
        chip,
        source,
    }
}

/// The EEPROM boards, by the name MAME's software list gives the slot: Eke-Eke's
/// `gen_eeprom.pdf` table, with MAME 0.285's code for which address is read and which
/// written where his naming leaves it open (the reference's comments give each
/// board's games and every disagreement).
pub const EEPROM_BOARDS: [(&str, EEPROMBoard); 11] = [
    ("rom_eeprom_mode1", board((0x200001, 0), (0x200001, 0), (0x200001, 1), MODE1, 0x7F, 0x03,
        "X24C01", "Eke gen_eeprom.pdf, SEGA and CAPCOM; CMD gen-eeprom.txt")),
    ("rom_eeprom", board((0x200001, 0), (0x200001, 0), (0x200001, 1), MODE1, 0x7F, 0x03,
        "X24C01", "Eke gen_eeprom.pdf, SEGA (MAME uses a mode-2 24C01 here)")),
    ("rom_nbajam_alt", board((0x200001, 1), (0x200001, 0), (0x200001, 1), MODE2, 0xFF, 0x03,
        "24C02", "Eke gen_eeprom.pdf, ACCLAIM type #1, with the bits as MAME eeprom.cpp lines 606-628 has them (his are self-contradictory)")),
    ("rom_nbajamte", board((0x200001, 0), (0x200001, 0), (0x200000, 0), MODE2, 0xFF, 0x03,
        "24C02", "Eke gen_eeprom.pdf, ACCLAIM type #2")),
    ("rom_nflqb96", board((0x200001, 0), (0x200001, 0), (0x200000, 0), MODE2, 0x7FF, 0x07,
        "24C16", "Eke gen_eeprom.pdf, ACCLAIM type #2")),
    ("rom_cslam", board((0x200001, 0), (0x200001, 0), (0x200000, 0), MODE3, 0x1FFF, 0x07,
        "24C64", "Eke gen_eeprom.pdf, ACCLAIM type #2")),
    ("rom_nhlpa", board((0x200001, 7), (0x200001, 7), (0x200001, 6), MODE1, 0x7F, 0x03,
        "X24C01", "Eke gen_eeprom.pdf, ELECTRONIC ARTS")),
    ("rom_codemast", board((0x380001, 7), (0x300000, 0), (0x300000, 1), MODE2, 0x3FF, 0x0F,
        "24C08", "Eke gen_eeprom.pdf, CODEMASTERS (which of $380001 and $300000 is read and which written from MAME jcart.cpp lines 186-210)")),
    ("rom_mm96", board((0x380001, 7), (0x300000, 0), (0x300000, 1), MODE2, 0x7FF, 0x0F,
        "24C16", "Eke gen_eeprom.pdf, CODEMASTERS")),
    ("rom_blara95", board((0x380001, 7), (0x380000, 0), (0x380000, 1), MODE1, 0x7F, 0x03,
        "X24C01", "Eke gen_eeprom.pdf, CODEMASTERS mode 1; addresses from MAME eeprom.cpp lines 344-366 (MAME fits a 24C65 instead)")),
    ("rom_blara96", board((0x380001, 7), (0x380000, 0), (0x380000, 1), MODE3, 0x1FFF, 0x07,
        "24C64", "Eke gen_eeprom.pdf, CODEMASTERS mode 3 (his PAGE_MASK is '?'; 0x07 is MAME's 24C65); addresses from MAME eeprom.cpp lines 344-366")),
]; // fmt: skip

pub fn eeprom_board(slot: &str) -> Option<EEPROMBoard> {
    EEPROM_BOARDS
        .iter()
        .find(|(name, _)| *name == slot)
        .map(|&(_, board)| board)
}

/// Boards with the two extra control ports Codemasters put on the cartridge, read and
/// written at $38FFFE (MAME `jcart.cpp` lines 150-172).
pub const JCART_SLOTS: [&str; 6] = [
    "rom_jcart",
    "rom_jcart_sampras",
    "rom_codemast",
    "rom_mm96",
    "rom_blara95",
    "rom_blara96",
];
pub const JCART_PORT: u32 = 0x38FFFE;

/// A Super Magic Drive dump: a 512-byte header, then 16 KB blocks whose first half
/// holds the odd bytes and second half the even ones (a copier format).
fn deinterleave_smd(data: &[u8]) -> Vec<u8> {
    let body = &data[512..];
    let mut out = vec![0u8; body.len()];
    for block in (0..body.len()).step_by(0x4000) {
        let chunk = &body[block..(block + 0x4000).min(body.len())];
        let half = chunk.len() / 2;
        for (i, &byte) in chunk[..half].iter().enumerate() {
            if block + 1 + 2 * i < block + chunk.len() {
                out[block + 1 + 2 * i] = byte;
            }
        }
        for (i, &byte) in chunk[half..].iter().enumerate() {
            if block + 2 * i < block + chunk.len() {
                out[block + 2 * i] = byte;
            }
        }
    }
    out
}

// -- a zip archive, as far as a cartridge needs one ---------------------------------

struct ZipMember {
    name: String,
    method: u16,
    compressed: usize,
    size: usize,
    local: usize,
}

fn le16(data: &[u8], at: usize) -> usize {
    usize::from(data[at]) | usize::from(data[at + 1]) << 8
}

fn le32(data: &[u8], at: usize) -> usize {
    le16(data, at) | le16(data, at + 2) << 16
}

/// The members of a zip archive (its central directory), or None if `data` is not
/// one (the reference's `zipfile.is_zipfile`: an end-of-central-directory record).
fn zip_members(data: &[u8]) -> Option<Vec<ZipMember>> {
    if data.len() < 22 {
        return None;
    }
    let lowest = data.len().saturating_sub(22 + 0xFFFF);
    let end = (lowest..=data.len() - 22)
        .rev()
        .find(|&at| data[at..at + 4] == [0x50, 0x4B, 0x05, 0x06])?;
    let count = le16(data, end + 10);
    let mut at = le32(data, end + 16);
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        if at + 46 > data.len() || data[at..at + 4] != [0x50, 0x4B, 0x01, 0x02] {
            return None;
        }
        let name_len = le16(data, at + 28);
        let extra_len = le16(data, at + 30);
        let comment_len = le16(data, at + 32);
        let name = String::from_utf8_lossy(data.get(at + 46..at + 46 + name_len)?).into_owned();
        members.push(ZipMember {
            name,
            method: le16(data, at + 10) as u16,
            compressed: le32(data, at + 20),
            size: le32(data, at + 24),
            local: le32(data, at + 42),
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Some(members)
}

fn zip_read(data: &[u8], member: &ZipMember) -> Result<Vec<u8>, String> {
    let at = member.local;
    if at + 30 > data.len() || data[at..at + 4] != [0x50, 0x4B, 0x03, 0x04] {
        return Err(format!("{}: bad local header", member.name));
    }
    let start = at + 30 + le16(data, at + 26) + le16(data, at + 28);
    let body = data
        .get(start..start + member.compressed)
        .ok_or_else(|| format!("{}: truncated", member.name))?;
    match member.method {
        0 => Ok(body.to_vec()),
        8 => {
            let mut out = Vec::with_capacity(member.size);
            flate2::read::DeflateDecoder::new(body)
                .read_to_end(&mut out)
                .map_err(|e| format!("{}: {e}", member.name))?;
            Ok(out)
        }
        method => Err(format!("{}: compression method {method}", member.name)),
    }
}

fn has_image_suffix(name: &str) -> bool {
    let lower = name.to_lowercase();
    IMAGE_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
}

/// `PurePosixPath(name).stem`: the last component without its last suffix.
fn posix_stem(name: &str) -> &str {
    let last = name.rsplit('/').next().unwrap_or(name);
    match last.rfind('.') {
        Some(dot) if dot > 0 => &last[..dot],
        _ => last,
    }
}

/// Which member of a zip is the cartridge: the only image, else the image whose name
/// matches the archive's own, else the largest (`Sonic & Knuckles (World).zip` holds
/// the 256 KB lock-on adapter ROM *before* the 2 MB game).
fn pick_image<'a>(members: &'a [ZipMember], stem: &str) -> Result<&'a ZipMember, String> {
    let mut names: Vec<&ZipMember> = members
        .iter()
        .filter(|m| has_image_suffix(&m.name))
        .collect();
    if names.is_empty() {
        names = members.iter().filter(|m| !m.name.ends_with('/')).collect();
    }
    if names.is_empty() {
        return Err("no image inside".into());
    }
    if names.len() == 1 {
        return Ok(names[0]);
    }
    if let Some(member) = names.iter().find(|m| posix_stem(&m.name) == stem) {
        return Ok(member);
    }
    // Python's max() keeps the first of equal sizes.
    let mut best = names[0];
    for member in &names[1..] {
        if member.size > best.size {
            best = member;
        }
    }
    Ok(best)
}

/// Read a cartridge image, from a zip if need be (see `pick_image`).
pub fn load_image(path: &Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (data, smd) = match zip_members(&raw) {
        Some(members) => {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let member =
                pick_image(&members, stem).map_err(|e| format!("{}: {e}", path.display()))?;
            let smd = member.name.to_lowercase().ends_with(".smd");
            (zip_read(&raw, member)?, smd)
        }
        None => {
            let smd = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("smd"));
            (raw, smd)
        }
    };
    if smd || (data.len() % 0x4000 == 512 && data.len() >= 10 && data[8..10] == [0xAA, 0xBB]) {
        return Ok(deinterleave_smd(&data));
    }
    Ok(data)
}

// -- MAME's software list -----------------------------------------------------------

/// One entry of MAME's software list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub slot: String,
    pub sram: usize,
}

fn unescape(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim_end();
        let key = key.rsplit(char::is_whitespace).next().unwrap_or("");
        let after = rest[eq + 1..].trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let close = after[1..].find(quote)? + 1;
        if key == name {
            return Some(unescape(&after[1..close]));
        }
        rest = &after[close + 1..];
    }
    None
}

/// Python's `int(text, 0)` for the sizes the list gives.
fn parse_int(text: &str) -> usize {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        usize::from_str_radix(hex, 16).unwrap_or(0)
    } else {
        text.parse().unwrap_or(0)
    }
}

/// SHA-1 of the ROM -> its entry, from MAME's megadriv.xml: for each part with
/// exactly one ROM, the software's name, the `slot` feature ("rom" by default) and
/// the `sram` data area's size. Empty when the list is not installed.
pub fn parse_software_list(xml: &str) -> HashMap<String, Listed> {
    let mut table = HashMap::new();
    let mut software = String::new();
    let mut slot = String::from("rom");
    let mut sram = 0usize;
    let mut roms: Vec<String> = Vec::new();
    let mut in_part = false;
    let mut at = 0;
    while let Some(open) = xml[at..].find('<') {
        let start = at + open;
        if xml[start..].starts_with("<!--") {
            at = xml[start..]
                .find("-->")
                .map_or(xml.len(), |end| start + end + 3);
            continue;
        }
        let Some(close) = xml[start..].find('>') else {
            break;
        };
        let tag = &xml[start + 1..start + close];
        at = start + close + 1;
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        let element = tag
            .trim_end_matches('/')
            .split(char::is_whitespace)
            .next()
            .unwrap_or("");
        match element {
            "software" => software = attribute(tag, "name").unwrap_or_default(),
            "part" => {
                in_part = !tag.ends_with('/');
                slot = String::from("rom");
                sram = 0;
                roms.clear();
            }
            "feature" if in_part => {
                if attribute(tag, "name").as_deref() == Some("slot") {
                    slot = attribute(tag, "value").unwrap_or_else(|| String::from("rom"));
                }
            }
            "dataarea" if in_part => {
                if attribute(tag, "name").as_deref() == Some("sram") {
                    sram = parse_int(&attribute(tag, "size").unwrap_or_else(|| "0".into()));
                }
            }
            "rom" if in_part => {
                if let Some(sha1) = attribute(tag, "sha1") {
                    if !sha1.is_empty() {
                        roms.push(sha1);
                    }
                }
            }
            "/part" => {
                if in_part && roms.len() == 1 {
                    table.insert(
                        roms[0].clone(),
                        Listed {
                            name: software.clone(),
                            slot: slot.clone(),
                            sram,
                        },
                    );
                }
                in_part = false;
            }
            _ => {}
        }
    }
    table
}

/// The software list, read once.
pub fn software_list() -> &'static HashMap<String, Listed> {
    static LIST: OnceLock<HashMap<String, Listed>> = OnceLock::new();
    LIST.get_or_init(|| match std::fs::read_to_string(MAME_HASH) {
        Ok(xml) => parse_software_list(&xml),
        Err(_) => HashMap::new(),
    })
}

/// MAME's software-list entry for this exact image, or None.
pub fn identify(image: &[u8]) -> Option<&'static Listed> {
    software_list().get(&sha1_smol::Sha1::from(image).digest().to_string())
}

/// The Manual's backup-RAM header field at $1B0: `RA`, a type byte, $20, then the
/// start and end addresses (CMD `gen-hw.txt` 4.1 shows one).
pub fn header_backup_ram(image: &[u8]) -> Option<(u32, u32)> {
    if image.len() < 0x1BC || &image[0x1B0..0x1B2] != b"RA" {
        return None;
    }
    let start = u32::from_be_bytes(image[0x1B4..0x1B8].try_into().unwrap());
    let end = u32::from_be_bytes(image[0x1B8..0x1BC].try_into().unwrap());
    (end >= start).then_some((start, end))
}

/// True when the image's own code writes the Sega mapper's page registers: a
/// `move.b` ($13C0-$13FF, or the immediate form $13FC at one word's remove) whose
/// absolute long destination is $A130F3, $A130F5, ... $A130FF.
pub fn writes_sega_mapper(image: &[u8]) -> bool {
    let word = |at: usize| u32::from(image[at]) << 8 | u32::from(image[at + 1]);
    let mut at = 0;
    while at + 3 <= image.len() {
        if image[at..at + 3] == [0x00, 0xA1, 0x30]
            && at % 2 == 0
            && at + 3 < image.len()
            && image[at + 3] & 0x01 != 0
            && image[at + 3] >= 0xF3
        {
            let before = if at >= 2 { Some(word(at - 2)) } else { None };
            let earlier = if at >= 4 { Some(word(at - 4)) } else { None };
            if before.is_some_and(|b| (0x13C0..0x1400).contains(&b)) || earlier == Some(0x13FC) {
                return true;
            }
        }
        at += 1;
    }
    false
}

fn padded(image: &[u8]) -> Vec<u8> {
    let mut size = 1;
    while size < image.len() {
        size <<= 1;
    }
    let mut out = image.to_vec();
    out.resize(size, 0xFF);
    out
}

/// ROM on the 68000's bus, with optional battery RAM and bank registers.
#[derive(Clone, Debug)]
pub struct Cartridge {
    pub image: Vec<u8>,
    pub slot: String,
    pub name: String,
    /// A board this module does not model yet (the image runs as plain ROM).
    pub unsupported: Option<String>,
    /// The 4 MB window as one array: mirroring is then a plain index.
    pub rom: Vec<u8>,
    pub rom_size: usize,
    pub sram: Vec<u8>,
    pub sram_start: u32,
    pub sram_end: u32,
    /// $A130F1: bit 0 maps the RAM, bit 1 protects it.
    pub sram_mapped: bool,
    pub sram_protected: bool,
    /// The Sega mapper: bank n shows page banks[n] (512 KB pages).
    pub banks: [u8; 8],
    pub mapper: bool,
    pub bank_mask: u8,
    pub full: Vec<u8>,
    /// The serial EEPROM, its board, and its two line levels.
    pub eeprom: Option<SerialEEPROM>,
    pub eeprom_board: Option<EEPROMBoard>,
    scl: u8,
    sda: u8,
    /// The J-Cart's two extra control ports, if the board has them.
    pub jcart: bool,
    pub jcart_devices: [Option<Device>; 2],
    pub jcart_th: u8,
    /// Anything the cartridge decodes besides plain ROM: an EEPROM's lines or a
    /// J-Cart's ports.
    pub decoded: bool,
    /// True when *anything* has to be looked at before the ROM.
    pub special: bool,
}

impl Cartridge {
    pub fn new(image: Vec<u8>, slot: &str, sram_size: usize, name: &str) -> Self {
        let rom = if image.is_empty() {
            vec![0xFF, 0xFF]
        } else {
            padded(&image)
        };
        // The 4 MB window as one array: mirroring is then a plain index.
        let repeats = (ROM_WINDOW / rom.len()).max(1);
        let mut window = rom.repeat(repeats);
        window.truncate(ROM_WINDOW.max(rom.len()));
        let rom_size = image.len();
        // A battery RAM's contents before anything is saved: $FF, as MAME 0.285 starts
        // a new one, and the lockstep follows it.
        let sram = vec![0xFF; sram_size];
        let sram_start = 0x200000u32;
        let sram_end = sram_start + 2 * sram_size as u32; // odd bytes only
        let sram_mapped = sram_size != 0 && rom_size <= sram_start as usize;
        let mapper = MAPPER_SLOTS.contains(&slot);
        let full = if mapper {
            let mut full = image.clone();
            let pad = (0x80000 - full.len() % 0x80000) % 0x80000;
            full.resize(full.len() + pad, 0xFF);
            full
        } else {
            Vec::new()
        };
        let board = eeprom_board(slot);
        let eeprom = board.map(|b| SerialEEPROM::new(b.mode, b.size_mask, b.page_mask, None));
        let jcart = JCART_SLOTS.contains(&slot);
        let decoded = eeprom.is_some() || jcart;
        let special = !sram.is_empty() || decoded;
        let known = slot == "rom" || SRAM_SLOTS.contains(&slot) || MAPPER_SLOTS.contains(&slot);
        let unsupported = if !known && !decoded {
            Some(slot.to_string())
        } else if image.len() > ROM_WINDOW && !MAPPER_SLOTS.contains(&slot) {
            // Larger than the 68000's cartridge window with no mapper to page it in:
            // only the first 4 MB could ever be reached, so say so rather than run a
            // truncated copy (milestone 8: nine `Earthion` images).
            Some(format!(
                "oversize: {:.2} MB, no mapper",
                image.len() as f64 / f64::from(1u32 << 20)
            ))
        } else {
            None
        };
        Cartridge {
            image,
            slot: slot.to_string(),
            name: name.to_string(),
            unsupported,
            rom: window,
            rom_size,
            sram,
            sram_start,
            sram_end,
            sram_mapped,
            sram_protected: false,
            banks: [0, 1, 2, 3, 4, 5, 6, 7],
            mapper,
            bank_mask: bank_mask(slot),
            full,
            eeprom,
            eeprom_board: board,
            scl: 1,
            sda: 1,
            jcart,
            jcart_devices: [None, None],
            jcart_th: 1,
            decoded,
            special,
        }
    }

    /// An empty slot (the reference's `Cartridge(b"")`).
    pub fn empty() -> Self {
        Cartridge::new(Vec::new(), "rom", 0, "")
    }

    pub fn from_file(path: &Path) -> Result<Self, String> {
        Ok(Cartridge::from_image(load_image(path)?))
    }

    fn with_backup(image: Vec<u8>, slot: &str, start: u32, end: u32) -> Self {
        let size = (((end - start) / 2 + 1) as usize).min(0x8000);
        let mut cart = Cartridge::new(image, slot, size, "");
        cart.sram_start = start & 0x3FFFFE;
        cart.sram_end = cart.sram_start + 2 * cart.sram.len() as u32;
        cart
    }

    /// The board from MAME's software list, else from the header.
    pub fn from_image(image: Vec<u8>) -> Self {
        if let Some(listed) = identify(&image) {
            // MAME's `sram` data area is the size of the *address range* the RAM
            // answers, not of the RAM: the chip is eight bits wide on the odd byte, so
            // the board holds half (`md_slot.cpp` lines 618-623 and 101; CMD 4.3).
            let mut sram = listed.sram / 2;
            if SRAM_SLOTS.contains(&listed.slot.as_str()) && sram == 0 {
                sram = 0x8000; // MAME's default range, $200000 + $FFFF
            }
            let (slot, name) = (listed.slot.clone(), listed.name.clone());
            return Cartridge::new(image, &slot, sram, &name);
        }
        if image.len() >= 0x108 && SSF_HEADERS.iter().any(|h| &image[0x100..0x108] == *h) {
            // Krikzz's Extended SSF announces itself in the console field; the
            // backup-RAM header then says which of the two boards it is.
            return match header_backup_ram(&image) {
                None => Cartridge::new(image, "ssf", 0, ""),
                Some((start, end)) => Cartridge::with_backup(image, "ssf_sram", start, end),
            };
        }
        let backup = header_backup_ram(&image);
        if image.len() > ROM_WINDOW && writes_sega_mapper(&image) {
            // Larger than the window, listed nowhere, announcing no board -- but its own
            // code writes the Sega mapper's page registers (the 5 MB "Rickroll" video
            // player, issue #2). A backup-RAM field is honoured as on `ssf_sram`.
            return match backup {
                None => Cartridge::new(image, "rom_ssf2", 0, ""),
                Some((start, end)) => Cartridge::with_backup(image, "rom_ssf2", start, end),
            };
        }
        if let Some((start, end)) = backup {
            let mut cart = Cartridge::with_backup(image, "rom_sram", start, end);
            cart.sram_mapped = cart.rom_size <= cart.sram_start as usize;
            return cart;
        }
        Cartridge::new(image, "rom", 0, "")
    }

    // -- the battery ----------------------------------------------------------------

    /// What this board saves with: "sram", "eeprom" or "" (nothing).
    pub fn battery(&self) -> &'static str {
        if self.eeprom.is_some() {
            "eeprom"
        } else if !self.sram.is_empty() {
            "sram"
        } else {
            ""
        }
    }

    /// The battery's contents, to write to a file.
    pub fn save_bytes(&self) -> Vec<u8> {
        match &self.eeprom {
            Some(eeprom) => eeprom.memory.clone(),
            None => self.sram.clone(),
        }
    }

    /// Put a saved file's contents back, as plugging the cartridge in does.
    pub fn load_bytes(&mut self, data: &[u8]) {
        let memory = match &mut self.eeprom {
            Some(eeprom) => &mut eeprom.memory,
            None => &mut self.sram,
        };
        let n = data.len().min(memory.len());
        memory[..n].copy_from_slice(&data[..n]);
    }

    // -- the 68000's accesses to $000000-$3FFFFF -----------------------------------

    fn rom_offset(&self, address: u32) -> usize {
        if self.mapper {
            (usize::from(self.banks[(address >> 19) as usize & 7]) << 19)
                | (address as usize & 0x7FFFF)
        } else {
            address as usize
        }
    }

    /// The EEPROM's /SDA or a J-Cart port, as a byte, or None for ROM.
    fn decode_read(&mut self, address: u32) -> Option<u8> {
        if let Some(board) = self.eeprom_board {
            if address == board.sda_in.0 {
                let sda = self.eeprom.as_ref().expect("board has a chip").read_sda();
                return Some((sda & 1) << board.sda_in.1);
            }
        }
        if self.jcart && address & !1 == JCART_PORT {
            return Some(self.jcart_byte(address));
        }
        None
    }

    /// One byte of the J-Cart's port word: port 3 low, port 4 high, bit 7 set.
    fn jcart_byte(&mut self, address: u32) -> u8 {
        let index = if address & 1 != 0 { 0 } else { 1 };
        let th = self.jcart_th;
        let pins = match &mut self.jcart_devices[index] {
            None => 0x3F,
            // The reference calls `pins(th)` with its default clock, 0.
            Some(device) => device.pins(th, 0),
        };
        (pins & 0x3F) | (th << 6) | 0x80
    }

    /// A byte written where the cartridge decodes something; true if it did.
    fn decode_write(&mut self, address: u32, value: u8) -> bool {
        let mut touched = false;
        if let Some(board) = self.eeprom_board {
            if address == board.scl.0 {
                self.scl = (value >> board.scl.1) & 1;
                touched = true;
            }
            if address == board.sda_out.0 {
                self.sda = (value >> board.sda_out.1) & 1;
                touched = true;
            }
            if touched {
                let (scl, sda) = (self.scl, self.sda);
                self.eeprom
                    .as_mut()
                    .expect("board has a chip")
                    .update(scl, sda);
            }
        }
        if self.jcart && address & !1 == JCART_PORT {
            // "assume TH only actively driven low" (MAME): bit 0 of the write is TH.
            self.jcart_th = value & 1;
            touched = true;
        }
        touched
    }

    pub fn read_word(&mut self, address: u32) -> u16 {
        if self.special {
            if self.sram_mapped && self.sram_start <= address && address < self.sram_end {
                // 8-bit RAM on the odd byte: the even byte is undriven; $FF there as
                // MAME does.
                return 0xFF00 | u16::from(self.sram[((address - self.sram_start) >> 1) as usize]);
            }
            if self.decoded {
                let high = self.decode_read(address);
                let low = self.decode_read(address | 1);
                if high.is_some() || low.is_some() {
                    return u16::from(high.unwrap_or(0)) << 8 | u16::from(low.unwrap_or(0));
                }
            }
        }
        if self.mapper {
            let offset = self.rom_offset(address);
            if offset + 1 < self.full.len() {
                return u16::from(self.full[offset]) << 8 | u16::from(self.full[offset + 1]);
            }
            return 0xFFFF;
        }
        let rom = &self.rom;
        u16::from(rom[address as usize]) << 8 | u16::from(rom[address as usize + 1])
    }

    pub fn read_byte(&mut self, address: u32) -> u8 {
        if self.special {
            if self.sram_mapped && self.sram_start <= address && address < self.sram_end {
                return if address & 1 != 0 {
                    self.sram[((address - self.sram_start) >> 1) as usize]
                } else {
                    0xFF
                };
            }
            if self.decoded {
                if let Some(value) = self.decode_read(address) {
                    return value;
                }
            }
        }
        self.peek(address)
    }

    /// A ROM byte without side effects (the board's `z80_peek` and the probe read the
    /// cartridge only where it is plain ROM or battery RAM; the reference calls
    /// `read_byte`, which for an EEPROM or J-Cart address reads the line -- a probe
    /// of such an address is not something any Z80 program does).
    pub fn peek(&self, address: u32) -> u8 {
        if self.mapper {
            let offset = self.rom_offset(address);
            return self.full.get(offset).copied().unwrap_or(0xFF);
        }
        self.rom[address as usize]
    }

    pub fn write_byte(&mut self, address: u32, value: u8) {
        if !self.special {
            return;
        }
        if self.decoded && self.decode_write(address, value) {
            return;
        }
        if self.sram_mapped
            && !self.sram_protected
            && address & 1 != 0
            && self.sram_start <= address
            && address < self.sram_end
        {
            self.sram[((address - self.sram_start) >> 1) as usize] = value;
        }
    }

    pub fn write_word(&mut self, address: u32, value: u16) {
        if !self.special {
            return;
        }
        if self.decoded {
            // A word write drives both bytes, and several of these boards put the clock
            // on one and the data on the other, so both halves are presented before the
            // chip is clocked.
            self.decode_write_word(address, value);
            return;
        }
        self.write_byte(address | 1, value as u8);
    }

    fn decode_write_word(&mut self, address: u32, value: u16) {
        let mut touched = false;
        if let Some(board) = self.eeprom_board {
            for (line, is_scl) in [(board.scl, true), (board.sda_out, false)] {
                let (at, bit) = line;
                let level = if at == address & !1 {
                    Some(((value >> (8 + bit)) & 1) as u8)
                } else if at == address | 1 {
                    Some(((value >> bit) & 1) as u8)
                } else {
                    None
                };
                if let Some(level) = level {
                    if is_scl {
                        self.scl = level;
                    } else {
                        self.sda = level;
                    }
                    touched = true;
                }
            }
            if touched {
                let (scl, sda) = (self.scl, self.sda);
                self.eeprom
                    .as_mut()
                    .expect("board has a chip")
                    .update(scl, sda);
            }
        }
        if self.jcart && address & !1 == JCART_PORT {
            self.jcart_th = (value & 1) as u8;
            touched = true;
        }
        if !touched {
            self.write_byte(address | 1, value as u8);
        }
    }

    // -- the /TIME range $A13000-$A130FF ---------------------------------------------

    /// A byte written to $A130xx: $A130F1 maps and protects the battery RAM;
    /// $A130F3-$A130FF page the Sega mapper's second to eighth 512 KB windows.
    pub fn write_time(&mut self, address: u32, value: u8) {
        let register = address & 0xFF;
        if register == 0xF1 && !self.sram.is_empty() {
            self.sram_mapped = value & 1 != 0;
            self.sram_protected = value & 2 != 0;
        } else if self.mapper && register & 1 != 0 && register >= 0xF3 {
            self.banks[((register - 0xF1) >> 1) as usize] = value & self.bank_mask;
        }
    }

    /// A byte read from $A130xx, or None when no cartridge chip drives the bus.
    pub fn read_time(&self, _address: u32) -> Option<u8> {
        None
    }
}

/// The TMSS boot ROM in the cartridge's place, while $A14101 bit 0 is clear: its
/// 2,048 bytes (the No-Intro dump) padded with zeros to 16 KB, and $FFFF above that,
/// as MAME's `tmss_r` (`reference/src/TMSS-Disassembly`, `RAM_Code`).
#[derive(Clone, Debug)]
pub struct BootRom {
    pub image: Vec<u8>,
    pub rom: Vec<u8>,
}

impl BootRom {
    pub fn new(image: Vec<u8>) -> Self {
        let mut rom = image.clone();
        if rom.len() < 0x4000 {
            rom.resize(0x4000, 0);
        }
        BootRom { image, rom }
    }

    pub fn read_word(&self, address: u32) -> u16 {
        let address = address as usize;
        if address + 1 < self.rom.len() {
            return u16::from(self.rom[address]) << 8 | u16::from(self.rom[address + 1]);
        }
        0xFFFF
    }

    pub fn read_byte(&self, address: u32) -> u8 {
        self.rom.get(address as usize).copied().unwrap_or(0xFF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_list_scan() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE softwarelist SYSTEM "softwarelist.dtd">
<softwarelist name="megadriv">
  <!-- a <comment> -->
  <software name="pstar4">
    <description>Phantasy Star IV &amp; more</description>
    <part name="cart" interface="megadriv_cart">
      <feature name="slot" value="rom_sram"/>
      <dataarea name="rom" size="3145728">
        <rom name="a.bin" size="3145728" crc="1" sha1="abc" offset="0"/>
      </dataarea>
      <dataarea name="sram" size="16384"></dataarea>
    </part>
  </software>
  <software name="two">
    <part name="cart" interface="megadriv_cart">
      <dataarea name="rom" size="0x10">
        <rom name="a" sha1="x1"/><rom name="b" sha1="x2"/>
      </dataarea>
    </part>
  </software>
</softwarelist>"#;
        let table = parse_software_list(xml);
        assert_eq!(table.len(), 1);
        assert_eq!(
            table["abc"],
            Listed {
                name: "pstar4".into(),
                slot: "rom_sram".into(),
                sram: 16384
            }
        );
    }
}
