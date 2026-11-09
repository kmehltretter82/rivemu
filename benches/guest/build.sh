#!/usr/bin/env bash
# Builds the guest benchmarks into target/guest-bench/:
#   crypto_bench.bin       the full run (about 1.7 G guest instructions)
#   crypto_bench_10.bin    a tenth of it, for callgrind
# Run: target/release/rivemu-system-riscv64 -M virt -m 256M -nographic -bios <bin>
# (it powers off when done). Needs CROSS (default riscv64-linux-gnu-).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out="$here/../../target/guest-bench"
cross=${CROSS:-riscv64-linux-gnu-}
mkdir -p "$out"
for v in "crypto_bench:1" "crypto_bench_10:10"; do
    name=${v%%:*}
    scale=${v##*:}
    "${cross}gcc" -DSCALE="$scale" -O2 -march=rv64gc -mabi=lp64d -mcmodel=medany \
        -ffreestanding -fno-builtin -nostdlib -nostartfiles -static \
        -Wl,-Ttext=0x80000000 -Wl,--no-relax -Wl,--build-id=none -Wl,-N \
        -o "$out/$name.elf" "$here/start.S" "$here/crypto_bench.c" 2>&1 | grep -v 'RWX permissions' || true
    "${cross}objcopy" -O binary "$out/$name.elf" "$out/$name.bin"
    echo "$out/$name.bin"
done
