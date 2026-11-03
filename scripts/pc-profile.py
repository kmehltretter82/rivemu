#!/usr/bin/env python3
"""Symbolizes a RIVEMU_PC_PROFILE file.

Usage: scripts/pc-profile.py <profile> [<System.map or /proc/kallsyms>] [top]

<profile> has "<instructions> <pc> [<host ns>]" lines: every scheduling slice
charges the guest instructions it ran and the host time it took to the PC
where it stopped. Each PC is charged to the nearest text symbol at or below
it; PCs below the first kernel symbol are grouped as "[firmware/user
<region>]". Prints the top functions by guest instructions with their share
of the host time and their speed (guest MIPS) when the profile has times.
"""
import bisect
import collections
import sys


def load_symbols(path):
    syms = []
    with open(path) as f:
        for line in f:
            parts = line.split()
            if len(parts) < 3 or parts[1] not in "tTwW":
                continue
            addr = int(parts[0], 16)
            if addr:
                syms.append((addr, parts[2]))
    syms.sort()
    return [a for a, _ in syms], [n for _, n in syms]


def main():
    profile = sys.argv[1]
    addrs, names = load_symbols(sys.argv[2]) if len(sys.argv) > 2 else ([], [])
    top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
    insns = collections.Counter()
    nanos = collections.Counter()
    total = total_ns = 0
    with open(profile) as f:
        for line in f:
            fields = line.split()
            n, pc = int(fields[0]), int(fields[1], 16)
            ns = int(fields[2]) if len(fields) > 2 else 0
            total += n
            total_ns += ns
            i = bisect.bisect_right(addrs, pc) - 1
            if i >= 0 and pc - addrs[i] < 1 << 20:
                name = names[i]
            elif pc >= 1 << 63:
                name = "[kernel, unknown symbol]"
            elif 0x8000_0000 <= pc < 0x8020_0000:
                name = "[M-mode firmware]"
            else:
                name = "[user or other %#x]" % (pc >> 28 << 28)
            insns[name] += n
            nanos[name] += ns
    if total_ns:
        print(f"{total} guest instructions sampled in {total_ns / 1e9:.2f} s of hart time "
              f"({total * 1e3 / total_ns:.1f} MIPS)")
        print("  insns%   cumul   time%   MIPS  function")
    else:
        print(f"{total} guest instructions sampled")
    acc = 0
    for name, n in insns.most_common(top):
        acc += n
        line = f"{100 * n / total:6.2f}% {100 * acc / total:6.2f}%"
        if total_ns:
            ns = nanos[name]
            mips = f"{n * 1e3 / ns:6.1f}" if ns else "     -"
            line += f" {100 * ns / total_ns:6.2f}% {mips}"
        print(f"{line}  {name}")
    if total_ns:
        print("by host time:")
        for name, ns in nanos.most_common(top // 2):
            print(f"{100 * ns / total_ns:6.2f}% {insns[name] * 1e3 / ns:6.1f} MIPS  {name}")


main()
