# rivemu

A pure-Rust full-system RISC-V (`RV64GC`) emulator with a QEMU-compatible command line (`rivemu-system-riscv64`).

> **Status:** Milestones **P0–P32** complete (see [docs/PLAN.md](docs/PLAN.md) and [docs/FOLLOWUP_PLAN.md](docs/FOLLOWUP_PLAN.md)).
> Passes all **244/244** official `riscv-tests` (`rv64{ui,um,ua,uf,ud,uc,mi,si}-p` and `rv64u*-v`) and **418/418** tests of the privileged [`riscv-arch-test`](tests/arch-test/README.md) suites. It boots upstream Debian RISC-V SMP Linux (`-smp 4`) with built-in or external OpenSBI. An Alpine 3.24 disk also boots through serial login and installs packages over DHCP/DNS/HTTPS. Linux end-to-end tests cover NVMe, virtio-gpu/input, vsock, the SBI PMU, reboot and Snapshot v2, and all 18 differential suites pass against Sail and QEMU. See [the validation matrix](docs/VALIDATION_STATUS.md) for exactly what is verified and what is outstanding (for example, the P17 boot-time targets are not met).

## Features

- **Guest ISA:** RISC-V **RV64GCV** (`RVV 1.0`, `VLEN = 256`, `ELEN = 64`) + the **RVA23U64** extensions (no `H`, so not RVA23S64) & Scalar Cryptography (`RV64IMAFDCV`, `Zba`, `Zbb`, `Zbc`, `Zbkb`, `Zbkc`, `Zbkx`, `Zbs`, `Zca`, `Zcb`, `Zcd`, `Zcmop`, `Zfa`, `Zfbfmin`, `Zfh`, `Zfhmin`, `Zicbom`, `Zicbop`, `Zicboz`, `Zicfilp`, `Zicfiss`, `Zicntr`, `Zicond`, `Zicsr`, `Zifencei`, `Zihintpause`, `Zimop`, `Zacas`, `Zabha`, `Zalasr`, `Zama16b`, `Zawrs`, `Zknd`, `Zkne`, `Zknh`, `Zkr`, `Zksed`, `Zksh`, `Zkt`, `Sdtrig`, `Sstc`, `Sscofpmf`, `Smcntrpmf`, `Smdbltrp`, `Smepmp`, `Smmpm`, `Smnpm`, `Smstateen`, `Ssdbltrp`, `Ssnpm`, `Ssqosid`, `Ssstateen`, `Svpbmt`, `Svinval`, `Svnapot`, `Svrsw60t59b`, `Svukte`, `Svvptc`), M/S/U privilege modes, `Sv39`/`Sv48`/`Sv57` MMU with ASID/global/superpage/NAPOT software TLB, 16-entry PMP (`TOR`/`NA4`/`NAPOT` + `Smepmp` `mseccfg`), 4 hardware debug triggers (`Sdtrig` `mcontrol6`/`icount`), and IEEE-754 compliant softfloat (`rivemu-softfloat`).
- **Interpreter Fast Paths (no JIT):** 8192-entry decoded-instruction cache (`icache`) with macro-op fusion (`auipc+addi`, `lui+addi`, `lui+addiw`, `slli+srli`, `auipc+jalr`), 1024-entry decoded basic-block cache (`bbcache`), 1-entry fetch page cache, and 1024-entry per-hart software TLB with direct `fast_ram` host-offset lookup.
- **Machine Model (`virt`):** QEMU-compatible physical memory map with 16550A UART, SiFive CLINT (ACLINT MSWI + MTIMER), SiFive PLIC, Goldfish RTC, SiFive test/syscon power controller, and automatic Flattened Device Tree (DTB v17) generation (`rivemu-fdt`).
- **VirtIO (`virtio-mmio` v2):** Split virtqueues with indirect descriptors (`VIRTIO_RING_F_INDIRECT_DESC`) and `VIRTIO_RING_F_EVENT_IDX`:
  - `virtio-blk-device` backed by raw files, in-memory disks, or **QCOW2 v2/v3** images.
  - `virtio-net-device` backed by a pure-Rust user-mode network stack (`10.0.2.0/24`, gateway `10.0.2.2`, DNS `10.0.2.3`, ARP, DHCPv4, ICMP echo, UDP/TCP gateway, and `hostfwd=tcp|udp::hostport-:guestport`).
  - `virtio-serial-device` (`virtio-console`) and `virtio-rng-device`.
- **SMP & Deterministic Execution:** Multi-core (`-smp N`) with true host atomics (`AtomicU32`/`AtomicU64` for `lr`/`sc`/`amo`), `pause`/IPI slice yielding, and deterministic instruction-count timekeeping (`-icount [shift=]N`) with idle timer warping.
- **Debugging & Introspection:**
  - Built-in GDB Remote Serial Protocol stub (`-s`, `-gdb tcp::PORT`, `-S`).
  - Human Monitor Protocol (`Ctrl-A c` on muxed stdio) and QMP JSON server (`-qmp unix:...|tcp:...`).
  - Diagnostic trace logging (`-d in_asm,exec,cpu,int,mmu,guest_errors,unimp` and `-D <logfile>`).
- **Memory Safety:** `#![forbid(unsafe_code)]` across all crates except `rivemu-mem` (which encapsulates atomic host RAM access).

## Building

```sh
cargo build --release
```

Requires Rust 1.85+ (pinned to `stable` via `rust-toolchain.toml`).
Supported hosts: macOS (`aarch64`) and Linux (`x86_64`, `aarch64`).

## Usage

Boot Linux with OpenSBI, a VirtIO block disk, and user-mode networking:

