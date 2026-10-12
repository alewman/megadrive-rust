#!/usr/bin/env python3
"""The lockstep gate: run each case on both boards and compare the traces.

Each case of ``scripts/cases.tsv`` runs ``md-trace`` (this board) and
``scripts/md_trace.py`` on the reference's interpreter, into ``external/traces/``,
several at a time, and the two traces are compared line for line. A case passes when
they are identical; otherwise the first differing line is reported with its frame.

    python3 scripts/lockstep.py [--only NAME ...] [--jobs N] [--frames N]
                                [--rust PATH] [--keep]

``$ROMS``, ``$TEST_ROMS`` and ``$EXTRA_ROMS`` in the case list come from the
environment, defaulting to the local romset, megadrive-python's fetched test ROMs and
the retrobat set. Stdlib only.
"""

from __future__ import annotations

import argparse
import os
import shlex
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
DEFAULTS = {
    "ROMS": "/data/emu/roms/megadrive",
    "TEST_ROMS": "/data/emu/megadrive-python/tests/test_roms",
    "EXTRA_ROMS": "/data/emu/roms-retrobat/megadrive",
}


def expand(text: str) -> str:
    for name, default in DEFAULTS.items():
        text = text.replace(f"${name}", os.environ.get(name, default))
    return text


def cases() -> list[tuple[str, int, str, list[str]]]:
    out = []
    for line in (HERE / "cases.tsv").read_text().splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split("\t")
        name, frames, cart = fields[0], int(fields[1]), expand(fields[2])
        args: list[str] = []
        if len(fields) > 3:
            # Arguments are space-separated; a value with spaces (a path) runs to the
            # next option.
            for token in shlex.split(fields[3].replace("$", "\\$"), posix=True):
                if args and not token.startswith("--") and not args[-1].startswith("--"):
                    args[-1] += " " + expand(token)
                else:
                    args.append(expand(token))
        out.append((name, frames, cart, args))
    return out


def first_difference(a: Path, b: Path) -> str:
    frame = "-"
    with a.open() as left, b.open() as right:
        for number, (x, y) in enumerate(zip(left, right), 1):
            if x.startswith("F "):
                frame = x.split()[1]
            if x != y:
                return f"line {number} (frame {frame}): {x.split()[0]} line differs"
        return "one trace is longer"


def run_case(case, python: Path, rust: Path, out: Path, frames: int | None):
    name, count, cart, args = case
    count = frames or count
    rs, py = out / f"{name}.rs.trace", out / f"{name}.py.trace"
    common = [cart, "--frames", str(count), *args]
    started = time.perf_counter()
    r = subprocess.run(["nice", str(rust), *common, "--out", str(rs)], capture_output=True, text=True)
    rust_s = time.perf_counter() - started
    started = time.perf_counter()
    p = subprocess.run(
        ["nice", str(python), str(HERE / "md_trace.py"), *common, "--out", str(py)],
        capture_output=True, text=True,
    )  # fmt: skip
    python_s = time.perf_counter() - started
    if r.returncode or p.returncode:
        return name, count, "ERROR", (r.stderr + p.stderr).strip()[-300:], rust_s, python_s
    if rs.read_bytes() == py.read_bytes():
        lines = sum(1 for _ in rs.open())
        lock = next((l for l in rs.open() if l.startswith("L ")), "")
        note = f"{lines} lines" + (f"; both stop: {lock.strip()}" if lock else "")
        return name, count, "identical", note, rust_s, python_s
    return name, count, "DIFFERENT", first_difference(rs, py), rust_s, python_s


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--jobs", type=int, default=14)
    parser.add_argument("--frames", type=int, help="override every case's frame count")
    parser.add_argument("--rust", type=Path, default=REPO / "target/release/md-trace")
    parser.add_argument(
        "--python", type=Path, default=REPO / "external/megadrive-python/.venv/bin/python"
    )
    parser.add_argument("--out", type=Path, default=REPO / "external/traces")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    selected = [c for c in cases() if not args.only or c[0] in args.only]
    failed = 0
    with ThreadPoolExecutor(args.jobs) as pool:
        jobs = [pool.submit(run_case, c, args.python, args.rust, args.out, args.frames)
                for c in selected]  # fmt: skip
        for job in jobs:
            name, count, verdict, note, rust_s, python_s = job.result()
            failed += verdict != "identical"
            print(f"{name:18} {count:6} frames  {verdict:9}  rust {rust_s:6.1f} s  "
                  f"python {python_s:7.1f} s  {note}", flush=True)  # fmt: skip
    print(f"{len(selected) - failed} of {len(selected)} identical")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
