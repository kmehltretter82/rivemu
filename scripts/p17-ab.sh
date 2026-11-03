#!/usr/bin/env bash
# Same-checkout A/B of run-time variants on the P17 benchmark
# (tests/linux_perf.rs): one test binary, run with different environment
# variables, alternating per round, so that every variant sees the same
# runner. Prints the median and p95 per variant and hart count.
#
# Usage: scripts/p17-ab.sh <rounds> <label>=[VAR=value,...] [...]
# e.g.   scripts/p17-ab.sh 3 base= fwd=RIVEMU_BLOCK_FWD=1
# Hart counts: RIVEMU_LINUX_PERF_SMPS (default 1). Needs images/linux-test.
set -euo pipefail

ROUNDS="${1:?rounds}"
shift
export RIVEMU_REQUIRE_LINUX_PERF=1
export RIVEMU_LINUX_PERF_WARMUPS="${RIVEMU_LINUX_PERF_WARMUPS:-1}"
export RIVEMU_LINUX_PERF_REPETITIONS="${RIVEMU_LINUX_PERF_REPETITIONS:-3}"
export RIVEMU_LINUX_PERF_SMPS="${RIVEMU_LINUX_PERF_SMPS:-1}"

cargo test -q --release -p rivemu-machine --test linux_perf --no-run

samples="$(mktemp)"
trap 'rm -f "$samples"' EXIT
for ((round = 1; round <= ROUNDS; round++)); do
    for spec in "$@"; do
        label="${spec%%=*}"
        vars="${spec#*=}"
        # shellcheck disable=SC2086
        env ${vars//,/ } cargo test -q --release -p rivemu-machine --test linux_perf -- --nocapture 2>&1 \
            | grep '^P17_SAMPLE' | sed "s/^/$label,round=$round,/" | tee -a "$samples"
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
