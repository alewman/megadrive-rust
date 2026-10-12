#!/usr/bin/env python3
"""Run a cartridge on megadrive-python and write the lockstep trace.

The reference's half of the gate (docs/handoff-brief.md, "The gate"): the same
arguments as ``md-trace`` and the same text, line for line, as
``megadrive_rust::trace`` documents. Run it with the reference's interpreter
(``external/megadrive-python/.venv/bin/python``, from
``scripts/fetch_megadrivepython.sh``).

    md_trace.py CART [--machine NAME] [--frames N] [--press BUTTONS@FROM:TO ...]
                [--reset-at FRAME ...] [--no-fm] [--tmss-rom PATH] [--pad KIND]
                [--pad2 KIND] [--steps A:B] [--out FILE]
"""

from __future__ import annotations

import argparse
import hashlib
import sys
import zlib
from array import array

from megadrive_python.board import Lockup
from megadrive_python.cartridge import Cartridge, load_image
from megadrive_python.io import BUTTONS, PAD_TYPES
from megadrive_python.machine import MACHINES
from megadrive_python.system import System

TRACE_HEADER = "# megadrive-trace 1"


def crc(data: bytes) -> str:
    return f"{zlib.crc32(data) & 0xFFFFFFFF:08X}"


def b(value: object) -> int:
    return 1 if value else 0


def opt(value: int | None) -> str:
    return "-" if value is None else f"{value:X}"


def packed(code: str, values: list) -> bytes:
    data = array(code, values)
    if sys.byteorder == "big":
        data.byteswap()
    return data.tobytes()


def parse_press(spec: str) -> tuple[int, int, int]:
    names, _, frames = spec.partition("@")
    start, _, stop = frames.partition(":")
    buttons = 0
    for name in (n.strip().lower() for n in names.split(",") if n.strip()):
        buttons |= BUTTONS[name]
    return buttons, int(start), int(stop)


def step_line(system: System, index: int, elapsed: int) -> str:
    cpu = system.m68k.capture_state()
    line = f"T {index} {system.mclk} {system.z80_mclk} {elapsed} pc={cpu.pc:06X} sr={cpu.sr:04X}"
    line += "".join(f" {d:X}" for d in cpu.d) + "".join(f" {a:X}" for a in cpu.a)
    return line + f" zpc={system.z80.pc:04X}"


