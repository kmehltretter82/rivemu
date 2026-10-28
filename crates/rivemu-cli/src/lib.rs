//! Command line front end for `rivemu-system-riscv64`.
//!
//! Accepts a subset of `qemu-system-riscv64`'s options with identical syntax,
//! so existing launch scripts work unchanged as long as they stay inside the
//! supported subset. Anything else is rejected with a clear error.

#![forbid(unsafe_code)]

pub mod console;
pub mod opts;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rivemu_backends::{
    BlockBackend, Chardev, FileChardev, HostFwdRule, NetBackend, NullChardev, SocketChardev,
    UserNet, UserNetConfig, block::open_block_image,
};
use rivemu_machine::virt::DEFAULT_RAM_SIZE;
use rivemu_machine::{MachineConfig, VirtioDeviceConfig};
use thiserror::Error;

pub use opts::{OptsError, QemuOpts};

/// Program name used in messages.
pub const PROG: &str = "rivemu-system-riscv64";

/// File name of the default firmware (same as QEMU's).
pub const DEFAULT_FIRMWARE: &str = "opensbi-riscv64-generic-fw_dynamic.bin";

/// Supported `-d` log items.
pub const LOG_ITEMS: &[(&str, &str)] = &[
    (
        "in_asm",
        "show target assembly code for each compiled block",
    ),
    ("exec", "show trace before each executed block"),
    ("cpu", "show CPU registers before entering a block"),
    ("int", "show interrupts/exceptions in short format"),
    ("mmu", "log MMU-related activities"),
    (
        "guest_errors",
        "log when the guest OS does something invalid",
    ),
    ("unimp", "log unimplemented functionality"),
];

/// Supported `-device` drivers (implemented in milestone P4).
pub const DEVICE_DRIVERS: &[(&str, &str)] = &[
    ("virtio-blk-device", "virtio block device (virtio-mmio)"),
    ("virtio-net-device", "virtio network device (virtio-mmio)"),
    ("virtio-serial-device", "virtio console (virtio-mmio)"),
    ("virtio-rng-device", "virtio entropy source (virtio-mmio)"),
];

/// Command line error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CliError {
    /// Unknown option.
    #[error("-{0}: invalid option")]
    InvalidOption(String),
    /// Option given without its argument.
    #[error("-{0}: requires an argument")]
    MissingArgument(String),
    /// Bad argument value.
    #[error("-{opt} {value}: {msg}")]
    InvalidValue {
        /// Option name.
        opt: String,
        /// Argument.
        value: String,
        /// Explanation.
        msg: String,
    },
    /// Bare argument (QEMU would treat it as a disk image).
    #[error(
        "{0}: positional disk images are not supported; use \
         -drive file=...,if=none,id=hd0 -device virtio-blk-device,drive=hd0"
    )]
    Positional(String),
    /// Non-UTF-8 argument.
    #[error("command line arguments must be valid UTF-8")]
    NotUtf8,
}

/// What the program should do after parsing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Action {
    /// Run the guest.
    #[default]
    Run,
    /// `-h` / `-help`.
    Help,
    /// `-version`.
    Version,
    /// `-machine help`.
    MachineHelp,
    /// `-cpu help`.
    CpuHelp,
    /// `-d help`.
    LogHelp,
    /// `-device help`.
    DeviceHelp,
}

