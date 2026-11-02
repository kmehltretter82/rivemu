# Validation status

Last updated: 2026-11-02

This file records observed exit evidence separately from the implementation
checkboxes in [`PLAN.md`](PLAN.md). A checked milestone means that its code was
implemented; it does not by itself prove every stated exit criterion.

## Validation target

- `qemu_rust` base: `f0471ad989d202c9b13341ab9325d1f2fdb2fe40`
- `rivemu-lockstep` base: `9e253081722c31698ab03c33d7c3b999601e5cce`
- Both worktrees had uncommitted validation fixes. Results below apply to the
  listed bases plus those changes, not to either base commit alone.
- Linux smoke artifacts:
  - `Image`: `8898c27a38f0bfcc3d9a4b0687ee752c4a2b3028576f919ba8955d2cbd6d4722`
  - `fw_dynamic.bin`: `49bdf7b939bda11321132d1042bf99d7324fb190f1feef423171fed3573f8705`
  - `rootfs.cpio`: `0147597e76632bffe305377000cc457705c6df0452f51a017a21d021d213366b`
- Strict 9p validation kernel (`CONFIG_9P_FS=y`, `CONFIG_NET_9P=y`,
  `CONFIG_NET_9P_VIRTIO=y`):
  `e8a2c965c06ebb682ffcb0dc0511fb8638dba2a73ba129fa05141257fa46eab8`
- Alpine 3.24.2 RISC-V minirootfs:
  `57132e6e4f3a4ba9ffdf24e485513ce507e45471e7fd565aeed91c055cd63f7b`
- Derived 512 MiB Alpine ext4 fixture:
  `f4fd312055ee192be5977c3caf096b1066652f699c9c00712814a2cc50d8000d`

## Verified in the current worktree

| Area | Evidence | Result |
|---|---|---|
| CPU unit and directed integration tests | `cargo test --release -p rivemu-cpu-riscv` | 8 unit tests, 32 directed integration tests, FWFT, and S-mode `seed` tests passed. |
| Official `riscv-tests` | `RIVEMU_REQUIRE_RISCV_TESTS=1 cargo test --release -p rivemu-cpu-riscv` | 244/244 passed. This is not a `riscv-arch-test` result. |
| Machine unit tests | `cargo test --release -p rivemu-machine --lib` | 16/16 passed. |
| Release workspace suite | `cargo test --release --workspace -- --test-threads=1` | All crate, integration, soft-float differential/reference, device, and doc tests passed. Opt-in Linux/performance tests have separate required-mode runs below. |
| User-network backend | `cargo test --release -p rivemu-backends` with loopback permission | 7/7 passed, including DHCP, DNS synthesis/proxy behavior, and a bidirectional UDP `hostfwd` round trip. |
| Built-in Rust SBI Linux boot | `boots_linux_with_builtin_rust_sbi_no_firmware` in the release Linux suite | SMP4 reached the shell, restored a live snapshot in 9.37 ms, passed block/network/console assertions, and powered off in 17.41 s. |
| External OpenSBI Linux boot | `boots_linux_with_virtio_blk_net_console_rng_and_powers_off` | SMP4 reached the shell, passed block/network/console assertions, and powered off in 21.83 s. |
| Original `< 60 s` initramfs target | The two release boots above on the current M-series host | Met for both firmware paths in the final retained run. No repeat/median/p95 full-smoke result is claimed. |
| VirtIO block | Linux smoke test | Guest read the expected disk marker, wrote sector 1, synchronized it, and the host verified the written bytes. |
| Basic guest networking | Linux smoke test | Guest configured a static address and received an ICMP reply from `10.0.2.2`. |
| P4 distro boot and package install | [Alpine 3.24.2 ext4 guest](ALPINE_VALIDATION.md) under external OpenSBI | Guest booted from VirtIO block, obtained `10.0.2.15` by DHCP, reached a real `ttyS0` login, resolved `dl-cdn.alpinelinux.org`, installed `curl` and dependencies over HTTPS, ran `curl --version`, and powered off cleanly. |
| TCP/UDP `hostfwd` | Alpine BusyBox `nc` listeners plus host loopback clients | Host-originated TCP and UDP payloads traversed Rivemu, reached guest ports 2222/2223, and were echoed back byte-for-byte. |
| VirtIO console | Final Linux smoke suite | Guest wrote `VCONSOLE_OK` to `/dev/hvc0` and the host `BufferChardev` assertion received it. |
| RNG and PCI enumeration | Rust PID 1 test | `getrandom` returned 16 nonzero bytes; Linux enumerated four configured PCI functions. |
| P8 VirtIO-9p guest path | `RIVEMU_REQUIRE_9P=1` with the 9p-capable kernel above | Guest mounted the share, read `HOST_9P_PAYLOAD_42`, wrote `WRITTEN_BY_RUST_INIT_OVER_9P`, and the host verified the reply. The retained run passed in 18.39 s at `/private/tmp/rivemu-validation/rivemu-9p-strict.log`. |
| Snapshot v1 threshold | Built-in Linux boot and snapshot unit test | Live SMP4 restore was 9.37 ms, below P10's 50 ms threshold. |
| P20 long differential campaigns | `suite=all`, 4 workers, 3,125 blocks x 32 requested instructions, seed `0x1000` | QEMU and Sail each reached a terminal `PASS:` for 100,000 requested instructions. Sail compared every instruction; QEMU used its default block-checkpoint behavior, with per-instruction checks only where the harness selects them. |

