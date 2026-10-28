#!/usr/bin/env bash
# Fetches OpenSBI, an upstream Debian riscv64 Linux kernel and builds a minimal
# BusyBox initramfs (including virtio_mmio, virtio_blk, virtio_net, virtio-rng
# kernel modules) into images/linux-test/ for Linux boot and VirtIO smoke tests.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/images/linux-test"
mkdir -p "$OUT"

DEBIAN_IMAGES="https://deb.debian.org/debian/dists/trixie/main/installer-riscv64/current/images"
OPENSBI_URL="https://github.com/riscv-software-src/opensbi/releases/download/v1.7/opensbi-1.7-rv-bin.tar.xz"

# 1. OpenSBI fw_dynamic.bin
if [[ ! -f "$OUT/fw_dynamic.bin" ]]; then
    for cand in \
        "/opt/homebrew/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin" \
        "/usr/local/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin" \
        "/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin" \
        "/usr/lib/riscv64-linux-gnu/opensbi/generic/fw_dynamic.bin"
    do
        if [[ -f "$cand" ]]; then
            cp "$cand" "$OUT/fw_dynamic.bin"
            break
        fi
    done
fi
if [[ ! -f "$OUT/fw_dynamic.bin" ]]; then
    echo "Fetching OpenSBI v1.7..."
    tmp_sbi="$(mktemp -d)"
    curl -fsSL "$OPENSBI_URL" | tar -xJ -C "$tmp_sbi"
    cp "$tmp_sbi/opensbi-1.7-rv-bin/share/opensbi/lp64/generic/firmware/fw_dynamic.bin" "$OUT/fw_dynamic.bin"
    rm -rf "$tmp_sbi"
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

# 3. Minimal BusyBox rootfs.cpio (with virtio kernel modules from cdrom/initrd.gz)
if [[ ! -f "$OUT/rootfs.cpio" ]]; then
    initrd_gz="$OUT/debian-cdrom-initrd.gz"
    if [[ ! -f "$initrd_gz" ]]; then
        echo "Fetching Debian riscv64 cdrom/initrd.gz..."
        curl -fsSL "$DEBIAN_IMAGES/cdrom/initrd.gz" -o "$initrd_gz"
    fi

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