/// Firmware selection (`-bios`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Bios {
    /// Search for the default OpenSBI image.
    #[default]
    Default,
    /// No firmware.
    None,
    /// Explicit file.
    Path(PathBuf),
}

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Requested action.
    pub action: Action,
    /// `-machine` parameters (besides `type`).
    pub machine: QemuOpts,
    /// `-cpu` model.
    pub cpu: String,
    /// RAM size in bytes (`-m`).
    pub memory: u64,
    /// Number of harts (`-smp`).
    pub smp: u32,
    /// `-bios`.
    pub bios: Bios,
    /// `-kernel`.
    pub kernel: Option<PathBuf>,
    /// `-initrd`.
    pub initrd: Option<PathBuf>,
    /// `-append`.
    pub append: Option<String>,
    /// `-dtb`.
    pub dtb: Option<PathBuf>,
    /// `-drive` (one per occurrence).
    pub drives: Vec<QemuOpts>,
    /// `-device`.
    pub devices: Vec<QemuOpts>,
    /// `-netdev`.
    pub netdevs: Vec<QemuOpts>,
    /// `-chardev`.
    pub chardevs: Vec<QemuOpts>,
    /// `-serial`.
    pub serials: Vec<String>,
    /// `-monitor`.
    pub monitor: Option<String>,
    /// `-qmp`.
    pub qmp: Vec<String>,
    /// `-nographic`.
    pub nographic: bool,
    /// `-gdb` / `-s`.
    pub gdb: Option<String>,
    /// `-S`: do not start the CPU at startup.
    pub start_paused: bool,
    /// `-d` items.
    pub log_items: Vec<String>,
    /// `-D` log file.
    pub log_file: Option<PathBuf>,
    /// `-icount`.
    pub icount: Option<QemuOpts>,
    /// `-no-reboot`.
    pub no_reboot: bool,
    /// `-no-shutdown`.
    pub no_shutdown: bool,
    /// `-name`.
    pub name: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            action: Action::Run,
            machine: QemuOpts::default(),
            cpu: "rv64".into(),
            memory: DEFAULT_RAM_SIZE,
            smp: 1,
            bios: Bios::Default,
            kernel: None,
            initrd: None,
            append: None,
            dtb: None,
            drives: Vec::new(),
            devices: Vec::new(),
            netdevs: Vec::new(),
            chardevs: Vec::new(),
            serials: Vec::new(),
            monitor: None,
            qmp: Vec::new(),
            nographic: false,
            gdb: None,
            start_paused: false,
            log_items: Vec::new(),
            log_file: None,
            icount: None,
            no_reboot: false,
            no_shutdown: false,
            name: None,
        }
    }
}

/// Parses a QEMU-style size: a number with optional fraction and optional
/// `B/K/M/G/T` suffix (case-insensitive). Numbers without suffix are
/// multiplied by `default_unit`.
pub fn parse_size(s: &str, default_unit: u64) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, suffix) = s.split_at(split);
    let unit = match suffix.to_ascii_lowercase().as_str() {
        "" => default_unit,
        "b" => 1,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    if num.is_empty() {
        return None;
    }
    if num.contains('.') {
        let v = num.parse::<f64>().ok()? * unit as f64;
        (v.is_finite() && v >= 0.0 && v < u64::MAX as f64).then(|| v.round() as u64)
    } else {
        num.parse::<u64>().ok()?.checked_mul(unit)
    }
}

fn invalid(opt: &str, value: &str, msg: impl Into<String>) -> CliError {
    CliError::InvalidValue {
        opt: opt.into(),
        value: value.into(),
        msg: msg.into(),
    }
}

fn opts(opt: &str, value: &str, implied: Option<&str>) -> Result<QemuOpts, CliError> {
    QemuOpts::parse(value, implied).map_err(|e| invalid(opt, value, e.to_string()))
}

fn parse_u32(opt: &str, raw: &str, v: Option<&str>, default: u32) -> Result<u32, CliError> {
    v.map_or(Ok(default), |v| {
        v.parse()
            .map_err(|_| invalid(opt, raw, format!("'{v}' is not a valid number")))
    })
}

