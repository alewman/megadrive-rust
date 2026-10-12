//! Just enough PNG to write frames: 8-bit RGB, no filtering (megadrive-python's
//! `png.py`, writing half).

use std::io::Write;
use std::path::Path;

pub const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// The PNG bytes of rows of RGB888 ints.
pub fn encode_png(rows: &[Vec<u32>]) -> Vec<u8> {
    let height = rows.len();
    let width = rows.first().map_or(0, Vec::len);
    let mut raw = Vec::with_capacity(height * (1 + 3 * width));
    for row in rows {
        raw.push(0); // filter type 0: none
        for &pixel in row {
            raw.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
        }
    }
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(&raw).expect("in memory");
    let compressed = encoder.finish().expect("in memory");
    let mut out = SIGNATURE.to_vec();
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &compressed);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Write rows of RGB888 ints as an 8-bit RGB PNG.
pub fn write_png(path: &Path, rows: &[Vec<u32>]) -> std::io::Result<()> {
    std::fs::write(path, encode_png(rows))
}