```sh
cargo run --release --bin rivemu-system-riscv64 -- \
    -M virt -m 256M -smp 4 -nographic \
    -bios images/linux-test/fw_dynamic.bin \
    -kernel images/linux-test/Image \
    -initrd images/linux-test/rootfs.cpio \
    -append "console=ttyS0" \
    -drive id=hd0,file=disk.qcow2,format=qcow2,if=none \
    -device virtio-blk-device,drive=hd0 \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 \
    -device virtio-net-device,netdev=net0 \
    -device virtio-rng-device
```

While running with `-nographic`:
- Press `Ctrl-A x` to terminate the emulator immediately.
- Press `Ctrl-A c` to switch between the guest serial console and the interactive HMP monitor (`(qemu) info registers`, `info mtree`, `info status`, `xp /8gx 0x80000000`, `system_reset`, `system_powerdown`, `quit`).
- Press `Ctrl-A h` to print console mux help.

## Testing

Run the full unit, softfloat, and 244-binary `riscv-tests` conformance suite:

```sh
cargo test --workspace
```

Fetch the upstream OpenSBI v1.7 + Debian RISC-V Linux 6.12 + BusyBox + VirtIO test artifacts and run the end-to-end 4-CPU Linux + VirtIO smoke test:

```sh
./scripts/fetch-linux-test.sh
RIVEMU_LINUX_TEST=1 cargo test --release -p rivemu-machine --test linux_boot -- --nocapture
```

### Linux kernel A/B evidence

Use `scripts/linux_ab.py` (Python 3.10 or newer) to run one reproducer under two kernel images with the same Rivemu binary, machine shape, init payload, and guest arguments. The reproducer should print exactly one result line (for example, `RIVEMU_AB_RESULT=BUG` or `RIVEMU_AB_RESULT=PASS`) and then power off cleanly.

```sh
cargo build --release --bin rivemu-runner
python3 scripts/linux_ab.py \
    --kernel-a /path/to/Image.before \
    --kernel-b /path/to/Image.after \
    --label-a BAD_KERNEL_COMMIT \
    --label-b FIXED_KERNEL_COMMIT \
    --init /path/to/reproducer-init \
    --expect-a 'RIVEMU_AB_RESULT=BUG' \
    --expect-b 'RIVEMU_AB_RESULT=PASS' \
    --smp 1 --strict --timeout 90 \
    --out linux-ab-results/my-test -- reproducer.option=value
```

The driver runs A and B sequentially, requires clean poweroff, enforces a process-group timeout, rejects ambiguous or conflicting result markers, rejects mutable `RIVEMU_SHARE_DIR` state, and verifies that every hashed input remains unchanged across both runs. It refuses identical kernels unless `--allow-same-kernel` is supplied for an orchestration control. Its evidence directory contains both complete console logs, `manifest.json` with SHA-256 identities and run metadata, and `report.md` with wording suitable for a review note or commit message. The runner binary's SHA-256 is the executable identity; workspace Git state is recorded separately and is not claimed as a binary build stamp. Whole boot logs are retained but not compared because timestamps and SMP output ordering are not stable oracles.

The downloaded smoke image can exercise the complete path as a same-kernel control:

```sh
python3 scripts/linux_ab.py \
    --kernel-a images/linux-test/Image \
    --kernel-b images/linux-test/Image \
    --label-a control-a --label-b control-b \
    --init images/linux-test/rootfs.cpio \
    --expect-a RIVEMU_SMOKE_OK --expect-b RIVEMU_SMOKE_OK \
    --allow-same-kernel --smp 1 --timeout 90 -- rivemu.poweroff
```

The generated control report is labeled `CONTROL PASS` and explicitly states that it is not kernel A/B evidence.

Run the orchestration tests without booting a guest:

```sh
python3 -m unittest discover -s scripts/tests -p 'test_*.py' -v
```

## Workspace Layout

| Crate | Purpose |
|---|---|
| `rivemu-core` | Shared vocabulary types (`GuestAddr`, `AccessSize`, `IrqLine`, `HostTimebase`, `IcountTimebase`) |
| `rivemu-mem` | Guest RAM (`Ram` with host atomics), `MmioDevice` trait, `AddressSpace` with O(1) fast-RAM path |
| `rivemu-softfloat` | Pure-Rust IEEE-754 `f32`/`f64` arithmetic with all 5 RISC-V rounding modes and exception flags |
| `rivemu-cpu-riscv` | RV64GC decoder, interpreter, `icache`, CSRs, M/S/U traps, Sv39/Sv48 MMU + TLB, PMP, logger |
| `rivemu-devices` | `Uart16550`, `Clint` (ACLINT MSWI + MTIMER), `Plic`, `GoldfishRtc`, `SifiveTest` (syscon power) |
| `rivemu-fdt` | Zero-dependency Flattened Device Tree (DTB v17) builder |
| `rivemu-backends` | Block (`RawBlock`, `MemoryBlock`, `Qcow2Block`), Network (`UserNet`), and Chardev backends |
| `rivemu-virtio` | `VirtioMmio` v2 transport, split `VirtQueue`, `VirtioBlk`, `VirtioNet`, `VirtioConsole`, `VirtioRng` |
| `rivemu-gdbstub` | GDB Remote Serial Protocol (RSP) TCP server (`-s`, `-gdb`, `-S`) |
| `rivemu-monitor` | Human Monitor Protocol (`Hmp`) and QMP JSON server (`QmpServer`) |
| `rivemu-machine` | `virt` board wiring, ELF64/raw loaders, FDT generation, scheduling loop |
| `rivemu-cli` | `rivemu-system-riscv64` binary and QEMU-compatible option parser |

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
