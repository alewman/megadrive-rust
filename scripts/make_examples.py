#!/usr/bin/env python3
"""Write the example cartridges in ``tests/examples/``: programs of our own, assembled
here word by word, so the board can be run end to end without any ROM.

``board-tour.bin`` sets up the VDP (H40, both planes, the window off, a horizontal
interrupt every 16 lines changing CRAM, a vertical one scrolling plane A), runs a
68000-to-VRAM DMA from its own ROM, a VRAM fill and a VRAM copy, writes VSRAM, loads
a Z80 program through the bus request and starts it, then loops reading the HV
counter and taking the Z80's bus to read its RAM. The Z80 program writes the YM2612
(a sounding FM voice) and the PSG, then loops through the bank window, the FM's
status, the VDP through $7F08 and its own RAM, taking a VINT interrupt in IM 1.

``h32-sprites.bin`` (H32, PAL in its trace) puts sprites up with shadow/highlight,
the window over plane A and 2-cell vertical scroll, and moves them each frame.
``h32-raster.bin`` is the same with full-screen scroll and a main loop writing plane
B's VSRAM word at every phase of every line, which reaches the vertical scroll latch.

The expected traces are the reference's: ``scripts/md_trace.py`` writes them
(``--examples-traces``), and ``tests/examples.rs`` replays each cartridge through
``md-trace``'s logic and compares. Stdlib only.
"""

from __future__ import annotations

import argparse
import struct
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "tests/examples"


class Asm:
    """A 68000 program as words, with labels resolved on a second pass."""

    def __init__(self, origin: int) -> None:
        self.origin = origin
        self.words: list[int | tuple] = []
        self.labels: dict[str, int] = {}

    @property
    def pc(self) -> int:
        return self.origin + 2 * len(self.words)

    def label(self, name: str) -> None:
        self.labels[name] = self.pc

    def w(self, *words: int) -> None:
        self.words.extend(words)

    def l(self, value: int) -> None:  # noqa: E743
        self.w((value >> 16) & 0xFFFF, value & 0xFFFF)

    def rel16(self, name: str) -> None:
        """A 16-bit displacement to `name` from this extension word's address."""
        self.words.append(("rel16", name, self.pc))

    def bra8(self, opcode: int, name: str) -> None:
        """A short branch: opcode | 8-bit displacement from the opcode's address + 2."""
        self.words.append(("bra8", opcode, name, self.pc))

    def bytes(self) -> bytes:
        out = bytearray()
        for word in self.words:
            if isinstance(word, tuple):
                if word[0] == "rel16":
                    word = (self.labels[word[1]] - word[2]) & 0xFFFF
                else:
                    displacement = self.labels[word[2]] - (word[3] + 2)
                    assert -128 <= displacement < 128 and displacement != 0
                    word = word[1] | (displacement & 0xFF)
            out += struct.pack(">H", word)
        return bytes(out)


def vdp_reg(a: Asm, number: int, value: int) -> None:
    a.w(0x30BC, 0x8000 | (number << 8) | value)  # move.w #$8nvv,(a0)


def control(a: Asm, value: int) -> None:
    a.w(0x20BC)  # move.l #value,(a0)
    a.l(value)


def wait_dma(a: Asm, name: str) -> None:
    a.label(name)
    a.w(0x3410)  # move.w (a0),d2
    a.w(0x0802, 0x0001)  # btst #1,d2
    a.bra8(0x6600, name)  # bne


def take_bus(a: Asm, name: str) -> None:
    a.w(0x33FC, 0x0100)  # move.w #$100,$A11100
    a.l(0xA11100)
    a.label(name)
    a.w(0x0839, 0x0000)  # btst #0,$A11100
    a.l(0xA11100)
    a.bra8(0x6600, name)


def give_bus(a: Asm) -> None:
    a.w(0x33FC, 0x0000)  # move.w #0,$A11100
    a.l(0xA11100)


