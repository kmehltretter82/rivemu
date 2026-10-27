//! `rivemu-system-riscv64`: entry point.

#![forbid(unsafe_code)]

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rivemu_cli::console::{Console, ConsoleEvent, RawModeGuard};
use rivemu_cli::{Action, CliError, DEVICE_DRIVERS, LOG_ITEMS, PROG};
use rivemu_devices::PowerEvent;
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

    for (used, what) in [
        (!opts.drives.is_empty(), "-drive"),
        (!opts.devices.is_empty(), "-device"),
        (!opts.netdevs.is_empty(), "-netdev"),
        (!opts.chardevs.is_empty(), "-chardev"),
        (opts.gdb.is_some(), "-gdb/-s"),
        (opts.start_paused, "-S"),
        (opts.monitor.is_some(), "-monitor"),
        (!opts.qmp.is_empty(), "-qmp"),
        (opts.icount.is_some(), "-icount"),
    ] {
        if used {
            eprintln!("{PROG}: warning: {what} is not implemented yet; ignoring");
        }
    }

    let serial = SerialBackend::parse(opts.serials.first().map(String::as_str))?;
    let config = rivemu_cli::machine_config(&opts).map_err(|e| anyhow!(e))?;
    let mut machine = Machine::new(config.clone()).context("failed to initialise machine")?;

    let _raw = match serial {
        SerialBackend::MonStdio => Some(RawModeGuard::enable(true)),
        SerialBackend::Stdio => Some(RawModeGuard::enable(false)),
        _ => None,
    };
    let mut console = matches!(serial, SerialBackend::Stdio | SerialBackend::MonStdio)
        .then(|| Console::spawn(serial == SerialBackend::MonStdio));
    let mut sink: Box<dyn Write> = match &serial {
        SerialBackend::Stdio | SerialBackend::MonStdio => Box::new(std::io::stdout()),
        SerialBackend::File(p) => Box::new(
            std::fs::File::create(p).with_context(|| format!("cannot create {}", p.display()))?,
        ),
        SerialBackend::Null => Box::new(std::io::sink()),
    };

    let started = Instant::now();
    let exit = loop {
        match machine.run_slice() {
            Some(PowerEvent::Poweroff(code)) => break code,
            Some(PowerEvent::Reset) if opts.no_reboot => break 0,
            Some(PowerEvent::Reset) => {
                let out = machine.uart.take_output();
                sink.write_all(&out)?;
                machine = Machine::new(config.clone()).context("failed to reset machine")?;
            }
            None => {}
        }
        let out = machine.uart.take_output();
        if !out.is_empty() {
            sink.write_all(&out)?;
            sink.flush()?;
        }
        let timeout = if machine.all_idle() {
            machine.idle_timeout()
        } else {
            Duration::ZERO
        };
        match &mut console {
            Some(c) => {
                let mut quit = false;
                for ev in c.poll(timeout) {
                    match ev {
                        ConsoleEvent::Input(d) => machine.uart.push_input(&d),
                        ConsoleEvent::Quit => quit = true,
                    }
                }
                if quit {
                    break 0;
                }
            }
            None if !timeout.is_zero() => std::thread::sleep(timeout),
            None => {}
        }
    };
    sink.flush()?;
    drop(_raw);
    if std::env::var_os("RIVEMU_STATS").is_some() {
        let secs = started.elapsed().as_secs_f64();
        let n = machine.instret();
        eprintln!(
            "{PROG}: {n} instructions in {secs:.2}s ({:.1} MIPS)",
            n as f64 / secs / 1e6
        );
    }
    Ok(ExitCode::from(exit.clamp(0, 255) as u8))
}

/// Where the first serial port is connected.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SerialBackend {
    /// stdio with the `Ctrl-A` multiplexer (`mon:stdio`, `-nographic` default).
    MonStdio,
    /// Plain stdio (Ctrl-C terminates the emulator).
    Stdio,
    /// Output to a file, no input.
    File(std::path::PathBuf),
    /// Discard output.
    Null,
}

impl SerialBackend {
    fn parse(spec: Option<&str>) -> Result<Self> {
        Ok(match spec {
            None | Some("mon:stdio") => Self::MonStdio,
            Some("stdio") => Self::Stdio,
            Some("null" | "none") => Self::Null,
            Some(s) if s.starts_with("file:") => Self::File(s["file:".len()..].into()),
            Some(s) => {
                return Err(anyhow!(
                    "-serial {s}: unsupported backend (supported: mon:stdio, stdio, file:PATH, null)"
                ));
            }
        })
    }
}
