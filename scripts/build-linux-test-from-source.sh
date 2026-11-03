#!/usr/bin/env bash
# Builds a Linux test kernel and a static BusyBox initramfs from source into
# images/linux-src/, without Debian downloads. Every driver the device
# end-to-end tests need is built in (no modules): virtio-mmio/pci, blk, net,
# console, rng, 9p, vsock, gpu, input, NVMe, the SBI PMU, Goldfish RTC, and the
# syscon poweroff/reboot drivers used by the sifive_test device.
#
# Requirements (Ubuntu 24.04): gcc-riscv64-linux-gnu flex bison bc cpio
# libssl-dev libelf-dev git make.
# Usage: scripts/build-linux-test-from-source.sh   (about 25 minutes on 4 cores)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/images/linux-src"
SRC="$OUT/src"
JOBS="${JOBS:-$(nproc)}"
CROSS="${CROSS_COMPILE:-riscv64-linux-gnu-}"
LINUX_TAG="${LINUX_TAG:-v6.12}"
LINUX_COMMIT="${LINUX_COMMIT:-adc218676eef25575469234709c2d87185ca223a}"
BUSYBOX_TAG="${BUSYBOX_TAG:-1_36_1}"
BUSYBOX_COMMIT="${BUSYBOX_COMMIT:-1a64f6a20aaf6ea4dbba68bbfa8cc1ab7e5c57c4}"
mkdir -p "$OUT" "$SRC"

# clone_verified <url> <tag> <commit> <dir>
clone_verified() {
    if [[ ! -d "$4/.git" ]]; then
        git clone -q --depth 1 --branch "$2" "$1" "$4"
    fi
    local actual
    actual="$(git -C "$4" rev-parse HEAD)"
    if [[ "$actual" != "$3" ]]; then
        echo "error: $1 $2 is $actual, expected $3" >&2
        exit 1
    fi
}

# 1. Linux kernel Image. Sources are cloned only when an output is missing,
# so a cached Image/busybox (CI) needs no network.
cat > "$OUT/linux.fragment" <<'EOF'
CONFIG_MODULES=n
CONFIG_DEBUG_INFO_NONE=y
CONFIG_BLK_DEV_INITRD=y
CONFIG_DEVTMPFS=y
CONFIG_DEVTMPFS_MOUNT=y
CONFIG_PCI=y
CONFIG_PCI_HOST_GENERIC=y
CONFIG_PCI_MSI=y
CONFIG_VIRTIO_MENU=y
CONFIG_VIRTIO=y
CONFIG_VIRTIO_MMIO=y
CONFIG_VIRTIO_PCI=y
CONFIG_VIRTIO_BLK=y
CONFIG_VIRTIO_NET=y
CONFIG_VIRTIO_CONSOLE=y
CONFIG_HW_RANDOM=y
CONFIG_HW_RANDOM_VIRTIO=y
CONFIG_NET_9P=y
CONFIG_NET_9P_VIRTIO=y
CONFIG_9P_FS=y
CONFIG_VSOCKETS=y
CONFIG_VIRTIO_VSOCKETS=y
CONFIG_BLK_DEV_NVME=y
CONFIG_DRM=y
CONFIG_DRM_VIRTIO_GPU=y
CONFIG_FB=y
CONFIG_INPUT_EVDEV=y
CONFIG_VIRTIO_INPUT=y
CONFIG_PERF_EVENTS=y
CONFIG_RISCV_PMU=y
CONFIG_RISCV_PMU_SBI=y
CONFIG_RTC_CLASS=y
CONFIG_RTC_DRV_GOLDFISH=y
CONFIG_POWER_RESET=y
CONFIG_POWER_RESET_SYSCON=y
CONFIG_POWER_RESET_SYSCON_POWEROFF=y
CONFIG_SERIAL_8250=y
CONFIG_SERIAL_8250_CONSOLE=y
CONFIG_SERIAL_OF_PLATFORM=y
CONFIG_EXT4_FS=y
CONFIG_KVM=n
EOF
if [[ ! -f "$OUT/Image" ]]; then
    clone_verified https://github.com/torvalds/linux.git "$LINUX_TAG" "$LINUX_COMMIT" "$SRC/linux"
    make -C "$SRC/linux" -s ARCH=riscv CROSS_COMPILE="$CROSS" O="$OUT/linux-build" defconfig
    "$SRC/linux/scripts/kconfig/merge_config.sh" -m -O "$OUT/linux-build" \
        "$OUT/linux-build/.config" "$OUT/linux.fragment" >/dev/null
    make -C "$SRC/linux" -s ARCH=riscv CROSS_COMPILE="$CROSS" O="$OUT/linux-build" olddefconfig
    # Fail if a requested option did not survive dependency resolution.
    missing=0
    while IFS= read -r line; do
        [[ "$line" == CONFIG_*=y ]] || continue
        if ! grep -qx "$line" "$OUT/linux-build/.config"; then
            echo "error: $line not set in the resolved kernel config" >&2
            missing=1
        fi
    done < "$OUT/linux.fragment"
    [[ "$missing" == 0 ]]
    make -C "$SRC/linux" -s ARCH=riscv CROSS_COMPILE="$CROSS" O="$OUT/linux-build" -j"$JOBS" Image
    cp "$OUT/linux-build/arch/riscv/boot/Image" "$OUT/Image"
fi

# 2. Static BusyBox.
if [[ ! -f "$OUT/busybox" ]]; then
    clone_verified https://github.com/mirror/busybox.git "$BUSYBOX_TAG" "$BUSYBOX_COMMIT" "$SRC/busybox"
    mkdir -p "$OUT/busybox-build"
    make -C "$SRC/busybox" -s O="$OUT/busybox-build" defconfig
    sed -i -e 's/^# CONFIG_STATIC is not set/CONFIG_STATIC=y/' \
        -e 's/^CONFIG_TC=y/# CONFIG_TC is not set/' "$OUT/busybox-build/.config"
    make -C "$OUT/busybox-build" -s CROSS_COMPILE="$CROSS" -j"$JOBS" busybox
    cp "$OUT/busybox-build/busybox" "$OUT/busybox"
fi

# 3. Initramfs with the same /init protocol as scripts/fetch-linux-test.sh.
work="$(mktemp -d)"
(
    cd "$work"
    mkdir -p bin sbin usr/bin usr/sbin proc sys dev tmp etc
    cp "$OUT/busybox" bin/busybox
    ln -s busybox bin/sh
    cat > init << 'EOF'
#!/bin/busybox sh
/bin/busybox mkdir -p /proc /sys /dev /tmp
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
/bin/busybox --install -s
echo "=== rivemu BusyBox initramfs (built from source) ==="
uname -srm
if grep -q "rivemu.poweroff" /proc/cmdline; then
    echo "RIVEMU_SMOKE_OK"
    poweroff -f
fi
if grep -q "rivemu.reboot" /proc/cmdline; then
    echo "RIVEMU_REBOOT_OK"
    reboot -f
fi
if [ -x /run-test ]; then
    exec /run-test
fi
exec sh
EOF
    chmod +x init
    find . | cpio -o -H newc > "$OUT/rootfs.cpio" 2>/dev/null
)
rm -rf "$work"

for f in Image busybox rootfs.cpio; do
    sha256sum "$OUT/$f"
done
