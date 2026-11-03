#!/usr/bin/env bash
# Same-machine P17 comparison: runs tests/linux_perf.rs (Debian kernel and
# initramfs from scripts/fetch-linux-test.sh, Machine::new() to the first
# "~ # " prompt) for two rivemu checkouts, alternating between them, and prints
# the median and p95 of the recorded samples per checkout and hart count.
#
# Usage: scripts/p17-compare.sh <rounds> <label>=<checkout> [<label>=<checkout> ...]
# Each round runs every checkout once with RIVEMU_LINUX_PERF_WARMUPS (default
# 1) warmups and RIVEMU_LINUX_PERF_REPETITIONS (default 3) recorded boots per
# hart count. Every checkout needs images/linux-test populated.
set -euo pipefail

ROUNDS="${1:?rounds}"
shift
export RIVEMU_REQUIRE_LINUX_PERF=1
export RIVEMU_LINUX_PERF_WARMUPS="${RIVEMU_LINUX_PERF_WARMUPS:-1}"
export RIVEMU_LINUX_PERF_REPETITIONS="${RIVEMU_LINUX_PERF_REPETITIONS:-3}"

for spec in "$@"; do
    (cd "${spec#*=}" && cargo test -q --release -p rivemu-machine --test linux_perf --no-run)
done

samples="$(mktemp)"
trap 'rm -f "$samples"' EXIT
for ((round = 1; round <= ROUNDS; round++)); do
    for spec in "$@"; do
        label="${spec%%=*}"
        (cd "${spec#*=}" && cargo test -q --release -p rivemu-machine --test linux_perf -- --nocapture 2>&1) \
            | grep -E '^P17_(SAMPLE|CALIB)' | sed "s/^/$label,round=$round,/" | tee -a "$samples"
    done
done

echo
echo "label smp n median p95 (seconds, recorded samples only)"
for spec in "$@"; do
    label="${spec%%=*}"
    for smp in 1 4; do
        grep "^$label,.*smp=$smp,phase=recorded" "$samples" | sed 's/.*seconds=//' | sort -n \
            | awk -v l="$label" -v s="$smp" '
                { v[NR] = $1 }
                END {
                    if (NR == 0) exit
                    m = (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2
                    r = int(0.95 * NR); if (r < 0.95 * NR) r++
                    printf "%s %d %d %.3f %.3f\n", l, s, NR, m, v[r]
                }'
    done
done

# The control: every boot runs between two timings of a fixed native
# workload (tests/linux_perf.rs `calibrate`). A boot taken while the runner
# was slow shows a slow calibration next to it.
echo
echo "label smp round index seconds calib_before calib_after (recorded samples; calibration median per label and hart count first)"
for spec in "$@"; do
    label="${spec%%=*}"
    for smp in 1 4; do
        grep "^$label,.*P17_CALIB,smp=$smp,phase=recorded" "$samples" | sed 's/.*before=//; s/,after=/ /' \
            | tr ' ' '\n' | sort -n | awk -v l="$label" -v s="$smp" '
                { v[NR] = $1 }
                END { if (NR) printf "%s %d calibration median %.3f s, max %.3f s\n", l, s, (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2, v[NR] }'
        paste -d' ' \
            <(grep "^$label,.*P17_SAMPLE,smp=$smp,phase=recorded" "$samples" | sed 's/^[^,]*,round=\([0-9]*\),.*index=\([0-9]*\),seconds=/\1 \2 /') \
            <(grep "^$label,.*P17_CALIB,smp=$smp,phase=recorded" "$samples" | sed 's/.*before=//; s/,after=/ /') \
            | awk -v l="$label" -v s="$smp" '{ printf "%s %d %s %s %.3f %.3f %.3f\n", l, s, $1, $2, $3, $4, $5 }'
    done
done
