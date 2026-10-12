#!/usr/bin/env python3
"""Write ``tests/examples/eeprom-*.txt``: the reference's serial EEPROM driven through
bit-banged transactions (writes longer than a page, at random addresses, sequential
reads with and without the master's acknowledge, transactions abandoned part way), one
line per update of the two lines: ``scl sda out``, then the chip's memory as hex.
``tests/eeprom.rs`` replays them. No game in the gate writes across a page, so this is
what holds the page wrap and the three addressing modes. Run with the reference's
interpreter; stdlib only."""

from __future__ import annotations

import random
from pathlib import Path

from megadrive_python.eeprom import MODE1, MODE2, MODE3, SerialEEPROM

OUT = Path(__file__).resolve().parent.parent / "tests/examples"
CHIPS = {"mode1": (MODE1, 0x7F, 0x03), "mode2": (MODE2, 0x7FF, 0x0F), "mode3": (MODE3, 0x1FFF, 0x07)}


def main() -> None:
    for name, (mode, size_mask, page_mask) in CHIPS.items():
        chip = SerialEEPROM(mode, size_mask, page_mask)
        rng = random.Random(name)
        lines: list[str] = []
        scl, sda = 1, 1

        def set_lines(c: int, d: int) -> None:
            nonlocal scl, sda
            scl, sda = c, d
            chip.update(c, d)
            lines.append(f"{c} {d} {chip.read_sda()}")

        def start() -> None:
            set_lines(1, 1)
            set_lines(1, 0)
            set_lines(0, 0)

        def stop() -> None:
            set_lines(0, 0)
            set_lines(1, 0)
            set_lines(1, 1)

        def send(byte: int) -> None:
            for bit in range(7, -1, -1):
                set_lines(0, (byte >> bit) & 1)
                set_lines(1, (byte >> bit) & 1)
                set_lines(0, (byte >> bit) & 1)
            set_lines(0, 1)  # release for the acknowledge
            set_lines(1, 1)
            set_lines(0, 1)

        def receive(ack: bool) -> None:
            for _ in range(8):
                set_lines(0, 1)
                set_lines(1, 1)
            set_lines(0, 0 if ack else 1)
            set_lines(1, 0 if ack else 1)
            set_lines(0, 0 if ack else 1)

        def address(at: int, read: bool) -> None:
            if mode == MODE1:
                send(((at & 0x7F) << 1) | read)
            elif mode == MODE2:
                send(0xA0 | ((at >> 8) & 7) << 1 | read)
                if not read:
                    send(at & 0xFF)
            else:
                send(0xA0 | read)
                if not read:
                    send((at >> 8) & 0xFF)
                    send(at & 0xFF)

        for _ in range(60):
            at = rng.randrange(size_mask + 1)
            start()
            address(at, False)
            for _ in range(rng.randrange(1, 3 * (page_mask + 1))):
                send(rng.randrange(256))
            if rng.random() < 0.2:
                continue  # abandoned: no STOP, the next START begins again
            stop()
            start()
            if mode != MODE1 and rng.random() < 0.5:
                address(rng.randrange(size_mask + 1), False)  # set the counter, then read
                start()
            address(at, True)
            count = rng.randrange(1, 12)
            for i in range(count):
                receive(i + 1 < count)
            stop()
        lines.append(bytes(chip.memory).hex())
        (OUT / f"eeprom-{name}.txt").write_text("\n".join(lines) + "\n")
        print(f"eeprom-{name}: {len(lines) - 1} updates, {chip.writes} bytes written")


if __name__ == "__main__":
    main()