/// Parses the command line (without the program name).
pub fn parse_args<I, S>(args: I) -> Result<Options, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut o = Options::default();
    let mut it = args.into_iter().map(Into::into);

    while let Some(arg) = it.next() {
        let Some(name) = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-')) else {
            return Err(CliError::Positional(arg));
        };
        if name.is_empty() {
            return Err(CliError::InvalidOption(String::new()));
        }
        let name = name.to_owned();
        let mut value = || {
            it.next()
                .ok_or_else(|| CliError::MissingArgument(name.clone()))
        };

        match name.as_str() {
            "h" | "help" => o.action = Action::Help,
            "version" => o.action = Action::Version,
            "M" | "machine" => {
                let v = value()?;
                let m = opts(&name, &v, Some("type"))?;
                match m.get("type") {
                    Some("help") => o.action = Action::MachineHelp,
                    Some("virt") | None => o.machine = m,
                    Some(t) => {
                        return Err(invalid(
                            &name,
                            &v,
                            format!("unsupported machine type '{t}'; use -machine help"),
                        ));
                    }
                }
            }
            "cpu" => {
                let v = value()?;
                let model = v.split(',').next().unwrap_or_default();
                match model {
                    "help" => o.action = Action::CpuHelp,
                    "rv64" | "max" => o.cpu = model.into(),
                    _ => {
                        return Err(invalid(
                            &name,
                            &v,
                            format!("unsupported CPU model '{model}'; use -cpu help"),
                        ));
                    }
                }
            }
            "m" => {
                let v = value()?;
                let m = opts(&name, &v, Some("size"))?;
                let size = m
                    .get("size")
                    .ok_or_else(|| invalid(&name, &v, "missing size"))?;
                o.memory = parse_size(size, 1 << 20)
                    .filter(|&s| s > 0)
                    .ok_or_else(|| invalid(&name, &v, format!("invalid RAM size '{size}'")))?;
            }
            "smp" => {
                let v = value()?;
                let m = opts(&name, &v, Some("cpus"))?;
                let cpus = match m.get("cpus") {
                    Some(c) => parse_u32(&name, &v, Some(c), 1)?,
                    None => {
                        parse_u32(&name, &v, m.get("sockets"), 1)?
                            * parse_u32(&name, &v, m.get("cores"), 1)?
                            * parse_u32(&name, &v, m.get("threads"), 1)?
                    }
                };
                if cpus == 0 {
                    return Err(invalid(&name, &v, "CPU count must be at least 1"));
                }
                o.smp = cpus;
            }
            "bios" => {
                o.bios = match value()?.as_str() {
                    "none" | "builtin" => Bios::None,
                    "default" => Bios::Default,
                    p => Bios::Path(p.into()),
                }
            }
            "kernel" => o.kernel = Some(value()?.into()),
            "initrd" => o.initrd = Some(value()?.into()),
            "append" => o.append = Some(value()?),
            "dtb" => o.dtb = Some(value()?.into()),
            "drive" => {
                let v = value()?;
                o.drives.push(opts(&name, &v, Some("file"))?);
            }
            "device" => {
                let v = value()?;
                let d = opts(&name, &v, Some("driver"))?;
                if d.get("driver") == Some("help") {
                    o.action = Action::DeviceHelp;
                } else {
                    o.devices.push(d);
                }
            }
            "netdev" => {
                let v = value()?;
                o.netdevs.push(opts(&name, &v, Some("type"))?);
            }
            "chardev" => {
                let v = value()?;
                o.chardevs.push(opts(&name, &v, Some("backend"))?);
            }
            "serial" => o.serials.push(value()?),
            "monitor" => o.monitor = Some(value()?),
            "qmp" => o.qmp.push(value()?),
            "nographic" => o.nographic = true,
            "display" => {
                let v = value()?;
                if opts(&name, &v, Some("type"))?.get("type") != Some("none") {
                    return Err(invalid(&name, &v, "only '-display none' is supported"));
                }
            }
            "s" => o.gdb = Some("tcp::1234".into()),
            "gdb" => o.gdb = Some(value()?),
            "S" => o.start_paused = true,
            "d" => {
                let v = value()?;
                for item in v.split(',').filter(|i| !i.is_empty()) {
                    if item == "help" {
                        o.action = Action::LogHelp;
                    } else if LOG_ITEMS.iter().any(|(n, _)| *n == item) {
                        o.log_items.push(item.into());
                    } else {
                        return Err(invalid(
                            &name,
                            &v,
                            format!("unknown log item '{item}'; use -d help"),
                        ));
                    }
                }
            }
            "D" => o.log_file = Some(value()?.into()),
            "icount" => {
                let v = value()?;
                o.icount = Some(opts(&name, &v, Some("shift"))?);
            }
            "no-reboot" => o.no_reboot = true,
            "no-shutdown" => o.no_shutdown = true,
            "name" => o.name = Some(value()?),
            _ => return Err(CliError::InvalidOption(name)),
        }
    }
    Ok(o)
}

/// Search path for the default firmware, in priority order.
pub fn firmware_search_path() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("RIVEMU_FIRMWARE_DIR") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_owned))
    {
        dirs.push(exe_dir.join("../share/rivemu"));
        dirs.push(exe_dir);
    }
    // Reuse an installed QEMU's firmware if present.
    for d in [
        "/opt/homebrew/share/qemu",
        "/usr/local/share/qemu",
        "/usr/share/qemu",
    ] {
        dirs.push(d.into());
    }
    dirs
}

