//! A 16-bit stereo WAV writer, for `--wav` (megadrive-python's `wav.py`).

use std::path::Path;

/// `samples` as (left, right) pairs of signed 16-bit values.
pub fn write_wav(path: &Path, samples: &[(i32, i32)], rate: u32) -> std::io::Result<()> {
    let mut payload = Vec::with_capacity(samples.len() * 4);
    for &(left, right) in samples {
        payload.extend_from_slice(&(left.clamp(-32768, 32767) as i16).to_le_bytes());
        payload.extend_from_slice(&(right.clamp(-32768, 32767) as i16).to_le_bytes());
    }
    let mut out = Vec::with_capacity(44 + payload.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + payload.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    std::fs::write(path, out)
}
