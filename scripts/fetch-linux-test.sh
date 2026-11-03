#!/usr/bin/env bash
# Fetches OpenSBI, an upstream Debian riscv64 Linux kernel and builds a minimal
# BusyBox initramfs (including virtio_mmio, virtio_blk, virtio_net, virtio-rng
# kernel modules) into images/linux-test/ for Linux boot and VirtIO smoke tests.
#
# Inputs are verified against pinned SHA-256 values so that every machine and CI
# run boots the same firmware and kernel. Debian's installer "current" directory
# moves with point releases; when it does, the kernel check fails loudly and the
# pin must be updated deliberately. Set RIVEMU_LINUX_TEST_UNPINNED=1 to accept
# unpinned inputs for a local experiment (their hashes are still printed).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/images/linux-test"
mkdir -p "$OUT"

DEBIAN_IMAGES="https://deb.debian.org/debian/dists/trixie/main/installer-riscv64/current/images"
OPENSBI_URL="https://github.com/riscv-software-src/opensbi/releases/download/v1.7/opensbi-1.7-rv-bin.tar.xz"
OPENSBI_TARBALL_SHA256="97a6f53276495203c25db8ce0f707936af96f0bfa0df5da9e50c426c12667164"
OPENSBI_FW_DYNAMIC_SHA256="4be292368f8c24371101f1b1048ede17284210545378994b193852d779d59bfe"
DEBIAN_KERNEL_SHA256="8898c27a38f0bfcc3d9a4b0687ee752c4a2b3028576f919ba8955d2cbd6d4722"
DEBIAN_INITRD_SHA256="d67a00121f695898895ad41cb47ee4d21cecf07ed05a767a23c61d6897d0a844"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# verify <file> <expected-sha256> <description>
verify() {
    local actual
    actual="$(sha256 "$1")"
    echo "sha256 $actual  $3"
    if [[ -n "$2" && "$actual" != "$2" ]]; then
        if [[ "${RIVEMU_LINUX_TEST_UNPINNED:-}" == 1 ]]; then
            echo "warning: $3 does not match pinned $2 (RIVEMU_LINUX_TEST_UNPINNED=1)" >&2
        else
            echo "error: $3 has SHA-256 $actual, expected $2" >&2
            echo "       Update the pin in $0 or set RIVEMU_LINUX_TEST_UNPINNED=1." >&2
            rm -f "$1"
            exit 1
        fi
    fi
}

# 1. OpenSBI fw_dynamic.bin from the pinned v1.7 release. A QEMU-bundled copy
#    differs between QEMU builds, so it is only used when requested explicitly
#    with RIVEMU_OPENSBI=/path/to/fw_dynamic.bin.
if [[ ! -f "$OUT/fw_dynamic.bin" ]]; then
    if [[ -n "${RIVEMU_OPENSBI:-}" ]]; then
        cp "$RIVEMU_OPENSBI" "$OUT/fw_dynamic.bin"
    else
        echo "Fetching OpenSBI v1.7..."
        tmp_sbi="$(mktemp -d)"
        curl -fsSL "$OPENSBI_URL" -o "$tmp_sbi/opensbi.tar.xz"
        verify "$tmp_sbi/opensbi.tar.xz" "$OPENSBI_TARBALL_SHA256" "opensbi-1.7-rv-bin.tar.xz"
        tar -xJf "$tmp_sbi/opensbi.tar.xz" -C "$tmp_sbi"
        cp "$tmp_sbi/opensbi-1.7-rv-bin/share/opensbi/lp64/generic/firmware/fw_dynamic.bin" "$OUT/fw_dynamic.bin"
        rm -rf "$tmp_sbi"
    fi
fi
if [[ -n "${RIVEMU_OPENSBI:-}" ]]; then
    verify "$OUT/fw_dynamic.bin" "" "fw_dynamic.bin (RIVEMU_OPENSBI override)"
else
    verify "$OUT/fw_dynamic.bin" "$OPENSBI_FW_DYNAMIC_SHA256" "fw_dynamic.bin"
fi

# 2. Linux kernel
if [[ ! -f "$OUT/Image" ]]; then
    if [[ -f "$ROOT/images/debian/linux" ]]; then
        cp "$ROOT/images/debian/linux" "$OUT/Image"
    else
        echo "Fetching Debian riscv64 Linux kernel..."
        curl -fsSL "$DEBIAN_IMAGES/cdrom/vmlinux" -o "$OUT/Image"
    fi
