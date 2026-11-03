#!/usr/bin/env python3
"""Symbolizes a RIVEMU_PC_PROFILE file.

Usage: scripts/pc-profile.py <profile> [<System.map or /proc/kallsyms>] [top]

<profile> has "<instructions> <pc>" lines. Each PC is charged to the nearest
text symbol at or below it; PCs below the first kernel symbol are grouped as
"[firmware/user <region>]". Prints the top functions by guest instructions.
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
    totals = collections.Counter()
    total = 0
    with open(profile) as f:
        for line in f:
            n, pc = line.split()
            n, pc = int(n), int(pc, 16)
            total += n
            i = bisect.bisect_right(addrs, pc) - 1
            if i >= 0 and pc - addrs[i] < 1 << 20:
                totals[names[i]] += n
            elif pc >= 1 << 63:
                totals["[kernel, unknown symbol]"] += n
            elif 0x8000_0000 <= pc < 0x8020_0000:
                totals["[M-mode firmware]"] += n
            else:
                totals["[user or other %#x]" % (pc >> 28 << 28)] += n
    print(f"{total} guest instructions sampled")
    acc = 0
    for name, n in totals.most_common(top):
        acc += n
        print(f"{100 * n / total:6.2f}% {100 * acc / total:6.2f}%  {name}")


main()
