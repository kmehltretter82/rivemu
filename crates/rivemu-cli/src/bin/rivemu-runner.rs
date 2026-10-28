//! `rivemu-runner`: Cargo target runner (`CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_MUSL_RUNNER`)
//! that executes any statically linked `riscv64` Linux ELF binary as PID 1 (`/init`)
//! inside `rivemu` using the built-in pure-Rust SBI handler.

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rivemu_core::size::MIB;
use rivemu_devices::PowerEvent;
use rivemu_machine::{Machine, MachineConfig, VirtioDeviceConfig};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("rivemu-runner: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn find_kernel() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("RIVEMU_KERNEL") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        bail!("RIVEMU_KERNEL '{}' does not exist", path.display());
    }
    let candidates = [
        PathBuf::from("images/linux-test/Image"),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/linux-test/Image"),
    ];
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }
    Err(anyhow!(
        "unable to find RISC-V Linux kernel 'Image' \
         (set RIVEMU_KERNEL=/path/to/Image or run scripts/fetch-linux-test.sh)"
    ))
}

fn run() -> Result<i32> {
    let mut args = std::env::args().skip(1);
    let Some(bin_arg) = args.next() else {
        eprintln!("usage: rivemu-runner <riscv64-linux-elf> [guest-args...]");
        return Ok(2);
    };
    if bin_arg == "-h" || bin_arg == "--help" {
        println!("usage: rivemu-runner <riscv64-linux-elf> [guest-args...]");
        return Ok(0);
    }
    let guest_bin = PathBuf::from(&bin_arg);
    if !guest_bin.is_file() {
        bail!("guest binary '{}' does not exist", guest_bin.display());
    }
    let extra_args: Vec<String> = args.collect();
    let kernel = find_kernel()?;
    let smp: u32 = std::env::var("RIVEMU_SMP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);

    let mut append = String::from("console=ttyS0 quiet panic=-1");
    if !extra_args.is_empty() {
        append.push_str(" -- ");
        append.push_str(&extra_args.join(" "));
    }

    let mut virtio = vec![VirtioDeviceConfig::Rng];
    if let Some(share) = std::env::var_os("RIVEMU_SHARE_DIR") {
        virtio.push(VirtioDeviceConfig::P9 {
            path: PathBuf::from(share),
            mount_tag: "host0".into(),
        });
    }

    let strict = std::env::var_os("RIVEMU_STRICT").is_some();
    let mut machine = Machine::new(MachineConfig {
        ram_size: 256 * MIB,
        smp,
        bios: None,
        kernel: Some(kernel),
        initrd: Some(guest_bin),
        dtb: None,
        append: Some(append),
        virtio,
        icount: None,
        strict_align: strict,
        svade: strict,
        strict_pmp: strict,
    })
    .context("failed to initialise rivemu machine")?;

    let mut stdout = std::io::stdout();
    loop {
        if let Some(ev) = machine.run_slice() {
            let out = machine.uart.take_output();
            if !out.is_empty() {
                stdout.write_all(&out)?;
                stdout.flush()?;
            }
            return Ok(match ev {
                PowerEvent::Poweroff(code) => code,
                PowerEvent::Reset => 0,
            });
        }
        let out = machine.uart.take_output();
        if !out.is_empty() {
            stdout.write_all(&out)?;
            stdout.flush()?;
        }
        if machine.all_idle() {
            std::thread::sleep(machine.idle_timeout().min(Duration::from_millis(2)));
        }
    }
}