def frame_lines(system: System, index: int) -> str:
    cpu = system.m68k.capture_state()
    z = system.z80.capture_state()
    board, vdp = system.board, system.vdp
    out = [
        f"F {index} {vdp.frame} {system.mclk} {system.z80_mclk} {system.m68k_instructions} "
        f"{system.z80_instructions} {system.z80_deferred}",
        f"C pc={cpu.pc:06X} sr={cpu.sr:04X} ir={cpu.ir:04X} irc={cpu.irc:04X} "
        f"d={','.join(f'{v:X}' for v in cpu.d)} a={','.join(f'{v:X}' for v in cpu.a)} "
        f"usp={cpu.usp:X} ssp={cpu.ssp:X} ipl={cpu.ipl} nmi={b(cpu.nmi_edge)} "
        f"tr={b(cpu.trace_pending)} stop={b(cpu.stopped)} halt={b(cpu.halted)} clk={cpu.clock}",
        f"Z pc={z.pc:04X} sp={z.sp:04X} af={z.a:02X}{z.f:02X} bc={z.b:02X}{z.c:02X} "
        f"de={z.d:02X}{z.e:02X} hl={z.h:02X}{z.l:02X} ix={z.ix:04X} iy={z.iy:04X} "
        f"wz={z.wz:04X} i={z.i:02X} r={z.r:02X} iff={b(z.iff1)}{b(z.iff2)} im={z.im} "
        f"alt={z.af_alt:04X},{z.bc_alt:04X},{z.de_alt:04X},{z.hl_alt:04X} q={z.q:02X} "
        f"halt={b(z.halted)} ei={z.ei_delay} rst={b(z.reset_pending)} "
        f"int={'-' if z.maskable_interrupt_vector is None else f'{z.maskable_interrupt_vector:02X}'} "
        f"nmi={b(z.non_maskable_interrupt_pending)}",
        f"M ram={crc(bytes(board.ram))} zram={crc(bytes(board.z80_ram))} "
        f"vram={crc(bytes(vdp.vram))} cram={crc(packed('H', vdp.cram))} "
        f"vsram={crc(packed('H', vdp.vsram))} bat={crc(board.game_cartridge.save_bytes())}",
    ]
    fifo = ",".join(
        f"{c:X}:{a:X}:{d:X}:{b(h)}{b(p)}:{ready}"
        for (c, a, d, h, p), ready in zip(vdp.fifo, vdp.fifo_ready, strict=True)
    )
    mode = vdp.hmode
    if vdp.slots is mode.slots_active:
        slots = "a"
    elif vdp.slots is mode.slots_first_idle:
        slots = "f"
    else:
        slots = "i"
    out.append(
        f"V regs={bytes(vdp.registers).hex().upper()} code={vdp.code:X} addr={vdp.address:X} "
        f"oaddr={vdp.original_address:X} pend={b(vdp.pending)} mod={b(vdp.modified)} "
        f"fifo={fifo} fr={vdp.fifo_read} fw={vdp.fifo_write} rb={vdp.read_buffer:X} "
        f"ra={b(vdp.read_available)} rh={b(vdp.read_half)} ready={vdp.read_ready_at} "
        f"fill={vdp.read_fill_after} early={vdp.read_early[0]},{vdp.read_early[1]} "
        f"dma={b(vdp.fill_running)}{b(vdp.transfer_active)}{b(vdp.transfer_cached)}:"
        f"{vdp.transfer_value:X}:{vdp.transfer_next_read}:{vdp._read_inc} "
        f"ints={b(vdp.vint_pending)}{b(vdp.hint_pending)}{b(vdp.exint_pending)}{b(vdp.f_flag)} "
        f"odd={b(vdp.odd)} hint={vdp.hint_counter} hv={vdp.hv_latched:X} "
        f"irq={vdp.z80_irq_start},{vdp.z80_irq_end} dbg={vdp.debug_register:X} "
        f"hold={vdp.hold} h={40 if mode.name == 'H40' else 32} vm={vdp.vmode.last:X} "
        f"il={b(vdp.interlace)}{b(vdp.interlace_double)} v={vdp.v:X} pv={vdp.previous_v:X} "
        f"border={b(vdp.border)}{b(vdp._border_started)} ls={vdp.line_start} mclk={vdp.mclk} "
        f"slots={slots}:{vdp.slot_index} done={b(vdp._vint_done)}{b(vdp._odd_done)} "
        f"at={vdp.line_end},{vdp.hblank_at},{vdp.vint_at},{vdp.odd_at},{vdp.render_at} "
        f"vsl={vdp.vscroll_latch[0]:X},{vdp.vscroll_latch[1]:X} "
        f"vsb={opt(vdp._vscroll_before[0])},{opt(vdp._vscroll_before[1])} "
        f"col={','.join(opt(v) for v in vdp._column_before)}"
    )
    renderer = vdp.renderer
    width, height, rows = renderer.frame()
    rgb = packed("I", [pixel for row in renderer.frame_rgb() for pixel in row])
    out.append(f"P {width}x{height} pix={crc(b''.join(bytes(r) for r in rows))} rgb={crc(rgb)}")
    fm = board.fm.take_samples()
    psg = board.psg.take_samples()
    fm_bytes = packed("i", [x for pair in fm for x in pair])
    psg_bytes = packed("d", [x for pair in psg for x in pair])
    # The analogue path (audio.py), fed what the chips finished, as take_audio does.
    mixed = system.audio.mix(fm, [left for left, _right in psg])
    mixed_bytes = packed("i", [x for pair in mixed for x in pair])
    out.append(
        f"S fm={len(fm)}:{crc(fm_bytes)} psg={len(psg)}:{crc(psg_bytes)} "
        f"mix={len(mixed)}:{crc(mixed_bytes)} "
        f"fmclk={board.fm.mclk},{board.fm.cycles_run} psgclk={board.psg_mclk},{board.psg.clock}"
    )
    hold = "-" if board.z80_hold is None else f"{board.z80_hold[0]},{board.z80_hold[1]}"
    partial = system._z80_partial
    partial = "-" if partial is None else f"{partial[0]},{partial[1]}"
    out.append(
        f"B bank={board.bank:X} zres={b(board.z80_reset)} breq={b(board.z80_bus_requested)} "
        f"busack={board.busack_at} cart={board.cart_refresh_due} "
        f"pulse={board.cart_refresh_pulse_end} ztaken={board.z80_refresh_taken} "
        f"ram={board.ram_refresh_due} pend={board.m68k_pending_stall} "
        f"resume={board.m68k_resume_at} end={board.m68k_step_end} "
        f"tmss={board.tmss_register:X},{b(board.tmss_cart_selected)} "
        f"banks={board.bank_accesses},{board.bank_z80_cycles},{board.bank_m68k_cycles} "
        f"zhold={hold} zpart={partial}"
    )
    return "\n".join(out) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("cartridge")
    parser.add_argument("--machine", default="genesis", choices=sorted(MACHINES))
    parser.add_argument("--frames", type=int, default=60)
    parser.add_argument("--press", action="append", default=[])
    parser.add_argument("--reset-at", type=int, action="append", default=[])
    parser.add_argument("--no-fm", action="store_true")
    parser.add_argument("--tmss-rom")
    parser.add_argument("--pad", default="3button", choices=sorted(PAD_TYPES))
    parser.add_argument("--pad2", default="3button", choices=sorted(PAD_TYPES))
    parser.add_argument("--steps")
    parser.add_argument("--out")
    args = parser.parse_args(argv)
    presses = [parse_press(spec) for spec in args.press]
    steps = tuple(int(x) for x in args.steps.split(":")) if args.steps else None
    image = load_image(args.cartridge)
    digest = hashlib.sha1(image).hexdigest()
    cartridge = Cartridge.from_image(image)
    system = System(
        MACHINES[args.machine],
        cartridge,
        pads=(PAD_TYPES[args.pad](), PAD_TYPES[args.pad2]()),
        tmss_rom=load_image(args.tmss_rom) if args.tmss_rom else None,
    )
    if args.no_fm:
        system.board.fm.enabled = False
    out = open(args.out, "w") if args.out else sys.stdout  # noqa: SIM115
    out.write(f"{TRACE_HEADER}\n# {digest} slot={cartridge.slot} machine={args.machine}\n")
    pad = system.board.io.ports[0].device
    step_index = 0
    for frame in range(args.frames):
        held = 0
        for buttons, start, stop in presses:
            if start <= frame < stop:
                held |= buttons
        if pad is not None:
            pad.pressed = held
        if frame in args.reset_at:
            system.press_reset()
        start = system.vdp.frame
        try:
            while system.vdp.frame == start:
                elapsed = system.step()
                if steps and steps[0] <= step_index < steps[1]:
                    out.write(step_line(system, step_index, elapsed) + "\n")
                step_index += 1
        except Lockup as lockup:
            out.write(f"L {frame} {lockup}\n")
            break
        system.board.sound_sync(system.mclk)
        out.write(frame_lines(system, frame))
    out.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
