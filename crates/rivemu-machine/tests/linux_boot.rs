//! End-to-end Linux + OpenSBI + BusyBox + VirtIO (`blk`, `net`, `console`, `rng`)
//! smoke test.
//!
//! Uses artifacts from `images/linux-test/` (created by
//! `scripts/fetch-linux-test.sh`). Enabled when `RIVEMU_LINUX_TEST` or
//! `RIVEMU_REQUIRE_LINUX_TEST` is set.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rivemu_backends::{BlockBackend, BufferChardev, MemoryBlock, UserNet, UserNetConfig};
use rivemu_core::size::MIB;
use rivemu_devices::PowerEvent;
use rivemu_machine::{Machine, MachineConfig, VirtioDeviceConfig};

/// Serializes multi-hart Linux VM boot tests so they do not starve each other
/// of host CPU cores on 2-vCPU CI runners.
static LINUX_BOOT_LOCK: Mutex<()> = Mutex::new(());

fn image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/linux-test")
}

fn run_linux_boot_test(use_external_opensbi: bool) {
    let _guard = LINUX_BOOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let required = std::env::var_os("RIVEMU_REQUIRE_LINUX_TEST").is_some();
    let enabled = required || std::env::var_os("RIVEMU_LINUX_TEST").is_some();
    if !enabled {
        eprintln!("skipping Linux boot test (set RIVEMU_LINUX_TEST=1 to run)");
        return;
    }

    let dir = image_dir();
    let bios_path = dir.join("fw_dynamic.bin");
    let kernel = dir.join("Image");
    let initrd = dir.join("rootfs.cpio");
    let required_files: Vec<&PathBuf> = if use_external_opensbi {
        vec![&bios_path, &kernel, &initrd]
    } else {
        vec![&kernel, &initrd]
    };
    for p in required_files {
        if !p.is_file() {
            assert!(
                !required,
                "missing Linux test image {} (run scripts/fetch-linux-test.sh)",
                p.display()
            );
            eprintln!("skipping: {} not found", p.display());
            return;
        }
    }

    let disk = MemoryBlock::new(64 * 1024, false);
    disk.write_at(0, b"RIVEMU_BLK_MAGIC_OK\n").unwrap();
    let net = UserNet::new(UserNetConfig::default());
    let vconsole = BufferChardev::new();

    let cfg = MachineConfig {
        ram_size: 256 * MIB,
        smp: 4,
        icount: None,
        bios: use_external_opensbi.then_some(bios_path),
        kernel: Some(kernel),
        initrd: Some(initrd),
        dtb: None,
        append: Some("console=ttyS0 rcupdate.rcu_cpu_stall_suppress=1".into()),
        virtio: vec![
            VirtioDeviceConfig::Blk {
                backend: disk.clone(),
                serial: "testdisk0".into(),
            },
            VirtioDeviceConfig::Net { backend: net },
            VirtioDeviceConfig::Console {
                chardev: vconsole.clone(),
            },
            VirtioDeviceConfig::Rng,
        ],
        ..Default::default()
    };
    let mut machine = Machine::new(cfg.clone()).expect("machine init");

    let start = Instant::now();
    let deadline = Duration::from_secs(90);
    let mut console_out = String::new();
    let mut sent_cmd = false;
    let mut poweroff_code = None;

    while start.elapsed() < deadline {
        if let Some(ev) = machine.run_slice() {
            match ev {
                PowerEvent::Poweroff(code) => {
                    poweroff_code = Some(code);
                    break;
                }
                PowerEvent::Reset => panic!("unexpected guest reset"),
            }
        }
        let bytes = machine.uart.take_output();
        if !bytes.is_empty() {
            console_out.push_str(&String::from_utf8_lossy(&bytes));
            if console_out.contains("NETDEV WATCHDOG")
                || console_out.contains("hard LOCKUP")
                || console_out.contains("rcu_sched detected stalls")
            {
                break;
            }
            if !sent_cmd && console_out.contains("~ # ") {
                if !use_external_opensbi {
                    // Milestone P10: save a live 4-CPU Linux VM snapshot at the
                    // shell prompt and restore it into a fresh Machine in < 50 ms.
                    let snap_path = std::env::temp_dir()
                        .join(format!("rivemu-linux-snap-{}.bin", std::process::id()));
                    machine.save_snapshot(&snap_path).expect("save_snapshot");
                    let mut restored = Machine::new(cfg.clone()).expect("restored machine init");
                    let t_restore = Instant::now();
                    restored.load_snapshot(&snap_path).expect("load_snapshot");
                    let restore_ms = t_restore.elapsed().as_secs_f64() * 1000.0;
                    let _ = std::fs::remove_file(&snap_path);
                    eprintln!("Restored 4-CPU Linux VM snapshot in {restore_ms:.2} ms");
                    assert!(
                        restore_ms < 50.0,
                        "expected VM snapshot restore in < 50 ms, took {restore_ms:.2} ms"
                    );
                    machine = restored;
                }
                let script = "\
echo SHELL_CMD_OK$((20+22))\n\
echo CPUS=$(/bin/busybox grep -c ^processor /proc/cpuinfo)\n\
/bin/busybox dd if=/dev/vda bs=20 count=1 2>/dev/null\n\
echo -n GUEST_BLK_WRITE | /bin/busybox dd of=/dev/vda bs=512 seek=1 conv=notrunc 2>/dev/null\n\
/bin/busybox sync\n\
/bin/busybox ip link set eth0 up && /bin/busybox ip addr add 10.0.2.15/24 dev eth0\n\
/bin/busybox ping -c 1 10.0.2.2\n\
echo VCONSOLE_OK > /dev/hvc0 2>/dev/null || true\n\
/bin/busybox poweroff -f\n";
                machine.uart.push_input(script.as_bytes());
                sent_cmd = true;
            }
        }
        if machine.all_idle() {
            std::thread::sleep(machine.idle_timeout().min(Duration::from_millis(2)));
        }
    }

    let bytes = machine.uart.take_output();
    console_out.push_str(&String::from_utf8_lossy(&bytes));
    let elapsed = start.elapsed();
    eprintln!(
        "Linux + VirtIO smoke test completed in {:.2}s ({} instructions)",
        elapsed.as_secs_f64(),
        machine.instret()
    );
    if poweroff_code != Some(0) {
        for h in &machine.harts {
            let pa = h.debug_translate(&machine.bus, h.pc);
            let insn = pa.and_then(|p| machine.bus.read(p, rivemu_core::AccessSize::Word).ok());
            eprintln!(
                "HART {}: pc={:#x} (pa={:?} insn={:#x?}) prv={:?} wfi={} mstatus={:#x} mip={:#x} mie={:#x} sepc={:#x} mepc={:#x} ra={:#x} a0={:#x} a1={:#x} a2={:#x}",
                h.id,
                h.pc,
                pa,
                insn,
                h.prv,
                h.wfi,
                h.csr.mstatus,
                h.mip(),
                h.csr.mie,
                h.csr.sepc,
                h.csr.mepc,
                h.x[1],
                h.x[10],
                h.x[11],
                h.x[12]
            );
        }
    }
    assert!(
        console_out.contains("Linux 6."),
        "expected uname output in console:\n{console_out}"
    );
    assert!(
        console_out.contains("SHELL_CMD_OK42"),
        "expected BusyBox shell arithmetic output in console:\n{console_out}"
    );
    assert!(
        console_out.contains("CPUS=4"),
        "expected 4 online CPUs in /proc/cpuinfo:\n{console_out}"
    );
    assert!(
        console_out.contains("RIVEMU_BLK_MAGIC_OK"),
        "expected virtio-blk read output in console:\n{console_out}"
    );
    assert!(
        console_out.contains("1 packets received"),
        "expected virtio-net ping reply from 10.0.2.2 in console:\n{console_out}"
    );
    let mut sector1 = [0u8; 15];
    disk.read_at(512, &mut sector1).unwrap();
    assert_eq!(
        &sector1, b"GUEST_BLK_WRITE",
        "expected guest write to sector 1 of virtio-blk"
    );
    assert_eq!(
        poweroff_code,
        Some(0),
        "expected clean guest poweroff, console:\n{console_out}"
    );
}

