//! `rivemu-system-riscv64`: entry point.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use rivemu_cli::{Action, CliError, DEVICE_DRIVERS, LOG_ITEMS, PROG};
use rivemu_machine::Machine;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{PROG}: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let args = std::env::args_os()
        .skip(1)
        .map(|a| a.into_string().map_err(|_| CliError::NotUtf8))
        .collect::<Result<Vec<_>, _>>()?;
    let opts = rivemu_cli::parse_args(args)?;

    match opts.action {
        Action::Run => {}
        Action::Help => {
            print!("{}", rivemu_cli::help_text());
            return Ok(ExitCode::SUCCESS);
        }
        Action::Version => {
            println!("{PROG} version {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Action::MachineHelp => {
            println!("Supported machines are:");
            println!("virt                 RISC-V VirtIO board (default)");
            return Ok(ExitCode::SUCCESS);
        }
        Action::CpuHelp => {
            println!("Available CPUs:");
            println!("  max");
            println!("  rv64");
            return Ok(ExitCode::SUCCESS);
        }
        Action::LogHelp => {
            println!("Log items (comma separated):");
            for (name, desc) in LOG_ITEMS {
                println!("{name:<15} {desc}");
            }
            return Ok(ExitCode::SUCCESS);
        }
        Action::DeviceHelp => {
            println!("Supported devices:");
            for (name, desc) in DEVICE_DRIVERS {
                println!("name \"{name}\", desc \"{desc}\"");
            }
            return Ok(ExitCode::SUCCESS);
        }
    }

    let config = rivemu_cli::machine_config(&opts).map_err(|e| anyhow!(e))?;
    let machine = Machine::new(config).context("failed to initialise machine")?;

    let boot = &machine.boot;
    eprintln!(
        "{PROG}: virt machine: {} MiB RAM, {} hart(s)",
        machine.ram.len() >> 20,
        machine.config.smp
    );
    for (what, img) in [("firmware", &boot.firmware), ("kernel", &boot.kernel)] {
        if let Some(img) = img {
            eprintln!(
                "{PROG}: {what:<8} {:#x}..{:#x} ({}), entry {:#x}",
                img.start,
                img.end,
                if img.elf { "ELF" } else { "raw" },
                img.entry
            );
        }
    }
    if let Some((start, end)) = boot.initrd {
        eprintln!("{PROG}: initrd   {start:#x}..{end:#x}");
    }
    eprintln!("{PROG}: boot entry {:#x}", boot.entry);
    eprintln!("{PROG}: CPU emulation is not implemented yet (milestone P1); exiting");
    Ok(ExitCode::SUCCESS)
}
