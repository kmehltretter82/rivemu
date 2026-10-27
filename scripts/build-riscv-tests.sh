#!/usr/bin/env bash
# Builds the official riscv-tests ISA suite into images/riscv-tests/build.
#
# Uses clang + lld (Homebrew `llvm`/`lld` on macOS, apt `clang lld` on Linux)
# or a riscv64 GCC if available. Usage: scripts/build-riscv-tests.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/images/riscv-tests"
OUT="$SRC/build"
# Pinned upstream revision for reproducibility.
REV="${RISCV_TESTS_REV:-3df703cd52d5f0d8224e3c50bf03c6cb70efff98}"

if [ ! -d "$SRC/.git" ]; then
    git clone -q https://github.com/riscv-software-src/riscv-tests.git "$SRC"
fi
git -C "$SRC" fetch -q origin "$REV" 2>/dev/null || true
git -C "$SRC" checkout -q "$REV"
git -C "$SRC" submodule update -q --init --recursive

CLANG=""
for c in "${RISCV_CLANG:-}" /opt/homebrew/opt/llvm/bin/clang /usr/local/opt/llvm/bin/clang clang; do
    if [ -n "$c" ] && command -v "$c" >/dev/null 2>&1 &&
        "$c" -print-targets 2>/dev/null | grep -q riscv64; then
        CLANG="$c"
        break
    fi
done
if [ -z "$CLANG" ]; then
    echo "error: need a clang with the riscv64 target (and ld.lld)" >&2
    exit 1
fi

CFLAGS=(--target=riscv64-unknown-elf -march=rv64gc -mabi=lp64d -static -mcmodel=medany
    -fvisibility=hidden -nostdlib -nostartfiles -fuse-ld=lld -Wno-unused-command-line-argument)

GNU_PREFIX=""
for p in riscv64-elf- riscv64-unknown-elf- riscv64-linux-gnu-; do
    if command -v "${p}as" >/dev/null 2>&1; then GNU_PREFIX=$p; break; fi
done

mkdir -p "$OUT"
count=0

# Physical-memory ("p") environment: one assembly file per test.
build_p() { # suite
    local suite=$1
    for src in "$SRC/isa/$suite"/*.S; do
        local name
        name=$(basename "$src" .S)
        local out="$OUT/$suite-p-$name"
        [ "$out" -nt "$src" ] && continue
        if ! "$CLANG" "${CFLAGS[@]}" -I"$SRC/env/p" -I"$SRC/isa/macros/scalar" \
            -T"$SRC/env/p/link.ld" "$src" -o "$out" 2>/dev/null; then
            # A few tests trip LLVM's assembler; fall back to GNU as/ld.
            local pre="$OUT/.tmp-$suite-$name"
            "$CLANG" --target=riscv64-unknown-elf -march=rv64gc -mabi=lp64d -E -P -I"$SRC/env/p" \
                -I"$SRC/isa/macros/scalar" "$src" -o "$pre.s"
            "$GNU_PREFIX"as -march=rv64gc_zicsr_zifencei "$pre.s" -o "$pre.o"
            "$GNU_PREFIX"ld -T"$SRC/env/p/link.ld" "$pre.o" -o "$out"
            rm -f "$pre.s" "$pre.o"
        fi
        count=$((count + 1))
    done
}

# Virtual-memory ("v") environment: test + small C runtime that enables Sv39.
build_v() { # suite
    local suite=$1
    for src in "$SRC/isa/$suite"/*.S; do
        local name
        name=$(basename "$src" .S)
        local out="$OUT/$suite-v-$name"
        [ "$out" -nt "$src" ] && continue
        "$CLANG" "${CFLAGS[@]}" -O2 -DENTROPY=0x$(printf '%s' "$name" | cksum | cut -c1-6) \
            -ffreestanding -I"$ROOT/scripts/riscv-tests-include" \
            -I"$SRC/env/v" -I"$SRC/isa/macros/scalar" -T"$SRC/env/v/link.ld" \
            "$src" "$SRC/env/v/entry.S" "$SRC/env/v/vm.c" "$SRC/env/v/string.c" -o "$out"
        count=$((count + 1))
    done
}

for s in rv64ui rv64um rv64ua rv64uf rv64ud rv64uc rv64mi rv64si; do
    build_p "$s"
done
for s in rv64ui rv64um rv64ua rv64uf rv64ud rv64uc; do
    build_v "$s"
done

echo "riscv-tests: built $count new binaries in $OUT ($(ls "$OUT" | wc -l | tr -d ' ') total)"
