#!/usr/bin/env bash
# Runs riscv-arch-test (ACT4) self-checking ELFs on rivemu and writes one log
# per test plus summary.txt. Build the ELFs first with
# tests/arch-test/README.md. A test passes when rivemu exits 0 and the test
# printed "RVCP-SUMMARY: TEST PASSED".
#
# Usage: scripts/run-arch-test.sh <elf-dir> <out-dir> [rivemu-binary]
set -uo pipefail

ELF_DIR="${1:?elf directory}"
OUT="${2:?output directory}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${3:-$ROOT/target/release/rivemu-system-riscv64}"
TIMEOUT="${ACT_TIMEOUT:-60}"
JOBS="${JOBS:-$(nproc)}"
# The UDB configuration enables Smdbltrp (MDT = 1 at reset), matching Sail.
ARGS=(-nographic -semihosting -M virt -cpu "rv64,smdbltrp=on")

mkdir -p "$OUT/logs"
export BIN TIMEOUT OUT ELF_DIR
export ARGS_STR="${ARGS[*]}"

run_one() {
    local elf="$1" rel name log status
    rel="${elf#"$ELF_DIR"/}"
    name="${rel%.elf}"
    log="$OUT/logs/${name//\//__}.log"
    # shellcheck disable=SC2086
    timeout "$TIMEOUT" "$BIN" $ARGS_STR -bios "$elf" > "$log" 2>&1
    status=$?
    if [[ $status -eq 0 ]] && grep -q "RVCP-SUMMARY: TEST PASSED" "$log"; then
        echo "PASS $name"
    elif [[ $status -eq 124 ]]; then
        echo "TIMEOUT $name"
    else
        echo "FAIL $name exit=$status"
    fi
}
export -f run_one

find "$ELF_DIR" -name '*.elf' | sort | xargs -P "$JOBS" -I{} bash -c 'run_one "$1"' _ {} \
    | sort -k2 > "$OUT/results.txt"
{
    echo "binary: $BIN ($("$BIN" --version 2>/dev/null | head -1))"
    echo "binary sha256: $(sha256sum "$BIN" | cut -d' ' -f1)"
    echo "elfs: $ELF_DIR"
    echo "args: ${ARGS[*]} -bios <elf>; timeout ${TIMEOUT}s"
    echo "total: $(wc -l < "$OUT/results.txt")"
    for s in PASS FAIL TIMEOUT; do
        echo "$s: $(grep -c "^$s " "$OUT/results.txt")"
    done
} > "$OUT/summary.txt"
cat "$OUT/summary.txt"
grep -v '^PASS ' "$OUT/results.txt" || true