/// Resolves the options into a machine configuration.
pub fn machine_config(o: &Options) -> Result<MachineConfig, String> {
    let bios = match &o.bios {
        Bios::None => None,
        Bios::Path(p) => Some(p.clone()),
        Bios::Default => {
            let found = firmware_search_path()
                .into_iter()
                .map(|d| d.join(DEFAULT_FIRMWARE))
                .find(|p| p.is_file());
            match (found, &o.kernel) {
                (Some(p), _) => Some(p),
                // When booting a kernel and no external OpenSBI binary is
                // installed on disk, fall back to rivemu's built-in Rust SBI.
                (None, Some(_)) => None,
                (None, None) => {
                    return Err(format!(
                        "unable to find default firmware '{DEFAULT_FIRMWARE}' \
                         (set RIVEMU_FIRMWARE_DIR, or use -bios <file> / -bios builtin / -bios none)"
                    ));
                }
            }
        }
    };

    let mut drives: HashMap<String, Arc<dyn BlockBackend>> = HashMap::new();
    let mut virtio = Vec::new();

    for (idx, d) in o.drives.iter().enumerate() {
        let file = d
            .get("file")
            .ok_or_else(|| "-drive: missing 'file=' parameter".to_string())?;
        let ro = matches!(d.get("readonly"), Some("on" | "true" | "1"));
        let fmt = d.get("format");
        let backend = open_block_image(Path::new(file), fmt, ro)
            .map_err(|e| format!("-drive file={file}: {e}"))?;
        let id = d
            .get("id")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("drive{idx}"));
        if d.get("if") == Some("virtio") {
            virtio.push(VirtioDeviceConfig::Blk {
                backend: backend.clone(),
                serial: id.clone(),
            });
        }
        drives.insert(id, backend);
    }

    let mut netdevs: HashMap<String, Arc<dyn NetBackend>> = HashMap::new();
    for (idx, n) in o.netdevs.iter().enumerate() {
        let kind = n.get("type").unwrap_or("user");
        if kind != "user" {
            return Err(format!(
                "-netdev {kind}: unsupported backend (supported: user)"
            ));
        }
        let id = n
            .get("id")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("net{idx}"));
        let mut hostfwd = Vec::new();
        for (k, v) in n.params() {
            if k == "hostfwd" {
                hostfwd.push(HostFwdRule::parse(v).map_err(|e| format!("-netdev {id}: {e}"))?);
            }
        }
        let backend: Arc<dyn NetBackend> = UserNet::new(UserNetConfig {
            hostfwd,
            ..Default::default()
        });
        netdevs.insert(id, backend);
    }

    let mut chardevs: HashMap<String, Arc<dyn Chardev>> = HashMap::new();
    for (idx, c) in o.chardevs.iter().enumerate() {
        let backend_name = c.get("backend").unwrap_or("null");
        let id = c
            .get("id")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("char{idx}"));
        let ch: Arc<dyn Chardev> = match backend_name {
            "null" => Arc::new(NullChardev),
            "file" => {
                let path = c
                    .get("path")
                    .ok_or_else(|| format!("-chardev file,id={id}: missing 'path='"))?;
                FileChardev::create(Path::new(path))
                    .map_err(|e| format!("-chardev file,path={path}: {e}"))?
            }
            "socket" => {
                let host = c.get("host").unwrap_or("127.0.0.1");
                let port = c
                    .get("port")
                    .ok_or_else(|| format!("-chardev socket,id={id}: missing 'port='"))?;
                let addr = format!("{host}:{port}");
                let is_server = c.get("server") == Some("on");
                let wait = is_server && c.get("nowait") != Some("on");
                if is_server {
                    SocketChardev::bind_tcp(&addr, wait)
                        .map_err(|e| format!("-chardev socket,{addr}: {e}"))?
                } else {
                    SocketChardev::connect_tcp(&addr)
                        .map_err(|e| format!("-chardev socket,{addr}: {e}"))?
                }
            }
            other => {
                return Err(format!(
                    "-chardev {other}: unsupported backend (supported: null, file, socket)"
                ));
            }
        };
        chardevs.insert(id, ch);
    }

    for dev in &o.devices {
        let driver = dev
            .get("driver")
            .ok_or_else(|| "-device: missing driver name".to_string())?;
        match driver {
            "virtio-blk-device" => {
                let drive_id = dev
                    .get("drive")
                    .ok_or_else(|| "-device virtio-blk-device: missing 'drive='".to_string())?;
                let backend = drives.get(drive_id).ok_or_else(|| {
                    format!("-device virtio-blk-device: unknown drive '{drive_id}'")
                })?;
                let serial = dev.get("serial").unwrap_or(drive_id).to_owned();
                virtio.push(VirtioDeviceConfig::Blk {
                    backend: backend.clone(),
                    serial,
                });
            }
            "virtio-net-device" => {
                let backend = match dev.get("netdev") {
                    Some(net_id) => netdevs
                        .get(net_id)
                        .ok_or_else(|| {
                            format!("-device virtio-net-device: unknown netdev '{net_id}'")
                        })?
                        .clone(),
                    None => UserNet::new(UserNetConfig::default()),
                };
                virtio.push(VirtioDeviceConfig::Net { backend });
            }
            "virtio-serial-device" | "virtio-console" => {
                let chardev = match dev.get("chardev") {
                    Some(ch_id) => chardevs
                        .get(ch_id)
                        .ok_or_else(|| format!("-device {driver}: unknown chardev '{ch_id}'"))?
                        .clone(),
                    None => Arc::new(NullChardev),
                };
                virtio.push(VirtioDeviceConfig::Console { chardev });
            }
            "virtio-rng-device" => {
                virtio.push(VirtioDeviceConfig::Rng);
            }
            other => {
                return Err(format!(
                    "-device {other}: unsupported device (use -device help)"
                ));
            }
        }
    }

    let icount = match &o.icount {
        Some(ic) => {
            let s = ic.get("shift").unwrap_or("auto");
            if s == "auto" {
                Some(5u8)
            } else {
                Some(
                    s.parse::<u8>()
                        .map_err(|_| format!("-icount: invalid shift '{s}'"))?,
                )
            }
        }
        None => None,
    };

    Ok(MachineConfig {
        ram_size: o.memory,
        smp: o.smp,
        bios,
        kernel: o.kernel.clone(),
        initrd: o.initrd.clone(),
        dtb: o.dtb.clone(),
        append: o.append.clone(),
        virtio,
        icount,
    })
}