def cartridge(program: Asm, z80: bytes, vectors: dict[int, int], data: bytes) -> bytes:
    rom = bytearray(b"\xff" * 0x2000)
    struct.pack_into(">II", rom, 0, 0x00FFFE00, 0x200)
    for vector in range(2, 64):
        struct.pack_into(">I", rom, vector * 4, vectors.get(vector, 0x200))
    rom[0x100:0x110] = b"SEGA GENESIS    "
    code = program.bytes()
    rom[0x200 : 0x200 + len(code)] = code
    rom[0x1000 : 0x1000 + len(data)] = data
    rom[0x1800 : 0x1800 + len(z80)] = z80
    return bytes(rom)


def z80_program() -> bytes:
    """The Z80's half: an FM voice and a PSG tone, then a loop over every path to the
    68000's side (the bank window, the FM's status, the VDP's HV counter through $7F08,
    the bank register), with a VINT handler in IM 1 counting in its own RAM."""
    table = [
        (0x22, 0x00), (0x27, 0x00), (0x28, 0x00), (0x2B, 0x00),
        (0x30, 0x01), (0x34, 0x02), (0x38, 0x01), (0x3C, 0x01),
        (0x40, 0x10), (0x44, 0x18), (0x48, 0x10), (0x4C, 0x08),
        (0x50, 0x1F), (0x54, 0x1F), (0x58, 0x1F), (0x5C, 0x1F),
        (0x60, 0x05), (0x64, 0x05), (0x68, 0x05), (0x6C, 0x05),
        (0x80, 0x1F), (0x84, 0x1F), (0x88, 0x1F), (0x8C, 0x1F),
        (0xB0, 0x07), (0xB4, 0xC0), (0xA4, 0x22), (0xA0, 0x69), (0x28, 0xF0),
    ]  # fmt: skip
    code = bytearray(0x100)

    def put(at: int, *data: int) -> None:
        code[at : at + len(data)] = bytes(data)

    put(0x00, 0xF3)  # di
    put(0x01, 0x31, 0x00, 0x1F)  # ld sp,$1F00
    put(0x04, 0xED, 0x56)  # im 1
    put(0x06, 0x21, 0xC0, 0x00)  # ld hl,$00C0 (the table)
    put(0x09, 0x11, 0x00, 0x40)  # ld de,$4000
    put(0x0C, 0x06, len(table))  # ld b,count
    put(0x0E, 0x7E, 0x12, 0x23, 0x13)  # ld a,(hl); ld (de),a; inc hl; inc de
    put(0x12, 0x7E, 0x12, 0x23, 0x1B)  # ld a,(hl); ld (de),a; inc hl; dec de
    put(0x16, 0x10, 0xF6)  # djnz $000E
    psg = (0x8E, 0x0F, 0x92, 0xE5, 0xF4)  # tone 0 period, volume; white noise, volume
    for i, value in enumerate(psg):
        put(0x18 + 5 * i, 0x3E, value, 0x32, 0x11, 0x7F)  # ld a,v; ld ($7F11),a
    put(0x31, 0xFB)  # ei
    put(0x32, 0xC3, 0x50, 0x00)  # jp $0050
    # RST 38, the IM 1 handler: count interrupts at $0100.
    put(0x38, 0xF5, 0x3A, 0x00, 0x01, 0x3C, 0x32, 0x00, 0x01)  # push af; inc ($0100)
    put(0x40, 0xF1, 0xFB, 0xED, 0x4D)  # pop af; ei; reti
    # The loop.
    put(0x50, 0x3A, 0x00, 0x80)  # ld a,($8000)  the bank window (ROM)
    put(0x53, 0x3A, 0x00, 0x40)  # ld a,($4000)  the FM's status
    put(0x56, 0x3A, 0x08, 0x7F)  # ld a,($7F08)  the HV counter through the VDP port
    put(0x59, 0x32, 0x02, 0x01)  # ld ($0102),a
    put(0x5C, 0xAF, 0x32, 0x00, 0x60)  # xor a; ld ($6000),a  a bank bit (0: stays ROM)
    put(0x60, 0x3A, 0x01, 0x40)  # ld a,($4001)
    put(0x63, 0xC3, 0x50, 0x00)  # jp $0050
    for i, (register, value) in enumerate(table):
        put(0xC0 + 2 * i, register, value)
    return bytes(code)


