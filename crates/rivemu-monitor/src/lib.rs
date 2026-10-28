//! Human Monitor Protocol (HMP) and QEMU Machine Protocol (QMP) subset.

#![forbid(unsafe_code)]

use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use rivemu_core::AccessSize;
use rivemu_machine::Machine;

/// Action requested by an HMP or QMP command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorAction {
    /// Pause guest execution (`stop`).
    Stop,
    /// Resume guest execution (`cont`).
    Cont,
    /// Reset the guest machine (`system_reset`).
    Reset,
    /// Power off the guest (`system_powerdown`).
    Powerdown,
    /// Quit the emulator immediately (`quit`).
    Quit,
}

const REG_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

/// Executes a single HMP command line and returns `(output, optional_action)`.
pub fn exec_hmp(line: &str, machine: &Machine, paused: bool) -> (String, Option<MonitorAction>) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return (String::new(), None);
    }
    match (parts[0], parts.get(1).copied()) {
        ("help" | "?", _) => (
            "\
Supported HMP commands:
  help | ?                 show this help
  info registers           display CPU registers
  info mtree               display physical memory regions
  info status              display VM run state
  stop                     pause emulation
  cont | c                 resume emulation
  system_reset             reset the machine
  system_powerdown         power off the machine
  xp [/N] ADDR             examine physical memory words at ADDR
  quit | q                 exit rivemu
"
            .into(),
            None,
        ),
        ("info", Some("registers" | "regs")) => {
            let mut out = String::new();
            for h in &machine.harts {
                let _ = writeln!(
                    out,
                    "Hart {}: pc={:#018x} prv={:?} mstatus={:#018x} satp={:#018x}",
                    h.id, h.pc, h.prv, h.csr.mstatus, h.csr.satp
                );
                for row in 0..8 {
                    for col in 0..4 {
                        let i = row * 4 + col;
                        let _ = write!(out, " {:>4}={:#018x}", REG_NAMES[i], h.x[i]);
                    }
                    out.push('\n');
                }
            }
            (out, None)
        }
        ("info", Some("mtree")) => {
            let mut out = String::from("address-space: memory\n");
            for r in machine.bus.regions() {
                let _ = writeln!(
                    out,
                    "  {:#018x}-{:#018x} ({:?}): {}",
                    r.base,
                    r.last(),
                    r.kind,
                    r.name
                );
            }
            (out, None)
        }
        ("info", Some("status")) => {
            let st = if paused { "paused" } else { "running" };
            (format!("VM status: {st}\n"), None)
        }
        ("stop" | "pause", _) => (String::new(), Some(MonitorAction::Stop)),
        ("cont" | "c", _) => (String::new(), Some(MonitorAction::Cont)),
        ("system_reset", _) => (String::new(), Some(MonitorAction::Reset)),
        ("system_powerdown", _) => (String::new(), Some(MonitorAction::Powerdown)),
        ("quit" | "q", _) => (String::new(), Some(MonitorAction::Quit)),
        ("xp" | "x", _) => (exec_xp(&parts[1..], machine), None),
        _ => (format!("unknown command: '{line}' (try 'help')\n"), None),
    }
}

