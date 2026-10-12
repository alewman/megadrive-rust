//! A Rust Mega Drive / Genesis board transcribed from `megadrive-python`.
//!
//! The module layout mirrors the reference's `src/megadrive_python/` so the two can be
//! read side by side: `machine`, `arbiter`, `io`, `eeprom`, `cartridge`, `render`,
//! `vdp`, `board`, `system`, `audio`, `wav` and `png` are its modules of the same
//! names. The chips are the family's Rust cores: m68000-rust, z80-rust, ym2612-rust
//! and sn76489-rust. `trace` writes the lockstep trace both boards share
//! (docs/handoff-brief.md, "The gate").

pub mod arbiter;
pub mod audio;
pub mod board;
pub mod cartridge;
pub mod eeprom;
pub mod io;
pub mod machine;
pub mod png;
pub mod render;
pub mod system;
pub mod trace;
pub mod vdp;
pub mod wav;

pub use crate::board::{Board, Lockup};
pub use crate::cartridge::Cartridge;
pub use crate::machine::{machine, Machine};
pub use crate::system::System;