def tmss_unlock(a: Asm) -> None:
    a.w(0x23FC)  # move.l #'SEGA',$A14000: TMSS consoles lock the VDP until it is written
    a.l(0x53454741)
    a.l(0xA14000)


def board_tour() -> bytes:
    a = Asm(0x200)
    a.w(0x46FC, 0x2700)  # move #$2700,sr
    tmss_unlock(a)
    a.w(0x41F9)
    a.l(0xC00004)  # lea $C00004,a0
    a.w(0x43F9)
    a.l(0xC00000)  # lea $C00000,a1
    for number, value in ((0, 0x04), (1, 0x74), (2, 0x30), (3, 0x2C), (4, 0x07), (5, 0x78),
                          (7, 0x00), (10, 0x0F), (11, 0x00), (12, 0x81), (13, 0x3F),
                          (15, 0x02), (16, 0x01), (17, 0x00), (18, 0x00)):  # fmt: skip
        vdp_reg(a, number, value)
    control(a, 0xC0000000)  # CRAM 0
    a.w(0x7E1F)  # moveq #31,d7
    a.label("cram")
    a.w(0x3007)  # move.w d7,d0
    a.w(0xE948)  # lsl.w #4,d0
    a.w(0x8047)  # or.w d7,d0
    a.w(0x3280)  # move.w d0,(a1)
    a.w(0x51CF)
    a.rel16("cram")  # dbra d7
    # 68000 -> VRAM DMA: $400 words from ROM $1000 to VRAM 0.
    for number, value in ((19, 0x00), (20, 0x04), (21, 0x00), (22, 0x08), (23, 0x00)):
        vdp_reg(a, number, value)
    control(a, 0x40000080)
    wait_dma(a, "dma1")
    # VRAM fill: $100 bytes of $AB at $8000.
    for number, value in ((19, 0x00), (20, 0x01), (23, 0x80)):
        vdp_reg(a, number, value)
    control(a, 0x40000082)
    a.w(0x32BC, 0xAB00)  # move.w #$AB00,(a1)
    wait_dma(a, "dma2")
    # VRAM copy: $40 bytes from 0 to $A000.
    for number, value in ((19, 0x40), (20, 0x00), (21, 0x00), (22, 0x00), (23, 0xC0)):
        vdp_reg(a, number, value)
    control(a, 0x200000C2)  # code $30: CD5 and CD4
    wait_dma(a, "dma3")
    # Name tables: plane A at $C000 points at tiles 0-63 with palette lines.
    control(a, 0x40000003)  # VRAM $C000
    a.w(0x7E3F)  # moveq #63,d7
    a.label("names")
    a.w(0x3007)  # move.w d7,d0
    a.w(0x0040, 0x2000)  # ori.w #$2000,d0
    a.w(0x3280)  # move.w d0,(a1)
    a.w(0x51CF)
    a.rel16("names")
    control(a, 0x40000010)  # VSRAM 0
    a.w(0x32BC, 0x0010, 0x32BC, 0x0020)
    # The Z80: request the bus, release its reset (the bus is granted only to a Z80
    # out of reset), wait for the grant, copy its program, start it.
    a.w(0x33FC, 0x0100)
    a.l(0xA11100)
    a.w(0x33FC, 0x0100)
    a.l(0xA11200)
    a.label("bus1")
    a.w(0x0839, 0x0000)
    a.l(0xA11100)
    a.bra8(0x6600, "bus1")
    a.w(0x45F9)
    a.l(0x1800)  # lea $1800,a2
    a.w(0x47F9)
    a.l(0xA00000)  # lea $A00000,a3
    a.w(0x303C, 0x00FF)  # move.w #$FF,d0
    a.label("copy")
    a.w(0x16DA)  # move.b (a2)+,(a3)+
    a.w(0x51C8)
    a.rel16("copy")  # dbra d0
    a.w(0x1839)
    a.l(0xA04000)  # move.b $A04000,d4
    a.w(0x33FC, 0x0000)
    a.l(0xA11200)
    give_bus(a)
    a.w(0x33FC, 0x0100)
    a.l(0xA11200)
    vdp_reg(a, 0, 0x14)  # the horizontal interrupt on
    vdp_reg(a, 15, 0x00)  # no auto-increment: the loop's data writes keep one address
    a.w(0x46FC, 0x2000)  # move #$2000,sr
    a.label("loop")
    a.w(0x3639)
    a.l(0xC00008)  # move.w $C00008,d3
    a.w(0x5281)  # addq.l #1,d1
    a.w(0x3281)  # move.w d1,(a1): VSRAM 2 (the handler's last address), at every phase
    a.w(0x0801, 0x0006)  # btst #6,d1
    a.bra8(0x6700, "loop")  # beq: every 64th pass takes the Z80's bus
    take_bus(a, "bus2")
    a.w(0x1A39)
    a.l(0xA00100)  # move.b $A00100,d5
    a.w(0x13FC, 0x00A0)
    a.l(0xA04000)  # move.b #$A0,$A04000: the FM, from the 68000, at a shifting phase
    a.w(0x13C6)
    a.l(0xA04001)  # move.b d6,$A04001
    a.w(0x1839)
    a.l(0xA04000)  # move.b $A04000,d4: its BUSY flag
    give_bus(a)
    a.w(0x7200)  # moveq #0,d1
    a.w(0x6000)
    a.rel16("loop")
    a.label("hint")
    a.w(0x2F00)  # move.l d0,-(sp)
    a.w(0x20BC)
    a.l(0xC0000000)  # CRAM 0 (control port)
    a.w(0x5246)  # addq.w #1,d6
    a.w(0x3286)  # move.w d6,(a1)
    a.w(0x20BC)
    a.l(0x40020010)  # VSRAM 2, plane B's full-screen word: the main loop writes it too,
    # and on a blank line its writes land after plane B's latch at H $143 (plane A's,
    # at the line's last pixel, no slot follows)
    a.w(0x3286)  # move.w d6,(a1)
    a.w(0x201F)  # move.l (sp)+,d0
    a.w(0x4E73)  # rte
    a.label("vint")
    a.w(0x5287)  # addq.l #1,d7
    a.w(0x20BC)
    a.l(0x7C000003)  # VRAM $FC00, the scroll table
    a.w(0x3287)  # move.w d7,(a1)
    a.w(0x20BC)
    a.l(0x40020010)  # back to VSRAM 2 for the main loop, through the blank lines
    a.w(0x4E73)  # rte
    a.bytes()
    vectors = {28: a.labels["hint"], 30: a.labels["vint"]}
    tiles = bytes((i * 37 + (i >> 5) * 11) & 0xFF for i in range(0x800))
    return cartridge(a, z80_program(), vectors, tiles)


