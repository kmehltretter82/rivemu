# rivemu

A small full-system RISC-V (RV64GC) emulator written entirely in Rust.
Interpreter-based, QEMU-compatible command line, aiming to boot Linux with
OpenSBI and virtio devices.

> **Status:** early development – milestone **P0** (skeleton). The memory
> system, image loaders and command line work; CPU emulation lands in P1.
> See [docs/PLAN.md](docs/PLAN.md) for the roadmap.

## Goals

- Full-system emulation of the RISC-V `virt` board (QEMU-compatible memory map)
- RV64GC, M/S/U privilege modes, Sv39/Sv48
- Boots unmodified upstream Linux via OpenSBI
- virtio-mmio block, network, console and rng devices
- Accepts a subset of `qemu-system-riscv64` options with identical syntax
- 100% Rust, no JIT, `unsafe` confined to the guest RAM implementation

Non-goals: KVM/HVF acceleration, user-mode emulation, other architectures,
graphics, live migration.

## Building

```sh
cargo build --release
```

Requires Rust 1.85+ (the repository pins `stable` via `rust-toolchain.toml`).
Supported hosts: macOS (Apple Silicon) and Linux (x86_64, arm64).

## Usage

```sh
rivemu-system-riscv64 -M virt -m 512M -nographic \
    -bios fw_jump.bin -kernel Image -initrd rootfs.cpio \
    -append "console=ttyS0"
```

`rivemu-system-riscv64 -help` lists supported options. Without `-bios`,
the default OpenSBI image (`opensbi-riscv64-generic-fw_dynamic.bin`) is looked
up in `$RIVEMU_FIRMWARE_DIR`, next to the binary, and in an installed QEMU's
share directory.

## Workspace layout

| Crate | Purpose |
|---|---|
| `rivemu-core` | Shared vocabulary types (addresses, access sizes, IRQ lines, clock) |
| `rivemu-mem` | Guest RAM, MMIO dispatch, physical address space |
| `rivemu-machine` | `virt` board, ELF/raw loaders, image placement |
| `rivemu-cli` | `rivemu-system-riscv64` binary, QEMU-style option parser |

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
