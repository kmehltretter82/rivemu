#!/usr/bin/env bash
# Compares Linux boot-to-poweroff wall time of rivemu builds on this machine.
#
# Each run boots images/linux-src (scripts/build-linux-test-from-source.sh)
# under OpenSBI v1.7 with "rivemu.poweroff" on the command line: /init prints
# RIVEMU_SMOKE_OK and powers off. Builds are interleaved (A B A B ...) so drift
# in host load affects every build alike. A run counts only if the guest
# printed RIVEMU_SMOKE_OK and rivemu exited 0.
#
# Usage: scripts/bench-boot.sh <runs> <smp> <label>=<binary> [<label>=<binary> ...]
# Output: one line per run, then "label smp=N median min max" per build.
set -uo pipefail

RUNS="${1:?runs}"
SMP="${2:?smp}"
shift 2
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="$ROOT/images"
BIOS="$IMG/linux-test/fw_dynamic.bin"
KERNEL="$IMG/linux-src/Image"
INITRD="$IMG/linux-src/rootfs.cpio"
for f in "$BIOS" "$KERNEL" "$INITRD"; do
    [[ -f "$f" ]] || { echo "missing $f" >&2; exit 1; }
done

declare -A TIMES
LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT
for ((i = 1; i <= RUNS; i++)); do
    for spec in "$@"; do
        label="${spec%%=*}"
        bin="${spec#*=}"
        start=$(date +%s.%N)
        timeout 300 "$bin" -M virt -smp "$SMP" -m 512M -nographic \
            -bios "$BIOS" -kernel "$KERNEL" -initrd "$INITRD" \
            -append "console=ttyS0 rivemu.poweroff" > "$LOG" 2>&1
        status=$?
        end=$(date +%s.%N)
        t=$(echo "$end - $start" | bc)
        if [[ $status -eq 0 ]] && grep -q RIVEMU_SMOKE_OK "$LOG"; then
            TIMES[$label]+="$t "
            echo "run $i $label smp=$SMP ${t}s"
        else
            echo "run $i $label smp=$SMP FAILED (exit $status)"
            tail -5 "$LOG"
        fi
    done
done
for spec in "$@"; do
    label="${spec%%=*}"
    # shellcheck disable=SC2086
    printf '%s\n' ${TIMES[$label]:-} | sort -n | awk -v l="$label" -v s="$SMP" '
        { v[NR] = $1 }
        END {
            if (NR == 0) { print l, "smp=" s, "no successful runs"; exit }
            m = (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2
            printf "%s smp=%s n=%d median=%.2fs min=%.2fs max=%.2fs\n", l, s, NR, m, v[1], v[NR]
        }'
done
