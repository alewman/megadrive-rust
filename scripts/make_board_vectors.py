#!/usr/bin/env python3
"""Every cartridge board against the reference's, access by access, for tests/boards.rs.

Games reach a small part of a mapper in the frames a case runs, and some boards have no
local dump at all, so the lockstep alone cannot speak for them. Here each class of the
reference's ``BOARDS`` (but the SVP, which is clocked and has its own cases) takes the same
stream of bus accesses in three image sizes, and the file records what it answered. The
addresses aim at the numbers in the class's own source; Pier Solar's EEPROM also gets whole
SPI transactions. Run with the reference's interpreter:

    external/megadrive-python-pin2/.venv/bin/python scripts/make_board_vectors.py \\
        > tests/examples/boards.txt
"""

import inspect
import random
import re
import sys
import zlib

from megadrive_python.cartridge import BOARDS, Cartridge
from megadrive_python.mappers import Board

SIZES = (0x100000, 0x400000, 0x800000)
ACCESSES = 1500
WINDOW = 0x800000


def image(size, multiplier):
    return bytes(((i * multiplier) >> 16) & 0xFF for i in range(size))


def literals(cls):
    """The numbers a board's source mentions: where to aim, and what to write."""
    found = set()
    for klass in cls.__mro__:
        if klass is Board or not issubclass(klass, Board):
            continue
        found |= set(klass.CONSTANTS) | set(klass.TIME_WORDS)
        try:
            source = inspect.getsource(klass)
        except OSError:  # a class _constants() or _time_words() made
            continue
        found |= {int(n, 16) for n in re.findall(r"0x([0-9A-Fa-f]+)", source)}
    return sorted(found)


class Stream:
    def __init__(self, cartridge):
        self.cart = cartridge
        self.lines = []

    def read(self, op, method, address):
        # A read past the end of an image too small for its board is an index error here
        # and $FF in Rust, a difference the README states: not a case.
        try:
            self.lines.append(f"{op} {address:X} {method(address):X}")
        except IndexError:
            pass

    def read_word(self, address):
        self.read("R", self.cart.read_word, address)

    def read_byte(self, address):
        self.read("B", self.cart.read_byte, address)

    def dma(self, address):
        # The board's DMA path: only the SVP has its own, and it is not here.
        self.read("D", self.cart.read_word, address)

    def write_word(self, address, value):
        self.cart.write_word(address, value)
        self.lines.append(f"W {address:X} {value:X}")

    def write_byte(self, address, value):
        self.cart.write_byte(address, value)
        self.lines.append(f"Y {address:X} {value:X}")

    def time_read(self, address):
        word = self.cart.read_time_word(address)
        self.lines.append(f"T {address:X} {'-' if word is None else f'{word:X}'}")

    def time_write(self, address, high, low, word):
        self.cart.write_time_lanes(address, high, low, word)
        self.lines.append(f"L {address:X} {high:X} {low:X} {int(word)}")

    def reset(self):
        self.cart.reset()
        self.lines.append("X")


def spi(stream, data, listen=0):
    """/CS low, the bytes in most significant bit first, ``listen`` bytes read back."""

    def pins(si, sck, cs):
        value = si | (sck << 1) | (cs << 3)
        stream.time_write(0xA13009, value, value, False)

    pins(0, 0, 0)
    for byte in data:
        for bit in range(7, -1, -1):
            si = (byte >> bit) & 1
            pins(si, 0, 0)
            pins(si, 1, 0)
    for _ in range(listen * 8):
        pins(0, 0, 0)
        pins(0, 1, 0)
        stream.time_read(0xA1300A)
    pins(0, 0, 1)


def eeprom_transactions(stream, rng):
    at = rng.randrange(0x1000)
    data = [rng.randrange(256) for _ in range(rng.randrange(1, 40))]
    high, low = at >> 8, at & 0xFF
    if rng.random() < 0.8:
        spi(stream, [0x06])
    spi(stream, [0x05], 1)
    spi(stream, [0x02, high, low, *data])
    spi(stream, [0x03, high, low], len(data))
    spi(stream, [0x05], 1)
    if rng.random() < 0.3:
        spi(stream, [0x04])
    if rng.random() < 0.3:
        spi(stream, [0x01, rng.randrange(256)])


def run(stream, slot, cls, rng):
    pool = literals(cls)
    near = [n for n in pool if 0x100 <= n < WINDOW]
    time = [n for n in pool if n < 0x100] or [0]
    values = [n for n in pool if n < 0x10000] or [0]

    def address():
        pick = rng.random()
        if near and pick < 0.65:
            return (rng.choice(near) + rng.randrange(-2, 4)) & (WINDOW - 1)
        if pick < 0.75:
            return rng.randrange(0x40)
        return rng.randrange(WINDOW)

    def value(top):
        return (rng.choice(values) if rng.random() < 0.3 else rng.randrange(0x10000)) & top

    for _ in range(ACCESSES):
        kind = rng.random()
        at = address()
        offset = (rng.choice(time) if rng.random() < 0.6 else rng.randrange(0x100)) & 0xFF
        if kind < 0.27:
            stream.read_word(at & ~1)
        elif kind < 0.40:
            stream.read_byte(at)
        elif kind < 0.47:
            stream.dma(at & ~1)
        elif kind < 0.60:
            stream.write_word(at & ~1, value(0xFFFF))
        elif kind < 0.76:
            stream.write_byte(at, value(0xFF))
        elif kind < 0.83:
            stream.time_read(0xA13000 | (offset & 0xFE))
        elif kind < 0.99:
            word = rng.random() < 0.3
            high, low = value(0xFF), value(0xFF)
            if not word:
                high = low
            stream.time_write(0xA13000 | (offset & 0xFE if word else offset), high, low, word)
        else:
            stream.reset()
        if slot == "rom_stm95" and rng.random() < 0.01:
            eeprom_transactions(stream, rng)


def main():
    rng = random.Random(0x5E6A)
    images = {size: image(size, 0x9E3779B1) for size in SIZES}
    second = image(0x400000, 0x85EBCA6B)  # 2 MB would mirror over the shift
    patch = image(0x40000, 0xC2B2AE35)
    out = sys.stdout
    for slot, cls in sorted(BOARDS.items()):
        if slot == "rom_svp":
            continue
        for size in SIZES:
            cartridge = cls(images[size], slot=slot, name="")
            socket = hasattr(cartridge, "attach")
            if socket:
                cartridge.attach(Cartridge.from_image(second),
                                 *([patch] if size == 0x400000 and slot == "rom_sk" else []))  # fmt: skip
            stream = Stream(cartridge)
            run(stream, slot, cls, rng)
            out.write(f"S {slot} {size:X}\n")
            out.write("\n".join(stream.lines) + "\n")
            out.write(f"C {zlib.crc32(cartridge.save_bytes()) & 0xFFFFFFFF:X}\n")


if __name__ == "__main__":
    main()
