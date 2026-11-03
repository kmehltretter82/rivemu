# images/

Guest test artifacts (OpenSBI, kernels, initramfs, disk images) live here.
Everything except this README is git-ignored.

| Directory | Created by | Used by |
|---|---|---|
| `linux-test/` | `scripts/fetch-linux-test.sh`: OpenSBI v1.7 `fw_dynamic.bin`, the Debian kernel and an initramfs built from the Debian installer initrd, all checked against pinned SHA-256 values | `crates/rivemu-machine/tests/linux_boot.rs` (`RIVEMU_LINUX_TEST=1`) |
| `linux-src/` | `scripts/build-linux-test-from-source.sh`: Linux v6.12 and BusyBox 1_36_1 from pinned commits, with every driver the device tests need built in (about 25 min on 4 cores) | `crates/rivemu-machine/tests/linux_devices.rs` (`RIVEMU_LINUX_DEVICES_TEST=1`); rivemu-lockstep boot lockstep |
| `riscv-tests/` | `scripts/build-riscv-tests.sh` (pinned revision; rebuilt from scratch when the revision, flags, linker script or toolchain change) | `crates/rivemu-cpu-riscv/tests/riscv_tests.rs` (`RIVEMU_REQUIRE_RISCV_TESTS=1`) |

`fetch-linux-test.sh` refuses unpinned inputs unless
`RIVEMU_LINUX_TEST_UNPINNED=1` is set. `linux_devices.rs` also needs
OpenSBI from `linux-test/fw_dynamic.bin`.
