# Build record

2026-10-11, one session, the day the last of the four Rust chip cores was finished
and megadrive-python was frozen as `megadrive-rust-pin-1`.

## The transcription

The reference's board is about 5,400 lines of Python in twelve modules, not counting
the pygame frontend. Each became a Rust module in one pass, in the same order, with
the same names and citations. Only Rust's ownership forced changes: both Rust cores
own their buses, where the Python board hands callables to both CPUs and calls back
into the system from inside them. The handoff brief ("The shape Rust forces") gives
the resulting layout, and each module's header says what changed there.

m68000-rust needed two additions, on its branch `board-hooks` (`e1b8000`),
merged to its main as `ce3b489`, which this board now pins. The core's
own tests and clippy pass with them:
- `Cycle::irc`, for open bus;
- `Bus::acknowledge_clock_credit`, for the E clock after a held push.

The crate compiled on the first build. Its first lockstep run, Sonic the Hedgehog for
300 frames from power-on, was identical to the reference's on the first try. No case
of the gate has differed since.

## The gate

`scripts/lockstep.py`: 37 cases, 69,900 frames, all identical. The README has the table.

Before the full run, the trace was widened to include the analogue mix (`audio.rs`),
which it had left out. The mixed samples are identical too: the float arithmetic is
the reference's, operation for operation.

## The seeded faults

`scripts/seeded_faults.py`, 24 faults. The first run caught 20 of 24. The four misses
were each traced to a cause, not waved away:

| Fault | Why the gate missed it | What closed it |
| --- | --- | --- |
| FM write lag 21 -> 22 MCLK (die machine) | **An equivalent change.** A 68000 access is on the 7-MCLK grid, so 21 and 22 fall in the same FM cycle. No run can tell them apart. | The fault is now 21 -> 28, a 68000 clock later; the examples' 68000 FM writes catch it. |
| The full-screen vertical scroll latch ignores late writes | In H40, plane A's latch is the line's last pixel and no slot follows it. Plane B's at $143 is followed only by idle-line slots, whose latch no visible line uses. In H32 the active slot at $105 follows plane B's at $103. | `h32-raster`: H32, full-screen scroll, plane B's word written at every phase. |
| Colour 15 cancels the shadow | Only low-priority sprite pixels reach the rule. In 900 frames, only Overdrive 2 on the die machine, at frame 4,140. | `h32-sprites`' sprites are low priority, in palette 3 (colours 62 and 63). |
| The EEPROM's page wrap | No game in the gate writes across a page. | `tests/eeprom.rs`: the reference's chip driven by bit-banged transactions in all three modes, update by update. |

The final run caught 24 of 24.

## Pin 2, 2026-10-31

megadrive-python moved 31 commits to `megadrive-rust-pin-2` (`b2d47cb`): the mappers, Pier
Solar, lock-on, the SVP, and the window bug kept to plane A. `ssp1601-rust` is the SVP's core,
transcribed from ssp1601-python `c863a2c`. The expansion sockets were left out, with nothing
here to attach to them.

`scripts/lockstep.py`: 80 cases, 114,000 frames, all identical. No case ever differed;
two lock-on cases had to be rerun for a quoting mistake in `cases.tsv`.

`scripts/seeded_faults.py`, 42 faults. The first run of the 14 new ones caught 7:

| Fault | Why the gate could not see it | What now catches it |
| --- | --- | --- |
| The window bug on both planes | No case has a left window with plane A at a fine scroll and plane B showing through in those frames. | `window-bug`, a cartridge of our own. |
| Tekken's latch, Top Fighter's word writes, Pier Solar's write enable, the 2-in-1 at reset | The games do not reach these in the frames scripted. | `tests/boards.rs`: every board access by access against the reference. |
| The S&K window not shifted | Equivalent with Sonic 3 or any 2 MB cartridge, which mirrors over the shift; the first version of the vectors had the same blind spot. | `tests/boards.rs` with a 4 MB cartridge locked on. |
| The DSP's clock 3/8 | The trace held nothing of the SVP, and a DSP an eighth slower showed the 68000 nothing different in those frames. | The DSP's and memory controller's state in the trace. The SVP cases were rerun with it. |

Four faults on slots with no local dump were added, which only `tests/boards.rs` catches. The
final run caught 42 of 42.

## Speed

The README has the table: 246-376 fps with the FM on, 4-6 times real time, before
any optimization.

## Tooling notes

- Build with `CARGO_TARGET_DIR=target/stable rustup run stable cargo build --release`.
  The default toolchain here is an old nightly.
- For clippy, use the stable rustc and a target directory of its own:
  `CARGO_TARGET_DIR=target/lint RUSTC=$(rustup which --toolchain stable rustc) rustup run stable cargo clippy --release --all-targets -- -D warnings`.
- `scripts/fetch_megadrivepython.sh` clones megadrive-python (private) over SSH.
  `MEGADRIVE_PYTHON_SOURCE` can name a local clone. The script checks that the tag is
  at the pinned commit.