fn exec_xp(args: &[&str], machine: &Machine) -> String {
    let (count, addr_s) = match args {
        [fmt, addr] if fmt.starts_with('/') => {
            let digits: String = fmt[1..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            (digits.parse::<u64>().unwrap_or(1).clamp(1, 64), *addr)
        }
        [addr] => (1, *addr),
        _ => return "usage: xp [/count] addr\n".into(),
    };
    let clean = addr_s
        .strip_prefix("0x")
        .or_else(|| addr_s.strip_prefix("0X"))
        .unwrap_or(addr_s);
    let Ok(base) = u64::from_str_radix(clean, 16) else {
        return format!("invalid hex address '{addr_s}'\n");
    };
    let mut out = String::new();
    for i in 0..count {
        let a = base.wrapping_add(i * 4);
        match machine.bus.read(a, AccessSize::Word) {
            Ok(v) => {
                let _ = writeln!(out, "{a:#012x}: {v:#010x}");
            }
            Err(e) => {
                let _ = writeln!(out, "{a:#012x}: <{e}>");
                break;
            }
        }
    }
    out
}

/// QMP greeting banner sent when a client connects.
pub const QMP_GREETING: &str = r#"{"QMP":{"version":{"qemu":{"micro":0,"minor":1,"major":0},"package":"rivemu"},"capabilities":[]}}"#;

/// Processes a single QMP JSON line and returns `(json_reply, optional_action)`.
pub fn exec_qmp_line(line: &str, paused: bool) -> (String, Option<MonitorAction>) {
    let Some(cmd) = extract_json_string(line, "execute") else {
        return (
            r#"{"error":{"class":"GenericError","desc":"missing 'execute' field"}}"#.into(),
            None,
        );
    };
    match cmd {
        "qmp_capabilities" => (r#"{"return":{}}"#.into(), None),
        "query-status" => {
            let running = !paused;
            let status = if paused { "paused" } else { "running" };
            (
                format!(
                    r#"{{"return":{{"status":"{status}","singlestep":false,"running":{running}}}}}"#
                ),
                None,
            )
        }
        "stop" => (r#"{"return":{}}"#.into(), Some(MonitorAction::Stop)),
        "cont" => (r#"{"return":{}}"#.into(), Some(MonitorAction::Cont)),
        "system_reset" => (r#"{"return":{}}"#.into(), Some(MonitorAction::Reset)),
        "system_powerdown" => (r#"{"return":{}}"#.into(), Some(MonitorAction::Powerdown)),
        "quit" => (r#"{"return":{}}"#.into(), Some(MonitorAction::Quit)),
        other => (
            format!(
                r#"{{"error":{{"class":"CommandNotFound","desc":"The command {other} has not been found"}}}}"#
            ),
            None,
        ),
    }
}

fn extract_json_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\"");
    let idx = json.find(&pat)?;
    let rest = json[idx + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Non-blocking QMP server supporting `tcp:host:port` and `unix:/path`.
#[derive(Debug)]
pub struct QmpServer {
    listener: QmpListener,
    client: Option<QmpClient>,
    unix_path: Option<PathBuf>,
}

#[derive(Debug)]
enum QmpListener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
}

#[derive(Debug)]
enum QmpClient {
    Tcp(BufReader<TcpStream>),
    #[cfg(unix)]
    Unix(BufReader<UnixStream>),
}

impl QmpServer {
    /// Binds a QMP endpoint from a QEMU `-qmp` specification
    /// (e.g. `"unix:/tmp/qmp.sock,server,nowait"` or `"tcp:127.0.0.1:4444,server,nowait"`).
    pub fn bind(spec: &str) -> io::Result<Self> {
        let base = spec.split(',').next().unwrap_or_default().trim();
        #[cfg(unix)]
        if let Some(path) = base.strip_prefix("unix:") {
            let p = PathBuf::from(path);
            let _ = std::fs::remove_file(&p);
            let l = UnixListener::bind(&p)?;
            l.set_nonblocking(true)?;
            return Ok(Self {
                listener: QmpListener::Unix(l),
                client: None,
                unix_path: Some(p),
            });
        }
        let addr = base.strip_prefix("tcp:").unwrap_or(base);
        let addr = if let Some(port) = addr.strip_prefix(':') {
            format!("127.0.0.1:{port}")
        } else {
            addr.to_owned()
        };
        let l = TcpListener::bind(&addr)?;
        l.set_nonblocking(true)?;
        Ok(Self {
            listener: QmpListener::Tcp(l),
            client: None,
            unix_path: None,
        })
    }

    /// Polls for incoming QMP connections and commands without blocking.
    pub fn poll(&mut self, paused: bool) -> Vec<MonitorAction> {
        if self.client.is_none() {
            match &self.listener {
                QmpListener::Tcp(l) => {
                    if let Ok((mut s, _)) = l.accept() {
                        let _ = s.set_nonblocking(true);
                        let _ = writeln!(s, "{QMP_GREETING}");
                        let _ = s.flush();
                        self.client = Some(QmpClient::Tcp(BufReader::new(s)));
                    }
                }
                #[cfg(unix)]
                QmpListener::Unix(l) => {
                    if let Ok((mut s, _)) = l.accept() {
                        let _ = s.set_nonblocking(true);
                        let _ = writeln!(s, "{QMP_GREETING}");
                        let _ = s.flush();
                        self.client = Some(QmpClient::Unix(BufReader::new(s)));
                    }
                }
            }
        }

        let mut actions = Vec::new();
        let Some(client) = &mut self.client else {
            return actions;
        };
        loop {
            let mut line = String::new();
            let res = match client {
                QmpClient::Tcp(r) => r.read_line(&mut line),
                #[cfg(unix)]
                QmpClient::Unix(r) => r.read_line(&mut line),
            };
            match res {
                Ok(0) => {
                    self.client = None;
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let (reply, act) = exec_qmp_line(trimmed, paused);
                    let write_res = match client {
                        QmpClient::Tcp(r) => {
                            writeln!(r.get_mut(), "{reply}").and_then(|()| r.get_mut().flush())
                        }
                        #[cfg(unix)]
                        QmpClient::Unix(r) => {
                            writeln!(r.get_mut(), "{reply}").and_then(|()| r.get_mut().flush())
                        }
                    };
                    if let Some(a) = act {
                        actions.push(a);
                    }
                    if write_res.is_err() {
                        self.client = None;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.client = None;
                    break;
                }
            }
        }
        actions
    }
}

impl Drop for QmpServer {
    fn drop(&mut self) {
        if let Some(p) = &self.unix_path {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivemu_machine::MachineConfig;

    #[test]
    fn hmp_commands() {
        let machine = Machine::new(MachineConfig::default()).unwrap();
        let (out, act) = exec_hmp("info status", &machine, false);
        assert_eq!(out, "VM status: running\n");
        assert_eq!(act, None);

        let (out, _) = exec_hmp("info registers", &machine, false);
        assert!(out.contains("Hart 0: pc="));

        let (out, _) = exec_hmp("info mtree", &machine, false);
        assert!(out.contains("riscv_virt_board.ram"));

        let (out, _) = exec_hmp("xp /2 0x1000", &machine, false);
        assert!(out.contains("0x0000001000: 0x00000297"));

        assert_eq!(
            exec_hmp("stop", &machine, false).1,
            Some(MonitorAction::Stop)
        );
        assert_eq!(exec_hmp("c", &machine, true).1, Some(MonitorAction::Cont));
        assert_eq!(
            exec_hmp("quit", &machine, false).1,
            Some(MonitorAction::Quit)
        );
    }

    #[test]
    fn qmp_protocol() {
        let (rep, act) = exec_qmp_line(r#"{"execute": "qmp_capabilities"}"#, false);
        assert_eq!(rep, r#"{"return":{}}"#);
        assert_eq!(act, None);

        let (rep, _) = exec_qmp_line(r#"{"execute": "query-status"}"#, true);
        assert!(rep.contains(r#""status":"paused""#));
        assert!(rep.contains(r#""running":false"#));

        let (_, act) = exec_qmp_line(r#"{"execute": "system_powerdown"}"#, false);
        assert_eq!(act, Some(MonitorAction::Powerdown));
    }
}