The Linux boot validation found and fixed a real decoded-basic-block bug. The
old `populate_bbcache_same_page()` loop could wrap from virtual offset `0xffc`
to the next virtual page while continuing at the physically adjacent address.
`virtio_net.ko` had an `auipc`/`jalr` pair exactly across that boundary, causing
Rivemu to execute `0x37d4` from the wrong physical page while the live virtual
address contained a legal `jalr`. The fix stops block construction at the
virtual-page boundary. The noncontiguous-Sv39-page regression and the complete
built-in-SBI Linux test both pass with the fix.

## Partial or missing exit evidence

| Claim | Status | What the current evidence actually proves |
|---|---|---|
| User-network breadth | Partial | Static ICMP, DHCP DISCOVER/REQUEST/ACK, external A-record resolution, outbound TCP/TLS package download, and TCP/UDP `hostfwd` pass guest E2E. Arbitrary outbound UDP NAT is not separately exercised, and the DNS proxy is synchronous and A-only. |
| Default Debian 9p coverage | Partial | The downloaded default kernel printed `RUST_9P_MOUNT_ERR=No such device`. The test now reports optional coverage explicitly; `RIVEMU_REQUIRE_9P=1` makes mount/read/write mandatory and passed with the alternate kernel above. |
| GPU and input guest operation | Partial | PCI enumeration and device unit tests pass; Linux scanout/input behavior is not asserted. |
| P19 NVMe, vsock, Snapshot v2 exits | Partial | Device-level queue/stream/snapshot tests exist. No retained Linux mount, `AF_VSOCK`, or PCI/QCOW2 restore result proves the stated end-to-end exits. |
| Poweroff and reboot | Partial | Clean poweroff is asserted. No current guest reboot integration result was retained. |
| SMP4 stability | Partial | The functional SMP4 boots pass and the isolated P17 campaign completed 20/20 SMP4 shell boots, but there is no repeated full device-smoke campaign. |
| P17 `< 8 s` single-hart / `< 10 s` SMP4 | Failed | An isolated exact-boundary campaign (post-`Machine::new()` to first `~ # `, 2 warmups + 20 recorded samples each) measured SMP1 median/p95 18.905/19.929 s and SMP4 21.103/22.175 s. |
| `riscv-arch-test` privileged subset | Not verified | No runner result or signature artifact was found. The 244 official `riscv-tests` are a different suite. |
| Guest PMU/perf | Not verified | Directed PMU semantics pass, but no guest `perf stat` or PMU-overflow integration result was retained. |
| Boot lockstep P16/P26 | Not verified | The implementation and directed unit tests exist; no retained zero-divergence boot-v3 run was found. |
| P27-P32 Sail/QEMU differential claims | Partial | Directed CPU tests and the generic `suite=all` campaigns pass, but those campaigns do not prove every explicit per-suite exit. QEMU's ordinary-suite checkpoint comparison can also miss a transient difference overwritten before the checkpoint. |
| Benchmarks | Partial | The repeated exact-boundary P17 boot benchmark is retained. No CoreMark, Dhrystone, or criterion history was found. |
| CI hygiene | Partial | `git diff --check` passes, and the changed lockstep Sail file passes targeted `rustfmt --check`. Repo-wide formatting checks fail on broad existing drift, while `clippy -D warnings` stops on five untouched `qemu_rust` vector warnings and three unchanged lockstep QEMU-test warnings. `cargo-deny` is not installed. No broad unrelated rewrite was made. |

## Retained validation transcripts

- Final CPU suite: `/private/tmp/rivemu-validation/rivemu-cpu-final-20261102.log`
  passed the 8 unit, FWFT, 32 directed integration, required official
  `riscv-tests`, and S-mode `seed` tests. SHA-256:
  `896ba8d8fc79fc90a588bf5a41c0be09b602466cffad7e17f3e29e75589c548d`.
