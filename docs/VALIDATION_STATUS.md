# Validation status

Last updated: 2026-11-02

This file records observed exit evidence separately from the implementation
checkboxes in [`PLAN.md`](PLAN.md). A checked milestone means that its code was
implemented; it does not by itself prove every stated exit criterion.

## 2026-11-02 cloud validation (branch `claude/trusting-bell-xvhb0r`)

These results come from a Claude Code cloud container (Linux 6.18, x86_64,
4 vCPUs). They supersede the "Partial or missing" rows below where they
overlap. Findings, causes and fixes are in
[`REVIEW-20261102.md`](REVIEW-20261102.md). Every row links its log.

| Area | Evidence | Result |
|---|---|---|
| `riscv-arch-test` privileged subset | `tests/arch-test/README.md`; `evidence/2026-11-02/arch-test/` | **418/418** in 15 privileged suites (Sail 0.16 reference). Before the three fixes it found (`mcycle`, `mtime`→`time`, CBO PMA), 411/418. |
| `riscv-arch-test` unprivileged suites | `evidence/2026-11-02/arch-test/summary-rv64i.txt` | **833/833** in the 43 `rv64i` suites (I, M, A, F, D, Zfh, Zfa, C, B, scalar crypto, Zicond, Zimop, Zicbo*, counters and others). With Zama16b and the matching reference config, all 1251 pass locally (`summary-zama16b.txt`) and in CI (job `arch-test`). |
| Official `riscv-tests`, fresh build | `RIVEMU_REQUIRE_RISCV_TESTS=1 cargo test --release -p rivemu-cpu-riscv` | 244/244 from a from-scratch build. Before `b8045b9`, a fresh build failed 22 `-v` tests, and CI passed only from a cache. |
| Linux device E2E: reboot, NVMe, virtio-gpu/input, vsock, SBI PMU/perf, Snapshot v2 | `RIVEMU_LINUX_DEVICES_TEST=1 cargo test --release -p rivemu-machine --test linux_devices`; `evidence/2026-11-02/linux-devices-e2e.log` | 6/6 with Linux 6.12 + OpenSBI v1.7, locally and in CI (job `linux-devices`, required mode). Snapshot v2 does not contain disk contents (the test shares the backend). |
| P27–P32 per-suite differential campaigns | `rivemu-lockstep/docs/evidence/2026-11-02-campaigns/summary.txt` | 18 suites × {QEMU 11.1.3 `--step-check`, Sail 0.16}: 36/36 PASS. |
| Expanded `Suite::Vector` (random LMUL/SEW/mask, segments, indexed, reserved encodings) | `rivemu-lockstep` `3c57b83` | 256 blocks pass against Sail and QEMU after three RVV fixes. |
| Boot lockstep P16/P26 (OpenSBI v1.7 + Linux 6.12 vs QEMU 11.1.3) | `rivemu-lockstep/docs/evidence/2026-11-02-boot-lockstep/run13-pass-30M-steps.log` | **30,000,000 instructions, 0 divergences** with `--check-csrs` (3M in run 10, 10M in run 12). Each earlier run stopped at a harness configuration difference or a rivemu bug (semihosting, `mstateen` reset); all are fixed. |
| Trace-driven boot lockstep to the shell prompt | `rivemu-lockstep/docs/evidence/2026-11-03-boot-trace/` | OpenSBI + Linux 6.12 + BusyBox from the firmware entry to `~ # `: 3/3 runs, 358.6–362.8 M instructions each, 0 divergences; CI job `boot-trace` in rivemu-lockstep. |
| H extension (`-cpu rv64,h=on`) | `rivemu-lockstep/docs/evidence/2026-11-03-h-extension/`; `crates/rivemu-cpu-riscv/tests/hypervisor.rs` | A 160-check test firmware (`rivemu-lockstep/firmware/h_test.S`) passes on QEMU 11.1.3 alone, on rivemu alone, and in trace lockstep (43,107 instructions, 61 exceptions, 4 interrupts, 0 divergences). OpenSBI + Linux with H on both sides reaches the shell prompt in lockstep with 0 divergences (364.9 M instructions). Two injected H bugs are each caught at the first trap they affect. QEMU differs from the spec in ten places around H (for example `mstatus.MPV` is not writable); the firmware's spec build adds 19 checks there, which the Sail model 0.16 and rivemu pass and QEMU fails 11 of. 11 rivemu unit tests cover the same ground. |
| P17 on Apple Silicon | `evidence/2026-11-03/p17-superblocks.txt`, `evidence/2026-11-03/p17-ci-apple-silicon.txt` | CI `p17-bench (macos-latest)`, `21acfa8` (run 41636562098), on the slowest macOS runner so far (base 25.36 / 29.30 s): **6.22 s** SMP1 (p95 7.67, all 9 samples under 8 s) / **7.03 s** SMP4 medians (targets 8 / 10 s). |
| Sail multi-worker startup | `rivemu-lockstep/docs/evidence/2026-11-02-sail-startup/` | 25/25 clean 6-worker campaigns without the startup lock (was 17/24). |
| CI hygiene | GitHub Actions on both PRs | `cargo fmt --check`, `clippy -D warnings`, MSRV 1.85, and `cargo deny` (rivemu) pass. rivemu-lockstep now has CI, including the differential suites. |

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
| GPU and input guest operation | Verified 2026-11-02 | Linux could not use virtio-gpu at all (wrong command codes, SHM registers); fixed in `2f10801`. `linux_devices` now asserts `/dev/dri/card0`, `/dev/fb0`, the evdev devices, and pixels reaching the scanout. |
| P19 NVMe, vsock, Snapshot v2 exits | Verified 2026-11-02, except disk state | `linux_devices`: Linux reads and writes NVMe, connects `AF_VSOCK` to CID 2, and NVMe and virtio-pci keep working after save/restore into a fresh machine. Snapshots do not contain disk contents, so no QCOW2 restore is claimed. |
| Poweroff and reboot | Verified 2026-11-02 | `linux_devices`: `reboot -f` reaches `PowerEvent::Reset`, twice. |
| SMP4 stability | Partial | The functional SMP4 boots pass and the isolated P17 campaign completed 20/20 SMP4 shell boots, but there is no repeated full device-smoke campaign. |
| P17 `< 8 s` single-hart / `< 10 s` SMP4 | **Met on Apple Silicon** (medians), 2026-11-03 | `docs/PLAN.md` defines the target on Apple Silicon. CI job `p17-bench (macos-latest)` (GitHub's M1-class arm64 runner) runs the P17 fixture (`tests/linux_perf.rs`: Debian inputs, `Machine::new()` to the first `~ # `) for the PR head and its base on the same runner, 9 samples each. Up to `8247296` the ratio to base on macOS stayed at 0.37, so slower runners missed 8 s (8.96 s at `27ddcbf`, base 24.49 s; 8.79 s at `8247296`, base 23.95 s). `21acfa8` (superblocks, a fetch fast TLB, cheaper block chains; `evidence/2026-11-03/p17-superblocks.txt`) cut the P17 boot's host instructions by 15% and the macOS ratio to 0.245: run 41636562098, on the slowest macOS runner so far (base 25.36 / 29.30 s), **6.22 s** SMP1 (p95 7.67; samples 5.76 to 7.67 s) and **7.03 s** SMP4 (p95 11.40: the three round-1 samples, 10.0 to 11.4 s, fell in a slow period of the runner that also shows in base, 30.9 and 34.5 s against 24 to 25 s later; the other six are 6.4 to 7.6 s). Ubuntu runner, same run: 4.96 / 5.47 s (base 20.68 / 23.92 s). Earlier rounds are in `evidence/2026-11-03/p17-ci-apple-silicon.txt`; the H code costs +0.095% host instructions with H off (`evidence/2026-11-03/h-extension-cost.txt`). |
| `riscv-arch-test` privileged subset | Verified 2026-11-02 | 418/418 (`tests/arch-test/README.md`). |
| Guest PMU/perf | Verified 2026-11-02 (counting) | `linux_devices`: `riscv-pmu-sbi` probes 16 firmware and 31 hardware counters, and `perf_event_open` counts instructions. PMU overflow interrupts are not asserted. |
| Boot lockstep P16/P26 | **Verified to the shell prompt**, 2026-11-03 | `rivemu-lockstep boot-trace` (QEMU at full speed with a trace plugin; rivemu checks every instruction, mirrors QEMU's interrupts, and takes QEMU's results for device-dependent reads): OpenSBI v1.7 + Linux 6.12 + BusyBox from the firmware entry to `~ # `, three runs of 358.6 M to 362.8 M instructions with 12,129 to 13,077 interrupts, **0 divergences** (`rivemu-lockstep/docs/evidence/2026-11-03-boot-trace/`). An injected `sraiw` bug is caught after 18.2 M instructions. The GDB-stepped `boot` mode reached 30,000,000 instructions with `--check-csrs` on 2026-11-02. Limits: one hart; rivemu runs through `Hart::step`, so the block and inline fast paths are covered by the equivalence tests, riscv-arch-test and the Linux E2E tests instead. |
| H extension | **Implemented; verified against QEMU and the Sail model**, 2026-11-03 | See the 2026-11-03 table above. Not covered: a KVM guest under Linux; guest external interrupts (GEILEN = 0, no AIA); `htinst`/`mtinst` are always written as 0 (allowed); Sail runs the firmware standalone, not in lockstep; the third-party `riscv-hyp-tests` suite was not run (building it was not permitted in the cloud session). |
| P27-P32 Sail/QEMU differential claims | Verified per suite 2026-11-02 | All 18 suites pass against Sail and against QEMU with `--step-check`. The Vector suite needed rework first; it had ignored its RNG. |
| Benchmarks | Partial | The repeated exact-boundary P17 boot benchmark is retained. No CoreMark, Dhrystone, or criterion history was found. |
| CI hygiene | Verified 2026-11-02 | Both repositories pass `cargo fmt --check` and `clippy --all-targets -D warnings` in CI, rivemu also MSRV 1.85 and `cargo deny check`, and rivemu-lockstep runs `cargo deny` through `cargo-deny-action`. In the cloud session `cargo-deny` could not be installed locally, so its results come from CI only. |

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

Ranked; details in [`REVIEW-20261102.md`](REVIEW-20261102.md) §6.

1. P17 is met on Apple Silicon by median with margin (6.22 / 7.03 s on the
   slowest macOS runner so far). The SMP4 p95 on that runner (11.4 s) came
   from one slow period of the runner; a larger sample on dedicated Apple
   hardware would settle the tail.
2. Trace lockstep: SMP (trace every hart), and a mode that runs rivemu's
   block path (`Hart::run`) instead of `Hart::step` between records.
3. H extension: boot a KVM guest under Linux (needs a KVM-enabled kernel
   and a VMM in the image); guest external interrupts (AIA/IMSIC); run
   `riscv-hyp-tests`.
4. riscv-arch-test runs in CI (job `arch-test`, 1251/1251 in run 41568117929; the ELFs are cached by configuration).
5. `linux_devices` now runs in CI (job `linux-devices`; the images build in
   about 20 min and are then cached).
6. Decide whether CI should supply the 9p-capable kernel and enable
   `RIVEMU_REQUIRE_9P=1`.
7. Add an explicit arbitrary outbound-UDP NAT guest test.
