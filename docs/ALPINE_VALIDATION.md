# Alpine disk and networking validation

This records the 2026-11-02 P4 validation fixture and commands. The generated
disk is a test artifact, not a distributable image.

## Inputs

- Alpine 3.24.2 RISC-V minirootfs:
  `/Users/kmehltretter/codex/linux-work/peterz/local-results/rv-earlyirq-riscv-joint-gate-20261021-a/alpine/alpine-minirootfs-3.24.2-riscv64.tar.gz`
  (`57132e6e4f3a4ba9ffdf24e485513ce507e45471e7fd565aeed91c055cd63f7b`)
- Linux image with ext4, devtmpfs, VirtIO MMIO/block/net, and DHCP built in:
  `/private/tmp/rivemu-validation/kvm-zicfilp-ab-20261102/baseline.Image`
  (`e8a2c965c06ebb682ffcb0dc0511fb8638dba2a73ba129fa05141257fa46eab8`)
- External OpenSBI: `images/linux-test/fw_dynamic.bin`
  (`49bdf7b939bda11321132d1042bf99d7324fb190f1feef423171fed3573f8705`)

The minirootfs was extracted into a temporary directory. Its absent OpenRC
startup was replaced by a BusyBox sysinit entry that mounts `proc`, `sysfs`,
`devtmpfs`, `devpts`, and `tmpfs`, brings up `eth0`, and runs `udhcpc`. The DHCP
hook configures `10.0.2.15/24`, the default route, and `/etc/resolv.conf` from
the lease. A `ttyS0` getty was enabled and the test-only root password was set
to `rivemu`. The directory was copied into a 512 MiB ext4 image with
`mke2fs -d`.

The resulting pristine fixture is
`/private/tmp/rivemu-validation/alpine-riscv64.ext4`, SHA-256
`f4fd312055ee192be5977c3caf096b1066652f699c9c00712814a2cc50d8000d`.
Use a copy for each run because the guest mounts it read-write.

## Boot command

```sh
rivemu-system-riscv64 \
  -M virt -m 512M -smp 4 -nographic \
  -bios images/linux-test/fw_dynamic.bin \
  -kernel /private/tmp/rivemu-validation/kvm-zicfilp-ab-20261102/baseline.Image \
  -append "console=ttyS0 root=/dev/vda rootfstype=ext4 rw rootwait" \
  -drive id=hd0,file=/private/tmp/rivemu-validation/alpine-riscv64-run.ext4,format=raw,if=none \
  -device virtio-blk-device,drive=hd0 \
  -netdev user,id=net0,hostfwd=tcp::2222-:2222,hostfwd=udp::2223-:2223 \
  -device virtio-net-device,netdev=net0 \
  -device virtio-rng-device
```

## Observed exit evidence

After logging in as `root`, the guest successfully ran:

```sh
ip -4 addr show dev eth0
nslookup dl-cdn.alpinelinux.org
apk add --no-cache curl
curl --version
```

The DHCP lease was `10.0.2.15`, DNS was `10.0.2.3`, `apk` installed `curl` and
eight dependencies over HTTPS, and the guest powered off cleanly. Separate
BusyBox `nc` listeners on guest ports 2222/TCP and 2223/UDP echoed host payloads
through both `hostfwd` paths.

Evidence hashes are listed in [`VALIDATION_STATUS.md`](VALIDATION_STATUS.md).
The DNS proxy currently resolves A records synchronously through the host; it
does not implement AAAA, DNSSEC, or raw DNS forwarding.
