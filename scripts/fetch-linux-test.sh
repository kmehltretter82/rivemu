#!/usr/bin/env bash
# Fetches OpenSBI, an upstream Debian riscv64 Linux kernel and builds a minimal
# BusyBox initramfs into images/linux-test/ for Linux boot smoke tests.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/images/linux-test"
mkdir -p "$OUT"

DEBIAN_BASE="https://deb.debian.org/debian/dists/trixie/main/installer-riscv64/current/images/netboot/debian-installer/riscv64"
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
        curl -fsSL "$DEBIAN_BASE/linux" -o "$OUT/Image"
    fi
fi

# 3. Minimal BusyBox rootfs.cpio
if [[ ! -f "$OUT/rootfs.cpio" ]]; then
    initrd_gz="$OUT/debian-initrd.gz"
    if [[ -f "$ROOT/images/debian/initrd.gz" ]]; then
        initrd_gz="$ROOT/images/debian/initrd.gz"
    elif [[ ! -f "$initrd_gz" ]]; then
        echo "Fetching Debian riscv64 initrd.gz..."
        curl -fsSL "$DEBIAN_BASE/initrd.gz" -o "$initrd_gz"
    fi

    echo "Building BusyBox rootfs.cpio..."
    work="$(mktemp -d)"
    (
        cd "$work"
        gzip -dc "$initrd_gz" | cpio -idmu \
            'bin' 'sbin' 'lib' \
            'usr/bin/busybox' \
            'usr/lib/ld-linux*' \
            'usr/lib/riscv64-linux-gnu/ld-linux*' \
            'usr/lib/riscv64-linux-gnu/libc.so*' \
            'usr/lib/riscv64-linux-gnu/libm.so*' \
            'usr/lib/riscv64-linux-gnu/libresolv*' 2>/dev/null || true
        mkdir -p proc sys dev tmp etc usr/sbin
        ln -sf ../bin/busybox usr/bin/sh
        cat > init << 'EOF'
#!/bin/busybox sh
/bin/busybox mkdir -p /proc /sys /dev /tmp
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
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
