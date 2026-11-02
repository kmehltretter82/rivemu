//! End-to-end Linux guest tests for devices that otherwise only have
//! device-level tests: guest reboot, PCIe NVMe, virtio-gpu and virtio-input,
//! virtio-vsock, the SBI PMU, and Snapshot v2 with PCI devices.
//!
//! They boot the kernel and BusyBox initramfs built by
//! `scripts/build-linux-test-from-source.sh` (`images/linux-src/`, all drivers
//! built in) under OpenSBI v1.7 (`images/linux-test/fw_dynamic.bin` from
//! `scripts/fetch-linux-test.sh`). The initramfs `/init` runs `/run-test` when
//! it exists; each test appends a small cpio archive with its own `/run-test`
//! (and, for vsock and PMU, a static Rust helper built for
//! `riscv64gc-unknown-linux-musl`).
//!
//! Skipped unless `RIVEMU_LINUX_DEVICES_TEST=1`; `RIVEMU_REQUIRE_LINUX_DEVICES=1`
//! turns missing inputs into failures.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rivemu_backends::{BlockBackend, MemoryBlock};
use rivemu_core::size::MIB;
use rivemu_devices::PowerEvent;
use rivemu_machine::{Machine, MachineConfig, VirtioDeviceConfig};

fn images() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images")
}

/// Returns `(opensbi, kernel, rootfs)` or `None` when the test is skipped.
fn inputs(test: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let required = std::env::var_os("RIVEMU_REQUIRE_LINUX_DEVICES").is_some();
    if !required && std::env::var_os("RIVEMU_LINUX_DEVICES_TEST").is_none() {
        eprintln!("skipping {test} (set RIVEMU_LINUX_DEVICES_TEST=1 to run)");
        return None;
    }
    let files = (
        images().join("linux-test/fw_dynamic.bin"),
        images().join("linux-src/Image"),
        images().join("linux-src/rootfs.cpio"),
    );
    for f in [&files.0, &files.1, &files.2] {
        if !f.exists() {
            assert!(!required, "{test}: {} is missing", f.display());
            eprintln!("skipping {test}: {} not found", f.display());
            return None;
        }
    }
    Some(files)
}

