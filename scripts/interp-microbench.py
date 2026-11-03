#!/usr/bin/env python3
"""Interpreter microbenchmarks that need no RISC-V toolchain.

Assembles three M-mode loops (each about 640 M guest instructions), runs each
with the release CLI (`-bios`, exit through the SiFive test device) under
every variant, alternating, and prints the median wall-clock time and MIPS:

  dep     30 adds, each depending on the previous one (a0 += a1)
  ind     30 adds over six independent registers
  addmul  the inner loop of the kernel's mpihelp_addmul_1 (two loads, mul,
          mulhu, a carry chain of add/sltu, a store; 32 limbs per pass)

`dep` against `ind` shows how much the host pays for a guest register
dependency (the result goes through memory, `Hart::x`).

Usage: interp-microbench.py <rivemu-system-riscv64> <rounds> <label>=[VAR=value,...] ...
"""

import os
import statistics
import struct
import subprocess
import sys
import tempfile
import time

ZERO, T0, T1, S0, A0, A1, A2, A3, A4, A5, A6 = 0, 5, 6, 8, 10, 11, 12, 13, 14, 15, 16


def r(f7, rs2, rs1, f3, rd, op=0x33):
    return f7 << 25 | rs2 << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op


def i(imm, rs1, f3, rd, op):
    return (imm & 0xFFF) << 20 | rs1 << 15 | f3 << 12 | rd << 7 | op


def add(rd, rs1, rs2):
    return r(0, rs2, rs1, 0, rd)


def sltu(rd, rs1, rs2):
    return r(0, rs2, rs1, 3, rd)


def mul(rd, rs1, rs2):
    return r(1, rs2, rs1, 0, rd)


def mulhu(rd, rs1, rs2):
    return r(1, rs2, rs1, 3, rd)


def addi(rd, rs1, imm):
    return i(imm, rs1, 0, rd, 0x13)


def andi(rd, rs1, imm):
    return i(imm, rs1, 7, rd, 0x13)


def addiw(rd, rs1, imm):
    return i(imm, rs1, 0, rd, 0x1B)


def ld(rd, rs1, imm):
    return i(imm, rs1, 3, rd, 0x03)


def sd(rs2, rs1, imm):
    return (imm >> 5 & 0x7F) << 25 | rs2 << 20 | rs1 << 15 | 3 << 12 | (imm & 0x1F) << 7 | 0x23


def sw(rs2, rs1, imm):
    return (imm >> 5 & 0x7F) << 25 | rs2 << 20 | rs1 << 15 | 2 << 12 | (imm & 0x1F) << 7 | 0x23


def bne(rs1, rs2, off):
    o = off & 0x1FFF
    return (o >> 12 & 1) << 31 | (o >> 5 & 0x3F) << 25 | rs2 << 20 | rs1 << 15 | 1 << 12 \
        | (o >> 1 & 0xF) << 8 | (o >> 11 & 1) << 7 | 0x63


def lui(rd, imm20):
    return (imm20 & 0xFFFFF) << 12 | rd << 7 | 0x37


def auipc(rd, imm20):
    return (imm20 & 0xFFFFF) << 12 | rd << 7 | 0x17


def li(rd, v):
    """lui + addiw for a 32-bit signed value."""
    hi = (v + 0x800) >> 12
    return [lui(rd, hi), addiw(rd, rd, v - (hi << 12))]


def jal0(off):
    o = off & 0x1FFFFF
    return (o >> 20 & 1) << 31 | (o >> 1 & 0x3FF) << 21 | (o >> 11 & 1) << 20 | (o >> 12 & 0xFF) << 12 | 0x6F


EXIT = li(T0, 0x100000) + li(T1, 0x5555) + [sw(T1, T0, 0), jal0(0)]


def counted_loop(count, body):
    """`count` passes over `body` (straight-line code), then exit."""
    code = li(T0, count) + li(A1, 3)
    start = len(code)
    code += body + [addi(T0, T0, -1)]
    code.append(bne(T0, ZERO, 4 * (start - len(code))))
    return code + EXIT


def addmul(outer):
    code = li(S0, outer) + li(A3, 0x7F4A7C15)
    outer_start = len(code)
    code += [auipc(A0, 0x100), andi(A0, A0, -16), auipc(A1, 0x101), andi(A1, A1, -16)]
    code += [addi(A2, A1, 256), addi(T1, ZERO, 0)]
    inner = len(code)
    code += [
        ld(A5, A1, 0),
        ld(A4, A0, 0),
        mul(A6, A5, A3),
        mulhu(A5, A5, A3),
        add(A6, A6, T1),
        sltu(T1, A6, T1),
        add(A6, A6, A4),
        add(T1, T1, A5),
        sltu(A4, A6, A4),
        sd(A6, A0, 0),
        add(T1, T1, A4),
        addi(A1, A1, 8),
        addi(A0, A0, 8),
    ]
    code.append(bne(A1, A2, 4 * (inner - len(code))))
    code.append(addi(S0, S0, -1))
    code.append(bne(S0, ZERO, 4 * (outer_start - len(code))))
    return code + EXIT


PROGRAMS = {
    # name: (code, guest instructions)
    "dep": (counted_loop(20_000_000, [add(A0, A0, A1)] * 30), 20_000_000 * 32),
    "ind": (
        counted_loop(20_000_000, [add(rd, rd, A1) for _ in range(5) for rd in (A0, A2, A3, A4, A5, A6)]),
        20_000_000 * 32,
    ),
    "addmul": (addmul(1_400_000), 1_400_000 * (6 + 32 * 14 + 2)),
}


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    binary, rounds, variants = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    with tempfile.TemporaryDirectory() as tmp:
        times = {}
        for name, (code, _) in PROGRAMS.items():
            with open(os.path.join(tmp, name + ".bin"), "wb") as f:
                f.write(b"".join(struct.pack("<I", w) for w in code))
        for _ in range(rounds):
            for name in PROGRAMS:
                for spec in variants:
                    label, _, env_spec = spec.partition("=")
                    env = dict(os.environ)
                    env.update(kv.split("=", 1) for kv in env_spec.split(",") if kv)
                    start = time.perf_counter()
                    subprocess.run(
                        [binary, "-M", "virt", "-m", "64M", "-nographic", "-bios", os.path.join(tmp, name + ".bin")],
                        env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=600,
                    )
                    times.setdefault((name, label), []).append(time.perf_counter() - start)
        print("program variant n median_s mips")
        for name, (_, insns) in PROGRAMS.items():
            for spec in variants:
                label = spec.partition("=")[0]
                m = statistics.median(times[(name, label)])
                print(f"{name} {label} {len(times[(name, label)])} {m:.3f} {insns / m / 1e6:.1f}")


if __name__ == "__main__":
    main()
