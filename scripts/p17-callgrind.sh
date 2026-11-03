#!/usr/bin/env bash
# Host instruction profile of the P17 Debian boot: runs the release CLI under
# callgrind (valgrind) to /init's "rivemu.poweroff" exit with -icount shift=3,
# so that the guest does the same work in every run however slow the host is,
# and prints the hottest functions (self and inclusive) and the hottest
# source lines of the interpreter.
#
# Usage: scripts/p17-callgrind.sh [<callgrind output file>]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${RIVEMU_P17_IMAGES:-$ROOT/images/linux-test}"
OUT="${1:-$ROOT/target/p17-callgrind.out}"
cargo build -q --release -p rivemu-cli --manifest-path "$ROOT/Cargo.toml"
valgrind --tool=callgrind --callgrind-out-file="$OUT" \
    "$ROOT/target/release/rivemu-system-riscv64" -M virt -smp 1 -m 256M -nographic -icount shift=3 \
    -bios "$IMG/fw_dynamic.bin" -kernel "$IMG/Image" -initrd "$IMG/rootfs.cpio" \
    -append "console=ttyS0 rcupdate.rcu_cpu_stall_suppress=1 nokaslr rivemu.poweroff" \
    > "$OUT.console" 2>&1
grep -aq RIVEMU_SMOKE_OK "$OUT.console" || { tail -20 "$OUT.console"; exit 1; }
grep -a "Collected" "$OUT.console" || true
echo "== functions by self cost"
callgrind_annotate "$OUT" 2>/dev/null | sed -n '/^Ir/,$p' | sed -n '3,45p'
echo "== functions by inclusive cost"
callgrind_annotate --inclusive=yes "$OUT" 2>/dev/null | sed -n '/^Ir/,$p' | sed -n '3,45p'
echo "== hottest source lines (instructions, line)"
callgrind_annotate --auto=yes --show-percs=no "$OUT" 2>/dev/null \
    | awk '/^-- Auto-annotated source: / { file = $NF }
           $1 ~ /^[0-9,]+$/ && file != "" && !/=>|events annotated|<counts for/ {
               n = $1; gsub(",", "", n); if (n + 0 > 0) print n, file ":" $0 }' \
    | sort -rn | head -60 | cut -c1-200