- Final machine-library suite:
  `/private/tmp/rivemu-validation/rivemu-machine-lib-final-20261102.log` passed
  16/16 tests. SHA-256:
  `733210132ec7b544578f659642d1af2ff8d68b1c76924890af2c6895424927e5`.
- Final release workspace suite:
  `/private/tmp/rivemu-validation/qemu-rust-workspace-final-20261102.log`
  passed. SHA-256:
  `ec70d86d87111a752e3b111b5d8f7a3e55133b267c226c9c342294d11f492eea`.
- Final user-network backend suite:
  `/private/tmp/rivemu-validation/rivemu-backends-final-20261102.log` passed
  7/7 tests. SHA-256:
  `99aeff7bcd927b9302e58e713c1fb25b4d6cfcb82f8b54149b80fb6cc8e97094`.
- QEMU campaign: `/private/tmp/rivemu-validation/qemu-campaign-final-100k.log`
  reached 3,125/3,125 blocks and a terminal `PASS:` for 100,000 requested
  instructions in 386.27 s. SHA-256:
  `19faf51fc98beec39490fbceee0238045c07f431b87d28569d4efae6cc2edd13`.
- Sail campaign: `/private/tmp/rivemu-validation/sail-campaign-final-100k.log`
  reached 3,125/3,125 blocks and a terminal `PASS:` for 100,000 per-instruction
  comparisons in 560.23 s. SHA-256:
  `befd9a9a176609099b9000e3cff4e07605bf172d48ae3bf40709eb3ecba4d583`.
- Final release Linux suite:
  `/private/tmp/rivemu-validation/linux-boot-final-network-20261102.log` passed
  all three serialized tests, including host-side VirtIO-console reception, in
  49.63 s. The default kernel did not exercise 9p, which is covered by the
  separate strict run. SHA-256:
  `f24ba4e9d65724ef743add41220d5a558e5598079d09a0d055c21e18cadd30b0`.
- Alpine distro/package run:
  `/private/tmp/rivemu-validation/alpine-distro-run-20261102.log` retains DHCP,
  serial login, external DNS, `apk add --no-cache curl`, `curl --version`, and
  clean poweroff. SHA-256:
  `9b92cbe81bfc81e79cf3b0899ae8f992811c48c92cbd8a8a80adfa1a1ce965e1`.
- Alpine TCP/UDP host-forward run and host round trips:
  `/private/tmp/rivemu-validation/alpine-hostfwd-run-20261102.log`,
  `/private/tmp/rivemu-validation/alpine-hostfwd-tcp-20261102.log`, and
  `/private/tmp/rivemu-validation/alpine-hostfwd-udp-20261102.log` have
  SHA-256 values `2839dc65aa1502877902d6e951a579035712abac85457282857aee0ff6199ac1`,
  `5b587bba29a0d1de179b5877f5aedac16a80dbadcb0bd41a6434deba37354c86`,
  and `4d6cee694a59a8c23feca4cb7132749df3926a643658841681abc5d5d9c4b9c0`.
- Isolated P17 campaign:
  `/private/tmp/rivemu-validation/rivemu-p17-isolated-20x-20261102.log`
  completed 20/20 recorded shell boots for both SMP1 and SMP4 and reports the
  failed thresholds above. SHA-256:
  `43af6f290520666abcb700175f51895c0b41ca3c0307ecc744211250471484cf`.
- Strict 9p run: `/private/tmp/rivemu-validation/rivemu-9p-strict.log` passed
  mount/read/write/host-verification in 18.39 s. SHA-256:
  `108ec8536448c13585118286af1d9c16fe64b8c6d7515528d88de3614aa06c79`.

The older `qemu-campaign.log` and `sail-campaign-fixed-100k.log` files remain
historical interrupted runs and are superseded by the terminal-PASS logs above.

## Linux A/B evidence boundary

`linux-ab-results/20261030T001601Z-42538/report.md` is a same-kernel control. It
validates the A/B runner and its evidence handling; it is not evidence that a
kernel bug was fixed.

## Remaining validation work

1. Optimize or revise P17: the exact repeated benchmark now proves that the
   `< 8 s` SMP1 and `< 10 s` SMP4 targets are not met.
2. Decide whether CI should supply the 9p-capable kernel and enable
   `RIVEMU_REQUIRE_9P=1`; local strict mount/read/write evidence is now green.
3. Add retained guest E2E results for reboot, NVMe, vsock, GPU/input, PMU/perf,
   and Snapshot v2.
4. Add an explicit arbitrary outbound-UDP NAT guest test; DHCP/DNS, outbound
   TCP/TLS, and both TCP/UDP `hostfwd` directions are already covered.
5. Run the privileged `riscv-arch-test` subset and retain its signatures.
6. Run final workspace formatting, lint, dependency-policy, and CI checks after
   the current source changes are settled.