/// `-help` text.
pub fn help_text() -> String {
    format!(
        "\
{PROG} version {version}
usage: {PROG} [options]

Machine:
  -machine|-M virt[,prop=value...]  select machine (only 'virt'; '-M help' lists)
  -cpu rv64|max                     select CPU model ('-cpu help' lists)
  -m [size=]N[KMG]                  guest RAM size (default 128M; unit M if omitted)
  -smp [cpus=]N                     number of harts

Boot:
  -bios file|none|default           firmware image (default: {DEFAULT_FIRMWARE})
  -kernel file                      kernel image (ELF or raw Image)
  -initrd file                      initial ramdisk
  -append cmdline                   kernel command line
  -dtb file                         use this device tree instead of a generated one

Devices and backends:
  -drive file=...,if=none,id=...,format=raw   block backend
  -device driver[,prop=value...]    add device ('-device help' lists)
  -netdev user|tap,id=...[,...]     network backend
  -chardev backend,id=...[,...]     character backend
  -serial dev                       serial port backend (e.g. stdio, mon:stdio, pty)
  -monitor dev                      HMP monitor backend
  -qmp dev                          QMP backend (e.g. unix:/tmp/qmp.sock,server,nowait)
  -nographic                        no graphics; serial + monitor on stdio
  -display none                     no display

Debugging:
  -s                                shorthand for -gdb tcp::1234
  -gdb dev                          wait for gdb connection on dev
  -S                                do not start CPU at startup
  -d item[,...]                     enable logging ('-d help' lists)
  -D file                           write log to file
  -icount [shift=]N                 deterministic instruction-count timing

Misc:
  -no-reboot                        exit instead of rebooting
  -no-shutdown                      stop instead of exiting on guest shutdown
  -name string                      guest name
  -h|-help                          display this help and exit
  -version                          display version information and exit
",
        version = env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options, CliError> {
        parse_args(args.iter().copied())
    }

    #[test]
    fn defaults() {
        let o = parse(&[]).unwrap();
        assert_eq!(o, Options::default());
        assert_eq!(o.memory, 128 << 20);
    }

    #[test]
    fn typical_linux_command_line() {
        let o = parse(&[
            "-M",
            "virt",
            "-m",
            "2G",
            "-smp",
            "4",
            "-nographic",
            "-bios",
            "none",
            "-kernel",
            "Image",
            "-initrd",
            "rootfs.cpio",
            "-append",
            "console=ttyS0 earlycon",
            "-drive",
            "file=disk.img,format=raw,if=none,id=hd0",
            "-device",
            "virtio-blk-device,drive=hd0",
            "-netdev",
            "user,id=net0,hostfwd=tcp::2222-:22",
            "-device",
            "virtio-net-device,netdev=net0",
            "-s",
            "-S",
            "--d",
            "int,guest_errors",
            "-D",
            "log.txt",
        ])
        .unwrap();
        assert_eq!(o.memory, 2 << 30);
        assert_eq!(o.smp, 4);
        assert!(o.nographic && o.start_paused);
        assert_eq!(o.bios, Bios::None);
        assert_eq!(o.kernel.as_deref(), Some(Path::new("Image")));
        assert_eq!(o.append.as_deref(), Some("console=ttyS0 earlycon"));
        assert_eq!(o.drives[0].get("file"), Some("disk.img"));
        assert_eq!(o.devices.len(), 2);
        assert_eq!(o.devices[1].get("driver"), Some("virtio-net-device"));
        assert_eq!(o.netdevs[0].get("type"), Some("user"));
        assert_eq!(o.gdb.as_deref(), Some("tcp::1234"));
        assert_eq!(o.log_items, ["int", "guest_errors"]);
    }

    #[test]
    fn memory_sizes() {
        let m = |s: &str| parse(&["-m", s]).map(|o| o.memory);
        assert_eq!(m("512"), Ok(512 << 20));
        assert_eq!(m("1.5G"), Ok(3 << 29));
        assert_eq!(m("size=256M,slots=2,maxmem=1G"), Ok(256 << 20));
        assert_eq!(m("65536k"), Ok(64 << 20));
        assert!(m("0").is_err());
        assert!(m("12X").is_err());
        assert!(m("G").is_err());
    }

    #[test]
    fn smp_topology() {
        let s = |v: &str| parse(&["-smp", v]).map(|o| o.smp);
        assert_eq!(s("cpus=2"), Ok(2));
        assert_eq!(s("sockets=2,cores=2,threads=2"), Ok(8));
        assert!(s("0").is_err());
        assert!(s("x").is_err());
    }

    #[test]
    fn help_actions() {
        let a = |args: &[&str]| parse(args).unwrap().action;
        assert_eq!(a(&["-help"]), Action::Help);
        assert_eq!(a(&["--version"]), Action::Version);
        assert_eq!(a(&["-M", "help"]), Action::MachineHelp);
        assert_eq!(a(&["-cpu", "help"]), Action::CpuHelp);
        assert_eq!(a(&["-d", "help"]), Action::LogHelp);
        assert_eq!(a(&["-device", "help"]), Action::DeviceHelp);
    }

    #[test]
    fn errors_match_qemu_style() {
        let e = |args: &[&str]| parse(args).unwrap_err().to_string();
        assert_eq!(e(&["-enable-kvm"]), "-enable-kvm: invalid option");
        assert_eq!(e(&["-kernel"]), "-kernel: requires an argument");
        assert!(e(&["-M", "sifive_u"]).contains("unsupported machine type 'sifive_u'"));
        assert!(e(&["-cpu", "sifive-u54"]).contains("unsupported CPU model"));
        assert!(e(&["-d", "int,bogus"]).contains("unknown log item 'bogus'"));
        assert!(e(&["-display", "gtk"]).contains("only '-display none'"));
        assert!(e(&["disk.img"]).starts_with("disk.img: positional disk images"));
    }

    #[test]
    fn bios_variants_and_config() {
        let o = parse(&["-bios", "fw.bin", "-m", "1G", "-smp", "2"]).unwrap();
        let c = machine_config(&o).unwrap();
        assert_eq!(c.bios.as_deref(), Some(Path::new("fw.bin")));
        assert_eq!((c.ram_size, c.smp), (1 << 30, 2));
        let o = parse(&["-bios", "none"]).unwrap();
        assert_eq!(machine_config(&o).unwrap().bios, None);
    }

    #[test]
    fn size_parser() {
        assert_eq!(parse_size("4096", 1), Some(4096));
        assert_eq!(parse_size("1t", 1), Some(1 << 40));
        assert_eq!(parse_size("2b", 1 << 20), Some(2));
        assert_eq!(parse_size("99999999999999999999", 1), None);
        assert_eq!(parse_size("20000000000000T", 1), None);
    }
}
