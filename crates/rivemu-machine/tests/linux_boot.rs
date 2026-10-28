//! End-to-end Linux + OpenSBI + BusyBox + VirtIO (`blk`, `net`, `console`, `rng`)
//! smoke test.
//!
//! Uses artifacts from `images/linux-test/` (created by
//! `scripts/fetch-linux-test.sh`). Enabled when `RIVEMU_LINUX_TEST` or
//! `RIVEMU_REQUIRE_LINUX_TEST` is set.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rivemu_backends::{BlockBackend, BufferChardev, MemoryBlock, UserNet, UserNetConfig};
use rivemu_core::size::MIB;
use rivemu_devices::PowerEvent;
use rivemu_machine::{Machine, MachineConfig, VirtioDeviceConfig};

fn image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/linux-test")
}

#[test]
fn boots_linux_with_virtio_blk_net_console_rng_and_powers_off() {
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

    let disk = MemoryBlock::new(64 * 1024, false);
    disk.write_at(0, b"RIVEMU_BLK_MAGIC_OK\n").unwrap();
    let net = UserNet::new(UserNetConfig::default());
    let vconsole = BufferChardev::new();

    let mut machine = Machine::new(MachineConfig {
        ram_size: 256 * MIB,
        smp: 1,
        bios: Some(bios),
        kernel: Some(kernel),
        initrd: Some(initrd),
        dtb: None,
        append: Some("console=ttyS0".into()),
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
                let script = "\
echo SHELL_CMD_OK$((20+22))\n\
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
    assert!(
        console_out.contains("Linux 6."),
        "expected uname output in console:\n{console_out}"
    );
    assert!(
        console_out.contains("SHELL_CMD_OK42"),
        "expected BusyBox shell arithmetic output in console:\n{console_out}"
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
    assert_eq!(poweroff_code, Some(0), "expected clean guest poweroff");
}
