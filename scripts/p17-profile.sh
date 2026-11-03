#!/usr/bin/env bash
# Diagnostic companion to the P17 benchmark: boots the P17 Debian inputs
# (images/linux-test, scripts/fetch-linux-test.sh) once per hart count with
# the release CLI, to /init's "rivemu.poweroff" exit, and prints the guest
# instruction count, MIPS, host CPU time, and the largest gaps between
# consecutive kernel log timestamps. Guest time follows host time, so a gap
# is where the guest spent its time between two messages. With PROFILE=1
# (default), it also boots once more with "rivemu.kallsyms" to get the
# kernel's symbols and prints the guest functions that ran the most
# instructions in the SMP1 boot (RIVEMU_PC_PROFILE, scripts/pc-profile.py).
# Both boots use "nokaslr" so that the symbols match.
#
# Usage: scripts/p17-profile.sh [smp ...]   (default: 1 4)
# With P17_PROFILE_SUMMARY=<file>, everything but the console logs is also
# appended to <file>.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMG="${RIVEMU_P17_IMAGES:-$ROOT/images/linux-test}"
cargo build -q --release -p rivemu-cli --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/release/rivemu-system-riscv64"
log="$(mktemp)"
pcs="$(mktemp)"
syms="$(mktemp)"
trap 'rm -f "$log" "$pcs" "$syms"' EXIT
APPEND="console=ttyS0 rcupdate.rcu_cpu_stall_suppress=1 nokaslr rivemu.poweroff"
boot() {
    local smp=$1
    shift
    "$BIN" -M virt -smp "$smp" -m 256M -nographic \
        -bios "$IMG/fw_dynamic.bin" -kernel "$IMG/Image" -initrd "$IMG/rootfs.cpio" "$@"
}
SUMMARY="${P17_PROFILE_SUMMARY:-/dev/null}"
# say: print a summary line (stdin) to the log and the summary file.
say() { tee -a "$SUMMARY"; }
[[ $# -gt 0 ]] || set -- 1 4
for smp in "$@"; do
    echo "== smp=$smp" | say
    # bash's `time` reports wall, user and sys for the emulator process.
    { time RIVEMU_STATS=1 RIVEMU_PC_PROFILE="$pcs.$smp" boot "$smp" -append "$APPEND" \
        > "$log" 2>&1; } 2>&1 | grep -v '^$' | say
    grep -a "instructions in" "$log" | say || true
    grep -aq RIVEMU_SMOKE_OK "$log" || { tail -20 "$log"; exit 1; }
    echo "largest gaps between kernel log lines (seconds before the line):" | say
    awk -F'[][]' '/^\[ *[0-9]+\.[0-9]+\]/ {
            t = $2 + 0
            if (seen) printf "%8.3f %s\n", t - prev, $0
            prev = t; seen = 1
        }' "$log" | sort -rn | head -15 | say
    echo "last kernel timestamp: $(grep -ao '^\[ *[0-9]*\.[0-9]*\]' "$log" | tail -1)" | say
    echo "::group::console smp=$smp"
    cat "$log"
    echo "::endgroup::"
done
if [[ "${PROFILE:-1}" == 1 && -f "$pcs.1" ]]; then
    boot 1 -append "$APPEND rivemu.kallsyms" | tr -d '\r' \
        | grep -aE '^[0-9a-f]{16} [tTwW] ' > "$syms" || true
    echo "== guest functions by instructions executed (smp=1, $(wc -l < "$syms") symbols)" | say
    python3 "$ROOT/scripts/pc-profile.py" "$pcs.1" "$syms" 40 | say
fi
rm -f "$pcs".*
