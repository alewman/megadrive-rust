# Handoff brief: megadrive-rust

## The request

Aubrey, 2026-10-11: "go ahead with megadrive-rust, public". It is step 5 of the plan
agreed on 2026-10-10, quoted here as it was given:

1. Bring z80-rust up to b62891b.
2. ym2612-python core, then ym2612-rust.
3. Decide the PSG.
4. Freeze a megadrive-python pin and transcribe the board.
5. Gate it by lockstep against megadrive-python.

Steps 1-4 are done. The four chip cores exist in Rust, each level with the version
the board pins. The board is frozen as the annotated tag `megadrive-rust-pin-1` on
megadrive-python `d7882e1`. Why do this at all: the Python board is a reference and
is slow by design. Aubrey wants "our emulator" (not BlastEm) to test the cartridges
being built, so it needs a form that runs at speed.

## What this repository is

A transcription of megadrive-python's board (`src/megadrive_python/`) at the pin. It
runs on the family's Rust cores:

| Python core pin (the board's) | Rust core | Rust pin |
| --- | --- | --- |
| m68000-python `38dbd0f` | m68000-rust | `board-hooks` (`b3df005` + the two hooks below) |
| z80-python `b62891b` | z80-rust | `b62891b` (main `3b15c1d`) |
| ym2612-python `2c2ab23` | ym2612-rust | `2c2ab23` (main `3061637`) |
| sn76489-python `b0acbc8` | sn76489-rust | `9ed3b85`; the chip is identical (main `c5fc09e`) |

Module for module: `machine`, `arbiter`, `io`, `eeprom`, `cartridge`, `render`,
`vdp`, `board`, `system`, `audio`, `wav`, `png`, and the command line
(`__main__.py`). They keep the same names, the same order and the same citations.
Nothing is redesigned. Where Rust forced a difference, the module header says what
it is.

Not transcribed:
- `frontend.py`, the pygame window. It is a later step: a Rust window needs a choice
  of library.
- The validation scripts. They are the reference's. This port is judged by lockstep
  with the reference, which carries those results over.

## The shape Rust forces

Each Rust core owns its bus (`M68000<B>`, `Z80<B>`). The Python board instead hands
callables to both CPUs and calls back into the system from inside them. Here:

- `System` owns `M68000<Md>`. `Md` is the 68000's bus. It owns the Z80, whose bus is
  the `Board`, plus the scheduler state that megadrive-python keeps on `System`.
  Everything the Python board does through `catch_up_z80`, `z80_busack`,
  `on_z80_reset` and `on_z80_resume` is a method of `Md`, which can reach both the
  Z80 and the board.
- `Board` splits into `vdp` and `dev` (everything else), so the VDP can be handed
  the devices it reaches into (DMA reads, the PSG, the two refresh clocks, open bus)
  as a separate borrow.
- The Z80 probe (`System.z80_probe`) swaps the Z80's bus callables in Python. Here
  the board carries a probe mode, which its Z80 read and write consult first.
- `Lockup`, raised out of a CPU step in Python, is recorded by the bus and returned
  by `System::step` as an error once the step ends.

m68000-rust needed two hooks, on its branch `board-hooks` (`e1b8000`):
- `Cycle::irc`. The Python board reads `cpu.irc` inside a callable, for open bus.
- `Bus::acknowledge_clock_credit`. The Python board adds its holds to `cpu.clock`
  inside the acknowledge callable, for the E clock.

The default credit is 0, so nothing else moves. Both hooks go to m68000-rust as a PR.

Paths the Python board reaches only through reentrancy are recorded as limitations
rather than imitated:
- The Z80 writing $A11100 or $A11200 through its bank window: Python probes and
  resets the Z80 from inside its own step.
- A DMA whose source is the Z80's space or $A1xxxx: Python may run the Z80 forward
  from inside the VDP.

## The gate

The reference is megadrive-python at `megadrive-rust-pin-1`, run on PyPy with exactly
the four pinned cores. Both boards write the same trace for the same cartridge:
- one line per frame: the clocks, both CPUs' registers, and digests of every memory,
  the VDP's state, the picture, and both sound chips' samples;
- one line per 68000 step, for narrowing a difference down.

A rung passes when the two traces are identical. The long rungs are the cold boots
megadrive-python is judged on: Overdrive 2, Streets of Rage 2, Gunstar Heroes, and
Sonic the Hedgehog. A gate that cannot fail proves nothing, so seeded faults check
that it bites.

## Working rules carried over

- Never copy GPL/AGPL/LGPL/non-commercial code: read it, cite it, write our own.
- Never commit ROMs, BIOS, recordings, or traces derived from ROMs.
- Push and open PRs only when Aubrey asks; Aubrey merges.
- Use `nice`, and at most 16 processes at once on the shared machine.
- Keep CI small: main and PRs, with concurrency cancel.