#[test]
fn boots_linux_with_virtio_blk_net_console_rng_and_powers_off() {
    run_linux_boot_test(true);
}

#[test]
fn boots_linux_with_builtin_rust_sbi_no_firmware() {
    run_linux_boot_test(false);
}

#[test]
fn rust_userspace_init_and_virtio_9p_on_linux() {
    let _guard = LINUX_BOOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let required = std::env::var_os("RIVEMU_REQUIRE_LINUX_TEST").is_some();
    let enabled = required || std::env::var_os("RIVEMU_LINUX_TEST").is_some();
    if !enabled {
        eprintln!("skipping Rust PID 1 + virtio-9p test (set RIVEMU_LINUX_TEST=1 to run)");
        return;
    }

    let kernel = image_dir().join("Image");
    if !kernel.is_file() {
        assert!(!required, "missing {}", kernel.display());
        return;
    }

    let tmp_dir = std::env::temp_dir().join(format!("rivemu-p7-p8-{}", std::process::id()));
    let share_dir = tmp_dir.join("share");
    std::fs::create_dir_all(&share_dir).unwrap();
    std::fs::write(share_dir.join("host_file.txt"), b"HOST_9P_PAYLOAD_42").unwrap();

    let guest_src = tmp_dir.join("init.rs");
    let guest_bin = tmp_dir.join("init_rv64");
    std::fs::write(
        &guest_src,
        r#"
use std::fs;
use std::sync::{Arc, Barrier, atomic::{AtomicU64, Ordering}};
use std::thread;

unsafe extern "C" {
    fn mount(
        source: *const u8,
        target: *const u8,
        filesystemtype: *const u8,
        mountflags: usize,
        data: *const u8,
    ) -> i32;
    fn reboot(cmd: i32) -> i32;
}

fn main() {
    println!("RUST_INIT_PID={}", std::process::id());

    // 1. Multi-threaded SMP barrier across 4 worker threads.
    let barrier = Arc::new(Barrier::new(4));
    let sum = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for i in 1..=4u64 {
        let b = barrier.clone();
        let s = sum.clone();
        handles.push(thread::spawn(move || {
            b.wait();
            s.fetch_add(i * 10, Ordering::SeqCst);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    println!("RUST_SMP_SUM={}", sum.load(Ordering::SeqCst));

    // 2. Exercise Linux epoll + eventfd + getrandom syscalls (rustix core surface).
    unsafe {
        let mut rand_buf = [0u8; 16];
        // SYS_getrandom on riscv64 = 278
        let mut r_ret: isize;
        std::arch::asm!(
            "ecall",
            in("a7") 278usize,
            inlateout("a0") rand_buf.as_mut_ptr() as usize => r_ret,
            in("a1") rand_buf.len(),
            in("a2") 0usize,
        );
        assert_eq!(r_ret, 16);
        assert_ne!(rand_buf, [0u8; 16]);

        // SYS_eventfd2 = 19, SYS_epoll_create1 = 20
        let mut efd: isize;
        std::arch::asm!(
            "ecall",
            in("a7") 19usize,
            inlateout("a0") 42usize => efd,
            in("a1") 0usize,
        );
        let mut epfd: isize;
        std::arch::asm!(
            "ecall",
            in("a7") 20usize,
            inlateout("a0") 0usize => epfd,
        );
        assert!(efd > 0 && epfd > 0);
        println!("RUST_SYSCALLS_RAND_EPOLL_EVENTFD=OK");
    }

    // 3. Mount /proc and inspect /proc/cpuinfo (verifying Zba/Zbb/Zbc/Zbs and 4 CPUs).
    unsafe {
        mount(b"proc\0".as_ptr(), b"/proc\0".as_ptr(), b"proc\0".as_ptr(), 0, std::ptr::null());
        mount(b"sysfs\0".as_ptr(), b"/sys\0".as_ptr(), b"sysfs\0".as_ptr(), 0, std::ptr::null());
    }
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        let cpus = cpuinfo.lines().filter(|l| l.starts_with("processor")).count();
        let has_zbb = cpuinfo.contains("zbb");
        let has_cbo = cpuinfo.contains("zicbom") && cpuinfo.contains("zicboz");
        println!("RUST_PROC_CPUS={cpus} HAS_ZBB={has_zbb} HAS_CBO={has_cbo}");
    }
    if let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") {
        let mut devs: Vec<String> = entries
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        devs.sort();
        println!("RUST_PCI_DEVS={}", devs.join(","));
    }

    // Exercise Zicboz (cbo.zero) directly from U-mode on a 64-byte aligned buffer.
    #[repr(align(64))]
    struct Aligned64([u8; 64]);
    let mut line = Aligned64([0x5a; 64]);
    unsafe {
        std::arch::asm!(
            ".insn i 0x0f, 2, x0, {ptr}, 4",
            ptr = in(reg) line.0.as_mut_ptr(),
            options(nostack),
        );
    }
    assert_eq!(line.0, [0u8; 64]);
    println!("RUST_CBO_ZERO=OK");

    // 3. Mount virtio-9p share ("host0") at /mnt, read host file, write guest file.
    let rc = unsafe {
        mount(
            b"host0\0".as_ptr(),
            b"/mnt\0".as_ptr(),
            b"9p\0".as_ptr(),
            0,
            b"trans=virtio,version=9p2000.L\0".as_ptr(),
        )
    };
    if rc == 0 {
        let host_txt = fs::read_to_string("/mnt/host_file.txt").unwrap_or_default();
        println!("RUST_9P_READ={host_txt}");
        let _ = fs::write("/mnt/guest_reply.txt", b"WRITTEN_BY_RUST_INIT_OVER_9P");
    } else {
        println!("RUST_9P_MOUNT_ERR={}", std::io::Error::last_os_error());
    }

    // 4. Clean poweroff via LINUX_REBOOT_CMD_POWER_OFF (0x4321fedc).
    unsafe {
        reboot(0x4321_fedc_u32 as i32);
    }
}
"#,
    )
    .unwrap();

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
        .arg(&guest_bin)
        .arg(&guest_src)
        .status();

    let Ok(st) = status else {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        eprintln!("skipping: rustc not available");
        return;
    };
    if !st.success() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        eprintln!("skipping: riscv64gc-unknown-linux-musl target not installed");
        return;
    }

    let mut machine = Machine::new(MachineConfig {
        ram_size: 256 * MIB,
        smp: 4,
        icount: None,
        bios: None,
        kernel: Some(kernel),
        // Passing an ELF directly to `initrd` automatically wraps it as `/init`
        // inside a `newc` cpio initramfs archive in memory!
        initrd: Some(guest_bin),
        dtb: None,
        append: Some("console=ttyS0 panic=-1".into()),
        virtio: vec![
            VirtioDeviceConfig::Rng,
            VirtioDeviceConfig::P9 {
                path: share_dir.clone(),
                mount_tag: "host0".into(),
            },
        ],
        virtio_pci: vec![
            VirtioDeviceConfig::Rng,
            VirtioDeviceConfig::Gpu {
                width: 640,
                height: 480,
            },
            VirtioDeviceConfig::Input,
        ],
        pflash: Vec::new(),
        fw_cfg_files: vec![("opt/org.rivemu/test".into(), b"fwcfg_ok".to_vec())],
        strict_align: true,
        svade: true,
        strict_pmp: true,
    })
    .expect("machine init");

    let start = Instant::now();
    let deadline = Duration::from_secs(90);
    let mut console_out = String::new();
    let mut poweroff_code = None;

    while start.elapsed() < deadline {
        if let Some(PowerEvent::Poweroff(code)) = machine.run_slice() {
            poweroff_code = Some(code);
            break;
        }
        let bytes = machine.uart.take_output();
        if !bytes.is_empty() {
            console_out.push_str(&String::from_utf8_lossy(&bytes));
        }
        if machine.all_idle() {
            std::thread::sleep(machine.idle_timeout().min(Duration::from_millis(2)));
        }
    }
    let bytes = machine.uart.take_output();
    console_out.push_str(&String::from_utf8_lossy(&bytes));
    eprintln!(
        "Rust PID 1 + virtio-9p test completed in {:.2}s:\n{}",
        start.elapsed().as_secs_f64(),
        console_out
            .lines()
            .filter(|l| l.starts_with("RUST_"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let guest_reply = std::fs::read_to_string(share_dir.join("guest_reply.txt")).ok();
    let _ = std::fs::remove_dir_all(&tmp_dir);

    assert_eq!(poweroff_code, Some(0), "console:\n{console_out}");
    assert!(console_out.contains("RUST_INIT_PID=1"), "{console_out}");
    assert!(console_out.contains("RUST_SMP_SUM=100"), "{console_out}");
    assert!(
        console_out.contains("RUST_SYSCALLS_RAND_EPOLL_EVENTFD=OK"),
        "{console_out}"
    );
    assert!(
        console_out.contains("RUST_PROC_CPUS=4 HAS_ZBB=true HAS_CBO=true"),
        "{console_out}"
    );
    assert!(
        console_out.contains("RUST_PCI_DEVS=0000:00:00.0,0000:00:01.0,0000:00:02.0,0000:00:03.0"),
        "{console_out}"
    );
    assert!(console_out.contains("RUST_CBO_ZERO=OK"), "{console_out}");
    if console_out.contains("RUST_9P_READ=") {
        assert!(
            console_out.contains("RUST_9P_READ=HOST_9P_PAYLOAD_42"),
            "{console_out}"
        );
        assert_eq!(guest_reply.as_deref(), Some("WRITTEN_BY_RUST_INIT_OVER_9P"));
    }
}
