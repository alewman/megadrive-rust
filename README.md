# megadrive-rust

A Rust **Sega Mega Drive / Genesis** board, transcribed from
[megadrive-python](https://github.com/alewman/megadrive-python) at the frozen pin
`megadrive-rust-pin-1` (`d7882e1`) and held in lockstep with it: on every case of the
gate the two boards' traces are identical, frame for frame. A trace covers:
- both CPUs' registers;
- every memory;
- every field of the VDP;
- the picture;
- both sound chips' samples;
- the analogue mix.

It runs on the family's Rust cores:

| Chip | Core | Pin |
| --- | --- | --- |
| 68000 | [m68000-rust](https://github.com/alewman/m68000-rust) | `ce3b489` (main, with the board's two hooks, see below) |
| Z80 | [z80-rust](https://github.com/alewman/z80-rust) | `3b15c1d` (= z80-python `b62891b`) |
| YM2612 / YM3438 | [ym2612-rust](https://github.com/alewman/ym2612-rust) | `3061637` (= ym2612-python `2c2ab23`) |
| SN76489 (the VDP's PSG) | [sn76489-rust](https://github.com/alewman/sn76489-rust) | `c5fc09e` (= sn76489-python `9ed3b85`, the chip of the board's `b0acbc8`) |

Why it exists: the Python board is the readable reference and is slow by design. This
board is the same machine at a speed that can be played, so cartridges can be tested
on our own emulator.

`src/` is the reference's `src/megadrive_python/` module for module:
- `machine`, `arbiter`, `io`, `eeprom`, `cartridge`, `render`, `vdp`, `board`,
  `system`, `audio`, `wav` and `png`;
- the command line as `md-run`.

Each keeps its names, its order and its citations. Nothing is redesigned. Where Rust
forced a difference, the module header says what it is ([docs/handoff-brief.md](docs/handoff-brief.md),
"The shape Rust forces"). The reference's docs (`docs/start-here.md`,
`docs/timing.md`, `docs/undocumented-behavior.md`, `docs/validation.md`) are the
documentation of what is modelled and why.

## Running a cartridge

```text
md-run CART [--machine genesis] [--frames 600] [--png frame.png] [--wav sound.wav]
       [--no-sound] [--press start --press-at 300] [--pad 6button] [--save game.srm]
       [--tmss-rom BOOT.zip]
```

The machines are the reference's: `genesis`, `genesis-tmss`, `genesis2`, `genesis3`,
`megadrive`, `megadrive-tmss`, `megadrive2`, `megadrive-japan`, `megadrive2-japan`,
and `nuked-md` (the die's DMA figures). The board comes from MAME's software list
when `/usr/share/games/mame/hash/megadriv.xml` is installed, else from the header, as
in the reference.

## Playing in a window

```text
cargo build --release --features frontend      # needs SDL2 (libsdl2-dev)
target/release/md-play CART [--machine NAME] [--scale 3] [--no-audio] [--no-fm]
       [--skip N] [--pad 6button] [--pad2 none] [--layout snes] [--save game.srm]
       [--tmss-rom BOOT.zip] [--volume 12] [--fullscreen]
```

| Key | Does | Key | Does |
| --- | --- | --- | --- |
| Arrows | D-pad | Z X C, or J K L | A B C |
| Enter | Start | A S D | X Y Z |
| Q | Mode | Backspace | the reset button |
| Tab (held) | fast forward | F3 | pause |
| F5 | power cycle | F11 | fullscreen |
| F12 | screenshot to `screenshots/` | Esc | quit |
| `-` `=` | volume down, up (3 dB) | | |

The window plays 12 dB louder than the board's mix (`--volume DB`, from -30 to 24;
`-` and `=` step it). The mix leaves room for every channel at full volume at once,
which no game comes near (fourteen titles peak 10.5 to 23 dB below full scale), so a
soft clamp rounds off the rare peak that would go over: untouched to -3 dBFS, bent
toward full scale above. Only the window: `md-run`'s WAVs and the traces are the mix
unchanged.

A game controller works too, on port 1 with the keyboard and the second on port 2.
Buttons go by position, whatever their letters: A, B, C are the left, bottom and
right face buttons (X, A, B on an Xbox pad; Y, B, A on a Nintendo one), and the
6-button pad's X, Y, Z are left shoulder, the top face button and right shoulder.
`--layout snes` is the Super Nintendo fighting layout instead: X, Y, Z (the punches in
Street Fighter II) on the left and top face buttons and left shoulder, A, B, C (the
kicks) on the bottom and right face buttons and right shoulder. Start is Start and
Mode is Back. Without `--pad` port 1 has the pad the header's I/O field asks for, the
6-button pad if it lists "6"; without `--machine` the machine comes from the header's
country codes: the USA's Genesis first, then a PAL Mega Drive, then a Japanese one.

`md-play` is the reference's `frontend.py` (`src/frontend.rs` and `src/play.rs`) on
SDL2 itself instead of pygame-ce. The sound queue sets the pace, so the console runs at
its own 59.92 Hz (49.70 Hz PAL). Unlike the reference on PyPy, this board has the speed
for it with the YM2612 on. The title bar shows the frame, the emulated time and the
speed. The window needs the `frontend` feature, so the gate and CI build without SDL.
`cargo test --features frontend` also runs the window headless, under SDL's dummy
drivers.

## Speed

`md-run --frames 1800`, one core of a shared i9-13900K (load average about 30),
2026-10-11:

| Cartridge | With the YM2612 | `--no-sound` |
| --- | ---: | ---: |
| Overdrive 2 | 376 fps, 6.3x real time | 522 fps |
| Streets of Rage 2 | 281 fps, 4.7x | 384 fps |
| Gunstar Heroes | 259 fps, 4.3x | 323 fps |
| Sonic the Hedgehog | 246 fps, 4.1x | 348 fps |

The reference runs these at 10-20 fps on PyPy with sound. Nothing has been optimized
yet: this is the transcription, and speed is a `-fast` fork's job, as for the cores.

## The gate

The reference is megadrive-python at `megadrive-rust-pin-1`, on PyPy 7.3.23 with
exactly the four cores its `pyproject.toml` pins. Both boards run the same cartridge
with the same inputs and write the same trace (`src/trace.rs`, `scripts/md_trace.py`).
After every frame a trace records:
- both CPUs' full state;
- a CRC-32 of each memory (work RAM, Z80 RAM, VRAM, CRAM, VSRAM, the battery);
- every field of the VDP (registers, command, FIFO with commit times, read buffer, DMA,
  interrupts, counters, slot position, line events, scroll latches);
- the picture as CRAM indices and as RGB;
- the FM's and PSG's samples, and the analogue mix's output;
- the board's clocks (bank, bus request, both refreshes, the arbiter's grid).

A case passes when the two traces are identical, byte for byte.

`python3 scripts/lockstep.py`, 2026-10-11, at this repository's first commit:
**37 of 37 cases identical**, 69,900 frames (about 19 emulated minutes).

| Rung | Cases | Frames |
| --- | --- | ---: |
| The reference's test ROMs | VDPFIFOTesting on four machines; Nemesis's sprite masking ROM; MoD's `gen_test_z80_bus`; r57shell's VDP tests and `joy_speed` (6-button pad); Sega's MD Soft Checker; Nemesis's six YM2612 test ROMs; the 240p Test Suite | 12,600 |
| Cold boots | Overdrive 2 for 9,000 frames, and Sonic the Hedgehog, Streets of Rage 2 and Gunstar Heroes for 3,600 each with play scripted on the pad, each on the default machine and on `nuked-md` (the die's DMA figures and FM lags) | 39,600 |
| Boards | Sonic 3 (FRAM), NBA Jam and Brian Lara Cricket 96 (EEPROM, J-Cart), Phantasy Star IV (battery RAM), Super Street Fighter II (the Sega mapper), Castlevania Bloodlines, and the 5 MB Rickroll cartridge (the mapper found by its own code) | 11,700 |
| Machines and buttons | The TMSS boot ROM handing off to Sonic; Sonic on a PAL Mega Drive and a Japanese Mega Drive 2 (the ASIC FM); the reset button, twice in one run and once after a TMSS boot; `--no-fm` with a 6-button pad and nothing on port 2 | 6,000 |

Run 14 at a time on a shared machine, the reference took from 26 seconds to 55
minutes a case, and this board 2 to 73 seconds.

**A gate that cannot fail proves nothing.** `scripts/seeded_faults.py` breaks the board
in 24 places, one at a time, in a copy of the repository. Each breakage is a constant
moved by one step or a rule bent, across every module. A broken board is checked against
all 37 cases (at most 900 frames each) and against `cargo test`. **All 24 are caught.** Each is caught by 1 to 42 of the 42 checks; the record is in
[docs/build-record.md](docs/build-record.md).

`cargo test` (also CI) needs neither ROMs nor Python:
- **The examples.** `tests/examples/` holds four runs of three cartridges of our own,
  assembled by `scripts/make_examples.py`. They cover DMA, fill and copy, both
  interrupts, raster writes to CRAM and VSRAM, a Z80 driving the FM and the PSG through
  every path to the 68000's side, sprites under shadow/highlight, the window, and 2-cell
  and full-screen scroll. Each run is replayed against the reference's committed trace.
- **The EEPROM vectors.** Three files of bit-banged EEPROM transactions, update by
  update, one per addressing mode.

## Reproducing

```text
scripts/fetch_megadrivepython.sh    # the reference at the tag, PyPy venv, the four pinned cores
cargo build --release
python3 scripts/lockstep.py         # every case of scripts/cases.tsv, both boards, compared
python3 scripts/seeded_faults.py    # after lockstep.py: each fault must be caught
python3 scripts/make_examples.py    # rewrite tests/examples/ from the reference
```

`scripts/md_trace.py` is the reference's half of the trace; `md-trace` is this
board's. They take the same arguments, so one case can be rerun by hand. `--steps A:B`
adds a line per 68000 step, to narrow a difference down. ROMs and traces are never
committed: `$ROMS`, `$TEST_ROMS` and `$EXTRA_ROMS` say where the local ones are.

## What differs from the reference

These paths the reference reaches only through reentrancy are not imitated:
- **The Z80 writing $A11100 or $A11200 through its bank window.** The reference
  probes and resets the Z80 from inside its own step. Here the run stops with a
  message, as a lock-up does.
- **A 68000-to-VDP DMA whose source is the Z80's space or $A1xxxx.** It reads what
  the reference's would, but without first running the Z80 up to the read.

No case of the gate reaches either. m68000-rust needed two hooks for the board, added by
its PR #1 (merged as `ce3b489`):
- `Cycle::irc`: the reference's board reads the CPU's IRC inside a bus callable, for
  open bus.
- `Bus::acknowledge_clock_credit`: it adds its holds to the CPU's clock inside the
  acknowledge, for the E clock.

The default credit is 0, so every other host is unchanged.

## License

MIT, like the reference. The emulators and die models the reference cites (Nuked-MD,
Nuked-OPN2, Genesis Plus GX, BlastEm, MAME, Exodus) are cited through it and never read
into this repository.
