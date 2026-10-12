#!/usr/bin/env python3
"""A gate that cannot fail proves nothing: break the board in one place at a time and
check the lockstep sees it.

Each fault is one change to a copy of ``src/``, built into its own ``md-trace``, which
then runs every case of ``scripts/cases.tsv`` (at most ``--frames`` frames each). Its
trace is compared with the clean board's (``external/traces/NAME.rs.trace``, which the
gate has shown identical to the reference's): a fault is caught when any case's trace
differs. Run after ``scripts/lockstep.py``, which writes the clean traces. Stdlib only.

    python3 scripts/seeded_faults.py [--only NAME ...] [--frames N] [--jobs N]
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import lockstep  # noqa: E402
from make_examples import EXAMPLES  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
WORK = REPO / "external/faults"

#: (name, file, the text, its replacement). Each text occurs exactly once.
FAULTS = [
    ("arbiter: the Z80's own cycle 30 MCLK", "src/arbiter.rs",
     "pub const OWN_CYCLE_MCLK: i64 = 29;", "pub const OWN_CYCLE_MCLK: i64 = 30;"),
    ("arbiter: the cut-off 22 MCLK", "src/arbiter.rs",
     "pub const CUTOFF_MCLK: i64 = 21;", "pub const CUTOFF_MCLK: i64 = 22;"),
    ("board: cartridge refresh runs by itself from 119", "src/board.rs",
     "pub const CART_REFRESH_SELF_MCLK: i64 = 112;", "pub const CART_REFRESH_SELF_MCLK: i64 = 119;"),
    ("board: work-RAM refresh due 820 after", "src/board.rs",
     "pub const RAM_REFRESH_AFTER_MCLK: i64 = 819;", "pub const RAM_REFRESH_AFTER_MCLK: i64 = 820;"),
    ("board: holds not rounded to the 68000's clock", "src/board.rs",
     "pub const HOLD_ON_68K_EDGE: bool = true;", "pub const HOLD_ON_68K_EDGE: bool = false;"),
    ("board: open bus reads $FFFF", "src/board.rs",
     "pub fn open_bus(&self) -> u16 {\n        self.m68k_irc", "pub fn open_bus(&self) -> u16 {\n        0xFFFF"),
    ("board: the refresh tie at 8", "src/board.rs",
     "pub const RAM_REFRESH_TIE_MCLK: i64 = 7;", "pub const RAM_REFRESH_TIE_MCLK: i64 = 8;"),
    # (22 would be an equivalent change: a 68000 access is on the 7-MCLK grid, so 21
    # and 22 fall in the same FM cycle; a clock later is not.)
    ("board: the FM takes a 68000 write a clock later", "src/board.rs",
     "pub const M68K_FM_WRITE_MCLK: i64 = 21;", "pub const M68K_FM_WRITE_MCLK: i64 = 28;"),
    ("board: the bank register shifts in bit 22", "src/board.rs",
     "self.bank = ((self.bank >> 1) | (u32::from(value & 1) << 23)) & 0xFF8000;",
     "self.bank = ((self.bank >> 1) | (u32::from(value & 1) << 22)) & 0xFF8000;"),
    ("system: BUSREQ sampled 34 after", "src/system.rs",
     "pub const Z80_BUSREQ_LAG_MCLK: i64 = 33;", "pub const Z80_BUSREQ_LAG_MCLK: i64 = 34;"),
    ("system: the Z80 resumes 43 after", "src/system.rs",
     "pub const Z80_RESUME_MCLK: i64 = 42;", "pub const Z80_RESUME_MCLK: i64 = 43;"),
    ("system: the Z80's clock phase 13", "src/system.rs",
     "pub const Z80_CLOCK_PHASE: i64 = 14;", "pub const Z80_CLOCK_PHASE: i64 = 13;"),
    ("system: no E-clock credit at an acknowledge", "src/system.rs",
     "self.acknowledge_credit = credit as u64;", "self.acknowledge_credit = 0;"),
    ("vdp: the read release 54 MCLK", "src/vdp.rs",
     "pub const READ_RELEASE_MCLK: i64 = 53;", "pub const READ_RELEASE_MCLK: i64 = 54;"),
    ("vdp: the FIFO's commit delay 45", "src/vdp.rs",
     "pub const FIFO_COMMIT_DELAY_MCLK: i64 = 44;", "pub const FIFO_COMMIT_DELAY_MCLK: i64 = 45;"),
    ("vdp: the HV sample 4 MCLK in", "src/vdp.rs",
     "pub const HV_SAMPLE_DELAY: i64 = 3;", "pub const HV_SAMPLE_DELAY: i64 = 4;"),
    ("vdp: the DMA start-up 40 MCLK", "src/vdp.rs",
     "pub const DMA_STARTUP_MCLK: i64 = 39;", "pub const DMA_STARTUP_MCLK: i64 = 40;"),
    ("vdp: the die's DMA resume 35", "src/vdp.rs",
     "pub const DMA_RESUME_DIE_MCLK: i64 = 34;", "pub const DMA_RESUME_DIE_MCLK: i64 = 35;"),
    ("vdp: the full-screen vscroll latch ignores late writes", "src/vdp.rs",
     "s.vscroll_latch[plane] = s.vscroll_before[plane].unwrap_or(s.vsram[plane]);",
     "s.vscroll_latch[plane] = s.vsram[plane];"),
    ("render: H40 output delay 22 pixels", "src/render.rs",
     "HName::H40 => 21,", "HName::H40 => 22,"),
    ("render: colour 15 cancels the shadow", "src/render.rs",
     "if shade == 1 && sv & 0x0F == 14 {", "if shade == 1 && sv & 0x0F == 15 {"),
    ("io: the 6-button counter stops at 3", "src/io.rs",
     "if th != 0 && self.mode_switch && self.edges < 4 {",
     "if th != 0 && self.mode_switch && self.edges < 3 {"),
    ("eeprom: a write wraps at the chip, not the page", "src/eeprom.rs",
     "(self.address & !self.page_mask) | ((self.address + 1) & self.page_mask);",
     "(self.address + 1) & self.size_mask;"),
    ("audio: the PSG 1 % louder", "src/audio.rs",
     "pub const PSG_GAIN: f64 = 422.0;", "pub const PSG_GAIN: f64 = 426.22;"),
]


def slug(name: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")


def build(fault) -> Path:
    name, path, old, new = fault
    work = WORK / "work"
    if work.exists():
        shutil.rmtree(work)  # our own scratch copy under external/
    shutil.copytree(REPO / "src", work / "src")
    shutil.copytree(REPO / "tests", work / "tests")
    shutil.copy(REPO / "Cargo.toml", work / "Cargo.toml")
    shutil.copy(REPO / "Cargo.lock", work / "Cargo.lock")
    source = (work / path).read_text()
    if source.count(old) != 1:
        raise SystemExit(f"{name}: the text occurs {source.count(old)} times in {path}")
    (work / path).write_text(source.replace(old, new))
    env = dict(os.environ, CARGO_TARGET_DIR=str(WORK / "target"))
    subprocess.run(["nice", "rustup", "run", "stable", "cargo", "build", "--release", "-q",
                    "--bin", "md-trace"], cwd=work, env=env, check=True)  # fmt: skip
    binary = WORK / "bin" / slug(name)
    binary.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(WORK / "target/release/md-trace", binary)
    # The repository's own tests (the example cartridges, the EEPROM vectors) as well.
    tests = subprocess.run(["nice", "rustup", "run", "stable", "cargo", "test", "--release",
                            "-q", "--test", "examples", "--test", "eeprom"],
                           cwd=work, env=env, capture_output=True, text=True)  # fmt: skip
    return binary, tests.returncode != 0


def check(binary: Path, case, frames: int, out: Path) -> str | None:
    name, count, cart, args = case
    count = min(count, frames)
    trace = out / f"{name}.trace"
    subprocess.run(["nice", str(binary), cart, "--frames", str(count), *args, "--out", str(trace)],
                   capture_output=True, check=False)  # fmt: skip
    reference = REPO / "external/traces" / f"{name}.rs.trace"
    if not reference.exists():  # an example cartridge: its committed reference trace
        reference = REPO / "tests/examples" / f"{name}.trace"
    clean = reference.read_text().splitlines()
    ours = trace.read_text().splitlines() if trace.exists() else []
    frame = "-"
    for number, line in enumerate(ours):
        if line.startswith("F "):
            frame = line.split()[1]
        if number >= len(clean) or line != clean[number]:
            return f"{name} frame {frame} ({line.split()[0] if line else '?'})"
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--frames", type=int, default=900)
    parser.add_argument("--jobs", type=int, default=14)
    args = parser.parse_args()
    cases = lockstep.cases()
    # The example cartridges too, against the reference's committed traces.
    for name, (make, frames, extra) in EXAMPLES.items():
        cartridge = REPO / "tests/examples" / f"{make.__name__.replace('_', '-')}.bin"
        cases.append((name, frames, str(cartridge), list(extra)))
    caught = 0
    faults = [f for f in FAULTS if not args.only or slug(f[0]) in args.only]
    for fault in faults:
        binary, tests_failed = build(fault)
        out = WORK / "traces" / slug(fault[0])
        out.mkdir(parents=True, exist_ok=True)
        with ThreadPoolExecutor(args.jobs) as pool:
            found = [r for r in pool.map(lambda c: check(binary, c, args.frames, out), cases) if r]
        if tests_failed:
            found.insert(0, "cargo test")
        caught += bool(found)
        verdict = f"caught by {len(found)} of {len(cases) + 1} checks, first {found[0]}" if found else "MISSED"
        print(f"{fault[0]:58} {verdict}", flush=True)
    print(f"{caught} of {len(faults)} caught")
    return 0 if caught == len(faults) else 1


if __name__ == "__main__":
    sys.exit(main())