fi
if [[ -n "${RIVEMU_LINUX_TEST_KERNEL_SHA256+set}" ]]; then
    DEBIAN_KERNEL_SHA256="$RIVEMU_LINUX_TEST_KERNEL_SHA256"
fi
verify "$OUT/Image" "$DEBIAN_KERNEL_SHA256" "Image"

# 3. Minimal BusyBox rootfs.cpio (with virtio kernel modules from cdrom/initrd.gz)
if [[ ! -f "$OUT/rootfs.cpio" ]]; then
    initrd_gz="$OUT/debian-cdrom-initrd.gz"
    if [[ ! -f "$initrd_gz" ]]; then
        echo "Fetching Debian riscv64 cdrom/initrd.gz..."
        curl -fsSL "$DEBIAN_IMAGES/cdrom/initrd.gz" -o "$initrd_gz"
    fi
    verify "$initrd_gz" "$DEBIAN_INITRD_SHA256" "debian-cdrom-initrd.gz"

    echo "Building BusyBox + VirtIO rootfs.cpio..."
    work="$(mktemp -d)"
    (
        cd "$work"
        gzip -dc "$initrd_gz" | cpio -idmu \
            'bin' 'sbin' 'lib' \
            'usr/bin/busybox' \
            'usr/bin/kmod' \
            'usr/sbin/insmod' \
            'usr/lib/ld-linux*' \
            'usr/lib/riscv64-linux-gnu/ld-linux*' \
            'usr/lib/riscv64-linux-gnu/libc.so*' \
            'usr/lib/riscv64-linux-gnu/liblzma.so*' \
            'usr/lib/riscv64-linux-gnu/libm.so*' \
            'usr/lib/riscv64-linux-gnu/libresolv*' \
            'usr/lib/modules/*/kernel/drivers/virtio/virtio_mmio.ko.xz' \
            'usr/lib/modules/*/kernel/drivers/block/virtio_blk.ko.xz' \
            'usr/lib/modules/*/kernel/drivers/char/hw_random/virtio-rng.ko.xz' \
            'usr/lib/modules/*/kernel/net/core/failover.ko.xz' \
            'usr/lib/modules/*/kernel/drivers/net/net_failover.ko.xz' \
            'usr/lib/modules/*/kernel/drivers/net/virtio_net.ko.xz' 2>/dev/null || true

        mkdir -p proc sys dev tmp etc usr/sbin usr/lib/modules-flat
        for ko_xz in $(find usr/lib/modules -name '*.ko.xz' 2>/dev/null); do
            base="$(basename "$ko_xz" .xz)"
            xz -dc "$ko_xz" > "usr/lib/modules-flat/$base"
        done
        rm -rf usr/lib/modules

        ln -sf ../bin/busybox usr/bin/sh
        cat > init << 'EOF'
#!/bin/busybox sh
/bin/busybox mkdir -p /proc /sys /dev /tmp
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
for m in virtio_mmio.ko virtio_blk.ko virtio-rng.ko failover.ko net_failover.ko virtio_net.ko; do
    if [ -f "/usr/lib/modules-flat/$m" ]; then
        /usr/sbin/insmod "/usr/lib/modules-flat/$m"
    fi
done
/bin/busybox mdev -s 2>/dev/null || true
echo "=== rivemu BusyBox initramfs ==="
/bin/busybox uname -srm
if /bin/busybox grep -q "rivemu.poweroff" /proc/cmdline; then
    # scripts/p17-profile.sh symbolizes guest PCs with this. Nested here so
    # that the P17 path (no rivemu.poweroff) runs the same commands as before.
    if /bin/busybox grep -q "rivemu.kallsyms" /proc/cmdline; then
        /bin/busybox cat /proc/kallsyms
    fi
    echo "RIVEMU_SMOKE_OK"
    /bin/busybox poweroff -f
fi
if /bin/busybox grep -q "rivemu.reboot" /proc/cmdline; then
    echo "RIVEMU_REBOOT_OK"
    /bin/busybox reboot -f
fi
exec /bin/busybox sh
EOF
        chmod +x init
        find . | cpio -o -H newc > "$OUT/rootfs.cpio" 2>/dev/null
    )
    rm -rf "$work"
fi

echo "Linux test artifacts ready in $OUT:"
ls -lh "$OUT/fw_dynamic.bin" "$OUT/Image" "$OUT/rootfs.cpio"
