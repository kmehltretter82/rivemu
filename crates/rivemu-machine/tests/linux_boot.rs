//! End-to-end Linux + OpenSBI + BusyBox shell smoke test.
//!
//! Fetches artifacts from `images/linux-test/` (created by
//! `scripts/fetch-linux-test.sh`). Enabled when `RIVEMU_LINUX_TEST` or
//! `RIVEMU_REQUIRE_LINUX_TEST` is set.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rivemu_core::size::MIB;
use rivemu_devices::PowerEvent;
use rivemu_machine::{Machine, MachineConfig};

fn image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/linux-test")
}

#[test]
fn boots_linux_to_busybox_shell_and_powers_off() {
    let required = std::env::var_os("RIVEMU_REQUIRE_LINUX_TEST").is_some();
    let enabled = required || std::env::var_os("RIVEMU_LINUX_TEST").is_some();
    if !enabled {
        eprintln!("skipping Linux boot test (set RIVEMU_LINUX_TEST=1 to run)");
        return;
    }

    let dir = image_dir();
    let bios = dir.join("fw_dynamic.bin");
    let kernel = dir.join("Image");
    let initrd = dir.join("rootfs.cpio");
    for p in [&bios, &kernel, &initrd] {
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

    let mut machine = Machine::new(MachineConfig {
        ram_size: 256 * MIB,
        smp: 1,
        bios: Some(bios),
        kernel: Some(kernel),
        initrd: Some(initrd),
        dtb: None,
        append: Some("console=ttyS0".into()),
    })
    .expect("machine init");

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
            if !sent_cmd && console_out.contains("=== rivemu BusyBox initramfs ===") {
                machine
                    .uart
                    .push_input(b"echo SHELL_CMD_OK$((20+22))\n/bin/busybox poweroff -f\n");
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
        "Linux boot + BusyBox shell + poweroff completed in {:.2}s ({} instructions)",
        elapsed.as_secs_f64(),
        machine.instret()
    );
    assert!(
        console_out.contains("Linux 6."),
        "expected uname output in console:\n{console_out}"
    );
    assert!(
        console_out.contains("SHELL_CMD_OK42"),
        "expected BusyBox shell arithmetic output in console:\n{console_out}"
    );
    assert_eq!(poweroff_code, Some(0), "expected clean guest poweroff");
}