def h32_sprites(raster: bool = False) -> bytes:
    a = Asm(0x200)
    a.w(0x46FC, 0x2700)
    tmss_unlock(a)
    a.w(0x41F9)
    a.l(0xC00004)
    a.w(0x43F9)
    a.l(0xC00000)
    for number, value in ((0, 0x04), (1, 0x64), (2, 0x30), (3, 0x2C), (4, 0x07), (5, 0x78),
                          (7, 0x02), (11, 0x00 if raster else 0x04), (12, 0x08), (13, 0x3F),
                          (15, 0x02),
                          (16, 0x11), (17, 0x85), (18, 0x14)):  # fmt: skip
        vdp_reg(a, number, value)
    control(a, 0xC0000000)
    a.w(0x7E3F)
    a.label("cram")
    a.w(0x3007, 0xE548, 0x8047, 0x3280)  # d0 = d7 << 2 | d7
    a.w(0x51CF)
    a.rel16("cram")
    control(a, 0x40000000)  # VRAM 0: tiles by the CPU this time
    a.w(0x303C, 0x07FF)
    a.label("tiles")
    a.w(0x3200)  # move.w d0,d1
    a.w(0xC2FC, 0x1111)  # mulu #$1111,d1
    a.w(0x3281)  # move.w d1,(a1)
    a.w(0x51C8)
    a.rel16("tiles")
    control(a, 0x40000010)  # VSRAM: 2-cell columns
    a.w(0x7E27)
    a.label("vsram")
    a.w(0x3287)
    a.w(0x51CF)
    a.rel16("vsram")
    a.w(0x46FC, 0x2000)
    a.label("loop")
    if raster:
        # Plane B's full-screen word, written at every phase of every line: in H32 the
        # active slot at H $105 lands after its latch at $103.
        a.w(0x20BC)
        a.l(0x40020010)  # VSRAM 2
        a.w(0x5243, 0x3283)  # addq.w #1,d3; move.w d3,(a1)
    else:
        a.w(0x4E72, 0x2000)  # stop #$2000
    a.w(0x6000)
    a.rel16("loop")
    a.label("vint")
    a.w(0x48E7, 0xE0C0)  # movem.l d0-d2/a0-a1,-(sp)
    a.w(0x5287)  # addq.l #1,d7
    a.w(0x3407, 0x0242, 0x003F)  # d2 = d7 & $3F
    a.w(0x20BC)
    a.l(0x70000003)  # VRAM $F000: the sprite table
    a.w(0x7013)  # moveq #19,d0
    a.label("sprites")
    a.w(0x3200, 0xE749, 0xD242, 0x0641, 0x0090)  # y = d0*8 + d2 + $90
    a.w(0x3281)  # move.w d1,(a1)  y
    a.w(0x7214, 0x9240, 0x0041, 0x0F00)  # size/link: 20 - d0 (the next entry), 4x4 cells
    a.w(0x3281)
    a.w(0x3200, 0xE549, 0x0041, 0x6000)  # pattern d0*4: low priority, palette 3 (62, 63)
    a.w(0x3281)
    a.w(0x3200, 0xE949, 0xD242, 0x0641, 0x0080)  # x = d0*16 + d2 + $80
    a.w(0x3281)
    a.w(0x51C8)
    a.rel16("sprites")
    a.w(0x4CDF, 0x0307)  # movem.l (sp)+,d0-d2/a0-a1
    a.w(0x4E73)
    a.bytes()
    return cartridge(a, bytes(0x100), {30: a.labels["vint"]}, b"")


def h32_raster() -> bytes:
    return h32_sprites(raster=True)


EXAMPLES = {
    # name: (cartridge, frames, md_trace arguments)
    "board-tour": (board_tour, 120, []),
    "board-tour-nuked": (board_tour, 120, ["--machine", "nuked-md"]),
    "h32-sprites": (h32_sprites, 90, ["--machine", "megadrive"]),
    "h32-raster": (h32_raster, 60, []),
}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--python", default=str(REPO / "external/megadrive-python/.venv/bin/python"))
    args = parser.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    for name, (make, frames, extra) in EXAMPLES.items():
        rom = OUT / f"{make.__name__.replace('_', '-')}.bin"
        rom.write_bytes(make())
        trace = OUT / f"{name}.trace"
        subprocess.run([args.python, str(REPO / "scripts/md_trace.py"), str(rom), "--frames",
                        str(frames), *extra, "--out", str(trace)], check=True)  # fmt: skip
        print(f"{name}: {rom.name}, {frames} frames, {trace.stat().st_size} bytes of trace")
    return 0


if __name__ == "__main__":
    sys.exit(main())