/// Appends one `newc` cpio entry.
fn cpio_entry(out: &mut Vec<u8>, ino: u32, name: &str, mode: u32, data: &[u8]) {
    let header = format!(
        "070701{ino:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        0,
        0,
        1,
        0,
        data.len(),
        0,
        0,
        0,
        0,
        name.len() + 1,
        0
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// `rootfs.cpio` followed by a second archive with `/run-test` and extra files.
fn initramfs(rootfs: &Path, run_test: &str, extra: &[(&str, Vec<u8>)], tag: &str) -> PathBuf {
    let mut bytes = std::fs::read(rootfs).unwrap();
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    let mut archive = Vec::new();
    cpio_entry(
        &mut archive,
        900_001,
        "run-test",
        0o100_755,
        run_test.as_bytes(),
    );
    for (i, (name, data)) in extra.iter().enumerate() {
        cpio_entry(&mut archive, 900_002 + i as u32, name, 0o100_755, data);
    }
    cpio_entry(&mut archive, 0, "TRAILER!!!", 0, &[]);
    bytes.extend_from_slice(&archive);
    let path = std::env::temp_dir().join(format!("rivemu-dev-{tag}-{}.cpio", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Builds the static guest helper; `None` (with a message) when the musl
/// target is unavailable and the tests are not required.
fn guest_helper() -> Option<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!("rivemu-dev-helper-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("helper.rs");
    let bin = dir.join("helper");
    std::fs::write(&src, GUEST_HELPER_SRC).unwrap();
    let status = std::process::Command::new("rustc")
        .args([
            "--target",
            "riscv64gc-unknown-linux-musl",
            "-C",
            "linker=rust-lld",
            "-C",
            "target-feature=+crt-static",
            "-O",
            "-o",
        ])
        .arg(&bin)
        .arg(&src)
        .status();
    match status {
        Ok(s) if s.success() => Some(std::fs::read(&bin).unwrap()),
        _ => {
            assert!(
                std::env::var_os("RIVEMU_REQUIRE_LINUX_DEVICES").is_none(),
                "rustc with riscv64gc-unknown-linux-musl is required"
            );
            eprintln!("skipping: riscv64gc-unknown-linux-musl target not installed");
            None
        }
    }
}

const GUEST_HELPER_SRC: &str = r#"
use std::hint::black_box;
extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn connect(fd: i32, addr: *const u8, len: u32) -> i32;
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn syscall(number: i64, ...) -> i64;
}
fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("vsock") => unsafe {
            // AF_VSOCK = 40, SOCK_STREAM = 1; struct sockaddr_vm is 16 bytes.
            let fd = socket(40, 1, 0);
            let mut addr = [0u8; 16];
            addr[0..2].copy_from_slice(&40u16.to_ne_bytes());
            addr[4..8].copy_from_slice(&1234u32.to_ne_bytes());
            addr[8..12].copy_from_slice(&2u32.to_ne_bytes()); // VMADDR_CID_HOST
            let rc = connect(fd, addr.as_ptr(), 16);
            let msg = b"VSOCK_HELLO_FROM_GUEST";
            let n = write(fd, msg.as_ptr(), msg.len());
            close(fd);
            println!("VSOCK fd={fd} connect={rc} write={n}");
        },
        Some("perf") => unsafe {
            // struct perf_event_attr: type 0 (HARDWARE), config 1
            // (INSTRUCTIONS), flags bit 0 (disabled) and bit 6 (exclude_hv).
            let mut attr = [0u8; 128];
            attr[4..8].copy_from_slice(&128u32.to_ne_bytes());
            attr[8..16].copy_from_slice(&1u64.to_ne_bytes());
            attr[40..48].copy_from_slice(&((1u64 << 0) | (1u64 << 6)).to_ne_bytes());
            let fd = syscall(241, attr.as_ptr(), 0i64, -1i64, -1i64, 0u64) as i32; // perf_event_open
            ioctl(fd, 0x2403, 0); // PERF_EVENT_IOC_RESET
            ioctl(fd, 0x2400, 0); // PERF_EVENT_IOC_ENABLE
            let mut acc = 0u64;
            for i in 0..2_000_000u64 {
                acc = black_box(acc.wrapping_add(i));
            }
            ioctl(fd, 0x2401, 0); // PERF_EVENT_IOC_DISABLE
            let mut count = [0u8; 8];
            let n = read(fd, count.as_mut_ptr(), 8);
            println!("PERF fd={fd} read={n} instructions={} acc={acc}", u64::from_ne_bytes(count));
        },
        _ => println!("usage: helper vsock|perf"),
    }
}
"#;

struct Run {
    machine: Machine,
    console: String,
    event: Option<PowerEvent>,
}

/// Runs until a power event, until `stop` matches the console, or until the
/// timeout expires.
fn run(mut machine: Machine, timeout: Duration, stop: Option<&str>) -> Run {
    let start = Instant::now();
    let mut console = String::new();
    let mut event = None;
    while start.elapsed() < timeout {
        if let Some(ev) = machine.run_slice() {
            event = Some(ev);
            break;
        }
        let bytes = machine.uart.take_output();
        if !bytes.is_empty() {
            console.push_str(&String::from_utf8_lossy(&bytes));
            if stop.is_some_and(|s| console.contains(s)) {
                break;
            }
        }
        if machine.all_idle() {
            std::thread::sleep(machine.idle_timeout().min(Duration::from_millis(2)));
        }
    }
    console.push_str(&String::from_utf8_lossy(&machine.uart.take_output()));
    Run {
        machine,
        console,
        event,
    }
}

fn config(opensbi: &Path, kernel: &Path, initrd: &Path, append: &str) -> MachineConfig {
    MachineConfig {
        ram_size: 256 * MIB,
        smp: 1,
        bios: Some(opensbi.to_path_buf()),
        kernel: Some(kernel.to_path_buf()),
        initrd: Some(initrd.to_path_buf()),
        append: Some(format!("console=ttyS0 {append}")),
        ..Default::default()
    }
}

const TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn guest_reboot_resets_the_machine_and_boots_again() {
    let Some((sbi, kernel, rootfs)) = inputs("guest_reboot_resets_the_machine_and_boots_again")
    else {
        return;
    };
    let initrd = initramfs(&rootfs, "#!/bin/sh\n", &[], "reboot");
    let cfg = config(&sbi, &kernel, &initrd, "rivemu.reboot");
    // A guest `reboot -f` must reach the platform reset (sifive_test), which
    // the CLI handles by building a fresh machine from the same config.
    for boot in 1..=2 {
        let r = run(Machine::new(cfg.clone()).unwrap(), TIMEOUT, None);
        assert!(
            r.console.contains("RIVEMU_REBOOT_OK"),
            "boot {boot}: no reboot marker:\n{}",
            r.console
        );
        assert!(
            matches!(r.event, Some(PowerEvent::Reset)),
            "boot {boot}: expected a reset, got {:?}:\n{}",
            r.event,
            r.console
        );
    }
    let _ = std::fs::remove_file(initrd);
}

#[test]
fn nvme_namespace_reads_and_writes_from_linux() {
    let Some((sbi, kernel, rootfs)) = inputs("nvme_namespace_reads_and_writes_from_linux") else {
        return;
    };
    let disk = MemoryBlock::new(1024 * 1024, false);
    disk.write_at(0, b"RIVEMU_NVME_MAGIC_OK").unwrap();
    let script = "#!/bin/sh\n\
echo NVME_SECTORS=$(cat /sys/block/nvme0n1/size)\n\
echo NVME_READ=$(dd if=/dev/nvme0n1 bs=20 count=1 2>/dev/null)\n\
echo -n NVME_GUEST_WRITE | dd of=/dev/nvme0n1 bs=512 seek=1 conv=notrunc,fsync 2>/dev/null\n\
echo NVME_DONE\n\
poweroff -f\n";
    let initrd = initramfs(&rootfs, script, &[], "nvme");
    let mut cfg = config(&sbi, &kernel, &initrd, "");
    cfg.virtio_pci = vec![VirtioDeviceConfig::Nvme {
        backend: disk.clone(),
        serial: "rivemu-nvme0".into(),
    }];
    let r = run(Machine::new(cfg).unwrap(), TIMEOUT, None);
    assert!(
        matches!(r.event, Some(PowerEvent::Poweroff(0))),
        "{}",
        r.console
    );
    assert!(r.console.contains("NVME_SECTORS=2048"), "{}", r.console);
    assert!(
        r.console.contains("NVME_READ=RIVEMU_NVME_MAGIC_OK"),
        "{}",
        r.console
    );
    let mut buf = [0u8; 16];
    disk.read_at(512, &mut buf).unwrap();
    assert_eq!(&buf, b"NVME_GUEST_WRITE");
    let _ = std::fs::remove_file(initrd);
}

#[test]
fn virtio_gpu_scanout_and_input_devices_work_from_linux() {
    let Some((sbi, kernel, rootfs)) =
        inputs("virtio_gpu_scanout_and_input_devices_work_from_linux")
    else {
        return;
    };
    // Fill the fbdev with 0x5a bytes, give the deferred-I/O flush time to
    // reach the virtio-gpu scanout, then power off.
    let script = "#!/bin/sh\n\
ls /dev/fb0 /dev/dri/card0 >/dev/null && echo GPU_NODES_OK\n\
echo FB_SIZE=$(cat /sys/class/graphics/fb0/virtual_size)\n\
cat /sys/class/input/input*/name | grep -i virtio && echo INPUT_OK\n\
ls /dev/input/event0 >/dev/null && echo EVDEV_OK\n\
yes Z | tr -d '\\n' | head -c 1048576 > /dev/fb0 2>/dev/null\n\
sleep 1\n\
echo GPU_WRITE_DONE\n\
poweroff -f\n";
    let initrd = initramfs(&rootfs, script, &[], "gpu");
    let mut cfg = config(&sbi, &kernel, &initrd, "");
    cfg.virtio = vec![
        VirtioDeviceConfig::Gpu {
            width: 640,
            height: 480,
        },
        VirtioDeviceConfig::Input,
    ];
    let r = run(Machine::new(cfg).unwrap(), TIMEOUT, None);
    assert!(
        matches!(r.event, Some(PowerEvent::Poweroff(0))),
        "{}",
        r.console
    );
    for marker in [
        "GPU_NODES_OK",
        "FB_SIZE=640,480",
        "INPUT_OK",
        "EVDEV_OK",
        "GPU_WRITE_DONE",
    ] {
        assert!(
            r.console.contains(marker),
            "missing {marker}:\n{}",
            r.console
        );
    }
    let (w, h, rgba, _) = r.machine.framebuffer.snapshot_rgba();
    assert_eq!((w, h), (640, 480));
    let lit = rgba
        .chunks(4)
        .filter(|px| px[..3].iter().any(|&c| c != 0))
        .count();
    assert!(lit > 10_000, "scanout shows only {lit} non-black pixels");
    let _ = std::fs::remove_file(initrd);
}

#[test]
fn virtio_vsock_guest_connects_to_host_cid() {
    let Some((sbi, kernel, rootfs)) = inputs("virtio_vsock_guest_connects_to_host_cid") else {
        return;
    };
    let Some(helper) = guest_helper() else {
        return;
    };
    let script = "#!/bin/sh\n/helper vsock\npoweroff -f\n";
    let initrd = initramfs(&rootfs, script, &[("helper", helper)], "vsock");
    let mut cfg = config(&sbi, &kernel, &initrd, "");
    cfg.virtio = vec![VirtioDeviceConfig::Vsock { guest_cid: 3 }];
    let r = run(Machine::new(cfg).unwrap(), TIMEOUT, None);
    assert!(
        matches!(r.event, Some(PowerEvent::Poweroff(0))),
        "{}",
        r.console
    );
    assert!(r.console.contains("connect=0 write=22"), "{}", r.console);
    let sent: Vec<u8> = r
        .machine
        .vsock
        .take_tx_payloads()
        .into_iter()
        .flat_map(|(_, port, payload)| {
            assert_eq!(port, 1234);
            payload
        })
        .collect();
    assert_eq!(sent, b"VSOCK_HELLO_FROM_GUEST");
    let _ = std::fs::remove_file(initrd);
}

#[test]
fn sbi_pmu_counts_guest_instructions() {
    let Some((sbi, kernel, rootfs)) = inputs("sbi_pmu_counts_guest_instructions") else {
        return;
    };
    let Some(helper) = guest_helper() else {
        return;
    };
    let script = "#!/bin/sh\n\
cat /sys/bus/event_source/devices/cpu/type >/dev/null && echo PMU_SYSFS_OK\n\
/helper perf\n\
poweroff -f\n";
    let initrd = initramfs(&rootfs, script, &[("helper", helper)], "perf");
    let cfg = config(&sbi, &kernel, &initrd, "");
    let r = run(Machine::new(cfg).unwrap(), TIMEOUT, None);
    assert!(
        matches!(r.event, Some(PowerEvent::Poweroff(0))),
        "{}",
        r.console
    );
    assert!(r.console.contains("PMU_SYSFS_OK"), "{}", r.console);
    assert!(
        r.console
            .contains("riscv-pmu-sbi: SBI PMU extension is available"),
        "{}",
        r.console
    );
    let count: u64 = r
        .console
        .split("instructions=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no PERF line:\n{}", r.console));
    // The loop retires at least 2M additions plus loop overhead.
    assert!(count >= 2_000_000, "only {count} instructions counted");
    let _ = std::fs::remove_file(initrd);
}

#[test]
fn snapshot_v2_restores_pci_nvme_into_a_fresh_machine() {
    let Some((sbi, kernel, rootfs)) = inputs("snapshot_v2_restores_pci_nvme_into_a_fresh_machine")
    else {
        return;
    };
    // The guest writes to NVMe, waits on the console, and after the host has
    // saved and restored the VM it reads the data back, writes again and
    // uses the virtio-pci RNG.
    let script = "#!/bin/sh\n\
echo -n BEFORE_SNAPSHOT | dd of=/dev/nvme0n1 bs=512 seek=2 conv=notrunc,fsync 2>/dev/null\n\
echo READY_FOR_SNAPSHOT\n\
read line\n\
echo AFTER_RESTORE_READ=$(dd if=/dev/nvme0n1 bs=15 skip=0 count=1 iflag=skip_bytes 2>/dev/null; dd if=/dev/nvme0n1 bs=512 skip=2 count=1 2>/dev/null | head -c 15)\n\
echo -n AFTER_RESTORE | dd of=/dev/nvme0n1 bs=512 seek=3 conv=notrunc,fsync 2>/dev/null\n\
echo RNG_BYTES=$(head -c 16 /dev/hwrng | wc -c)\n\
echo SNAPSHOT_DONE\n\
poweroff -f\n";
    let initrd = initramfs(&rootfs, script, &[], "snap");
    let disk = MemoryBlock::new(1024 * 1024, false);
    let mut cfg = config(&sbi, &kernel, &initrd, "");
    cfg.virtio_pci = vec![
        VirtioDeviceConfig::Nvme {
            backend: disk.clone(),
            serial: "rivemu-nvme0".into(),
        },
        VirtioDeviceConfig::Rng,
    ];
    let first = run(
        Machine::new(cfg.clone()).unwrap(),
        TIMEOUT,
        Some("READY_FOR_SNAPSHOT"),
    );
    assert!(first.event.is_none(), "{}", first.console);
    assert!(
        first.console.contains("READY_FOR_SNAPSHOT"),
        "{}",
        first.console
    );
    let snap = std::env::temp_dir().join(format!("rivemu-dev-snap-{}.bin", std::process::id()));
    first.machine.save_snapshot(&snap).unwrap();
    drop(first);

    // Restore into a fresh machine. The block backend is shared, because the
    // snapshot format stores device and queue state but not disk contents.
    let mut restored = Machine::new(cfg).unwrap();
    restored.load_snapshot(&snap).unwrap();
    restored.uart.push_input(b"go\n");
    let r = run(restored, TIMEOUT, None);
    assert!(
        matches!(r.event, Some(PowerEvent::Poweroff(0))),
        "{}",
        r.console
    );
    assert!(r.console.contains("BEFORE_SNAPSHOT"), "{}", r.console);
    assert!(r.console.contains("RNG_BYTES=16"), "{}", r.console);
    assert!(r.console.contains("SNAPSHOT_DONE"), "{}", r.console);
    let mut buf = [0u8; 13];
    disk.read_at(3 * 512, &mut buf).unwrap();
    assert_eq!(&buf, b"AFTER_RESTORE");
    let _ = std::fs::remove_file(snap);
    let _ = std::fs::remove_file(initrd);
}
