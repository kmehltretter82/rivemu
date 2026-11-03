#!/usr/bin/env bash
# Builds the riscv-arch-test (ACT4) self-checking ELFs for rivemu's DUT
# configuration (tests/arch-test/rivemu-rv64): the 15 privileged suites and
# every unprivileged rv64i suite, with Sail 0.16 as the reference model.
# Every input is pinned. Steps whose output already exists are skipped, so a
# cached <work-dir> rebuilds only what changed.
#
# Requirements: git, curl, cmake, a C/C++ compiler, libgmp-dev, zlib1g-dev,
# clang-20 + lld-20 + llvm-20, uv, and Ruby with Bundler.
# Usage: scripts/build-arch-test.sh <work-dir>
# ELFs:  <work-dir>/act-work/rivemu-rv64/elfs/{priv,rv64i}/...
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "${1:?work directory}"
WORK="$(cd "$1" && pwd)"
JOBS="${JOBS:-$(nproc)}"

SAIL_URL=https://github.com/rems-project/sail/releases/download/0.20.3-binary/sail-Linux-x86_64.tar.gz
SAIL_SHA256=8d7cd75624ece407d7001ad32905a13ae111d5cc1a055404f21510e113207e3f
SAIL_RISCV_GIT=https://github.com/riscv/sail-riscv.git
SAIL_RISCV_COMMIT=e5acebb04ff573e93203596486b831a5974117c3 # tag 0.16
ACT_GIT=https://github.com/riscv-non-isa/riscv-arch-test.git
ACT_COMMIT=2dc36dd2f3d25383039effa10f1a3930a78a945a
PRIV_SUITES=InterruptsS,InterruptsSm,PMP,S,Sm,Sstc,Sv,Svade,Svadu,Svbare,Svinval,Svnapot,Svpbmt,Svvptc,U

# fetch_commit <url> <commit> <dir>: shallow checkout of one pinned commit.
fetch_commit() {
    if [[ ! -d "$3/.git" ]]; then
        git init -q "$3"
        git -C "$3" fetch -q --depth 1 "$1" "$2"
        git -C "$3" checkout -q FETCH_HEAD
    fi
    [[ "$(git -C "$3" rev-parse HEAD)" == "$2" ]] || {
        echo "error: $3 is not at $2" >&2
        exit 1
    }
}

# 1. Sail compiler (prebuilt release binary).
if [[ ! -x "$WORK/sail/bin/sail" ]]; then
    curl -fsSL --retry 2 -o "$WORK/sail.tar.gz" "$SAIL_URL"
    echo "$SAIL_SHA256  $WORK/sail.tar.gz" | sha256sum -c -
    mkdir -p "$WORK/sail"
    tar -xzf "$WORK/sail.tar.gz" -C "$WORK/sail" --strip-components=1
    rm -f "$WORK/sail.tar.gz"
fi
export PATH="$WORK/sail/bin:$PATH"

# 2. Sail RISC-V model 0.16.
if [[ ! -x "$WORK/sail-riscv/bin/sail_riscv_sim" ]]; then
    fetch_commit "$SAIL_RISCV_GIT" "$SAIL_RISCV_COMMIT" "$WORK/sail-riscv-src"
    cmake -S "$WORK/sail-riscv-src" -B "$WORK/sail-riscv-build" \
        -DCMAKE_BUILD_TYPE=RelWithDebInfo -DENABLE_RISCV_TESTS=FALSE -DDOWNLOAD_GMP=FALSE
    cmake --build "$WORK/sail-riscv-build" --target sail_riscv_sim --parallel "$JOBS"
    mkdir -p "$WORK/sail-riscv/bin"
    cp "$WORK/sail-riscv-build/c_emulator/sail_riscv_sim" "$WORK/sail-riscv/bin/"
    rm -rf "$WORK/sail-riscv-build" "$WORK/sail-riscv-src"
fi
export PATH="$WORK/sail-riscv/bin:$PATH"
sail_riscv_sim --version

# 3. riscv-arch-test and its Ruby (UDB) dependencies.
fetch_commit "$ACT_GIT" "$ACT_COMMIT" "$WORK/riscv-arch-test"
(cd "$WORK/riscv-arch-test/framework/src/act/data" && bundle install --quiet)

# 4. Self-checking ELFs. ENABLE_EXPERIMENTAL_EXTENSIONS makes Sail model
# Svukte (see tests/arch-test/README.md).
suites="$PRIV_SUITES,$(ls "$WORK/riscv-arch-test/tests/rv64i" | paste -sd,)"
make -C "$WORK/riscv-arch-test" elfs \
    CONFIG_FILES="$ROOT/tests/arch-test/rivemu-rv64/test_config.yaml" \
    WORKDIR="$WORK/act-work" EXTENSIONS="$suites" EXCLUDE_EXTENSIONS=SsstrictV \
    ENABLE_EXPERIMENTAL_EXTENSIONS=True FAST=True JOBS="$JOBS"
find "$WORK/act-work/rivemu-rv64/elfs" -name '*.elf' | wc -l
