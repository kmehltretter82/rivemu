//! GDB Remote Serial Protocol (RSP) stub for `rivemu` (`-s` / `-gdb` / `-S`).
//!
//! Speaks the standard RSP over TCP so `riscv64-elf-gdb`, `gdb-multiarch` or
//! `lldb` can attach with `target remote :1234`, inspect registers and memory,
//! set software/hardware breakpoints, single-step and interrupt execution.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use rivemu_core::AccessSize;
use rivemu_machine::Machine;
use thiserror::Error;

/// Errors returned by the GDB stub.
#[derive(Debug, Error)]
pub enum GdbError {
    /// Invalid `-gdb` address specification.
    #[error("invalid -gdb address '{0}' (expected e.g. tcp::1234 or 127.0.0.1:1234)")]
    InvalidSpec(String),
    /// Socket I/O error.
    #[error("gdb socket error: {0}")]
    Io(#[from] io::Error),
}

/// Action requested by the debugger after handling a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GdbAction {
    /// Stay paused and wait for the next packet.
    Paused,
    /// Resume execution until a breakpoint, guest exit or Ctrl-C.
    Continue,
    /// Debugger detached (`D`); resume guest without debugger.
    Detach,
    /// Debugger killed the target (`k`); exit emulator immediately.
    Kill,
}

const TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
  <architecture>riscv:rv64</architecture>
  <feature name="org.gnu.gdb.riscv.cpu">
    <reg name="zero" bitsize="64" type="int" regnum="0"/>
    <reg name="ra" bitsize="64" type="code_ptr"/>
    <reg name="sp" bitsize="64" type="data_ptr"/>
    <reg name="gp" bitsize="64" type="data_ptr"/>
    <reg name="tp" bitsize="64" type="data_ptr"/>
    <reg name="t0" bitsize="64" type="int"/>
    <reg name="t1" bitsize="64" type="int"/>
    <reg name="t2" bitsize="64" type="int"/>
    <reg name="fp" bitsize="64" type="data_ptr"/>
    <reg name="s1" bitsize="64" type="int"/>
    <reg name="a0" bitsize="64" type="int"/>
    <reg name="a1" bitsize="64" type="int"/>
    <reg name="a2" bitsize="64" type="int"/>
    <reg name="a3" bitsize="64" type="int"/>
    <reg name="a4" bitsize="64" type="int"/>
    <reg name="a5" bitsize="64" type="int"/>
    <reg name="a6" bitsize="64" type="int"/>
    <reg name="a7" bitsize="64" type="int"/>
    <reg name="s2" bitsize="64" type="int"/>
    <reg name="s3" bitsize="64" type="int"/>
    <reg name="s4" bitsize="64" type="int"/>
    <reg name="s5" bitsize="64" type="int"/>
    <reg name="s6" bitsize="64" type="int"/>
    <reg name="s7" bitsize="64" type="int"/>
    <reg name="s8" bitsize="64" type="int"/>
    <reg name="s9" bitsize="64" type="int"/>
    <reg name="s10" bitsize="64" type="int"/>
    <reg name="s11" bitsize="64" type="int"/>
    <reg name="t3" bitsize="64" type="int"/>
    <reg name="t4" bitsize="64" type="int"/>
    <reg name="t5" bitsize="64" type="int"/>
    <reg name="t6" bitsize="64" type="int"/>
    <reg name="pc" bitsize="64" type="code_ptr" regnum="32"/>
  </feature>
</target>
"#;

/// Parses a QEMU-style `-gdb` TCP spec (`tcp::1234`, `:1234`, `127.0.0.1:1234`).
pub fn parse_tcp_spec(spec: &str) -> Result<String, GdbError> {
    let base = spec.split(',').next().unwrap_or_default().trim();
    let addr = base.strip_prefix("tcp:").unwrap_or(base);
    if let Some(port) = addr.strip_prefix(':') {
        if port.parse::<u16>().is_ok() {
            return Ok(format!("127.0.0.1:{port}"));
        }
    } else if let Some((host, port)) = addr.rsplit_once(':') {
        if port.parse::<u16>().is_ok() {
            let host = if host.is_empty() || host == "localhost" {
                "127.0.0.1"
            } else {
                host
            };
            return Ok(format!("{host}:{port}"));
        }
    }
    Err(GdbError::InvalidSpec(spec.into()))
}

/// Listening GDB server waiting for or connected to a debugger client.
#[derive(Debug)]
pub struct GdbServer {
    listener: TcpListener,
    client: Option<GdbConn>,
}

#[derive(Debug)]
struct GdbConn {
    stream: TcpStream,
    no_ack: bool,
    selected_hart: usize,
    rx_buf: Vec<u8>,
}

impl GdbServer {
    /// Binds a TCP listener for `spec` (e.g. `"tcp::1234"`).
    pub fn bind(spec: &str) -> Result<Self, GdbError> {
        let addr = parse_tcp_spec(spec)?;
        let listener = TcpListener::bind(&addr)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            client: None,
        })
    }

    /// Local socket address the server is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Blocks until a GDB client connects (used when `-S` is specified).
    pub fn wait_for_client(&mut self) -> io::Result<()> {
        if self.client.is_some() {
            return Ok(());
        }
        self.listener.set_nonblocking(false)?;
        let (stream, _) = self.listener.accept()?;
        self.listener.set_nonblocking(true)?;
        let _ = stream.set_nodelay(true);
        self.client = Some(GdbConn::new(stream));
        Ok(())
    }

    /// Accepts a pending client connection without blocking, if any.
    pub fn poll_accept(&mut self) -> io::Result<bool> {
        if self.client.is_some() {
            return Ok(true);
        }
        match self.listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                self.client = Some(GdbConn::new(stream));
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Checks whether a connected client has sent `Ctrl-C` (`0x03`) or a packet
    /// while the guest is running.
    pub fn check_interrupt(&mut self) -> io::Result<bool> {
        let Some(conn) = &mut self.client else {
            return self.poll_accept();
        };
        conn.stream.set_nonblocking(true)?;
        let mut tmp = [0u8; 256];
        let res = match conn.stream.read(&mut tmp) {
            Ok(0) => {
                self.client = None;
                Ok(false)
            }
            Ok(n) => {
                conn.rx_buf.extend_from_slice(&tmp[..n]);
                Ok(conn.rx_buf.contains(&0x03) || conn.rx_buf.contains(&b'$'))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        };
        if let Some(c) = &mut self.client {
            let _ = c.stream.set_nonblocking(false);
        }
        res
    }

    /// Notifies GDB that the target stopped with SIGTRAP (`S05`).
    pub fn notify_stop(&mut self) -> io::Result<()> {
        if let Some(conn) = &mut self.client {
            // Drain any pending Ctrl-C byte before sending the stop reply.
            if let Some(pos) = conn.rx_buf.iter().position(|&b| b == 0x03) {
                conn.rx_buf.drain(..=pos);
            }
            conn.send_packet("S05")?;
        }
        Ok(())
    }

    /// Runs the blocking command loop while the target is paused, returning
    /// when the debugger requests `Continue`, `Detach` or `Kill` (or disconnects).
    pub fn handle_paused(&mut self, machine: &mut Machine) -> io::Result<GdbAction> {
        self.wait_for_client()?;
        let Some(conn) = &mut self.client else {
            return Ok(GdbAction::Continue);
        };
        conn.stream.set_nonblocking(false)?;
        let _ = conn
            .stream
            .set_read_timeout(Some(Duration::from_secs(3600)));
        loop {
            let Some(pkt) = conn.read_packet()? else {
                self.client = None;
                return Ok(GdbAction::Detach);
            };
            match conn.dispatch(machine, &pkt)? {
                GdbAction::Paused => {}
                action @ (GdbAction::Continue | GdbAction::Detach | GdbAction::Kill) => {
                    if action == GdbAction::Detach {
                        for h in &mut machine.harts {
                            h.breakpoints.clear();
                        }
                        self.client = None;
                    }
                    return Ok(action);
                }
            }
        }
    }
}

impl GdbConn {
    fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            no_ack: false,
            selected_hart: 0,
            rx_buf: Vec::new(),
        }
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        if !self.rx_buf.is_empty() {
            return Ok(Some(self.rx_buf.remove(0)));
        }
        let mut b = [0u8; 1];
        match self.stream.read(&mut b) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(b[0])),
            Err(e) => Err(e),
        }
    }

    fn read_packet(&mut self) -> io::Result<Option<String>> {
        loop {
            let Some(b) = self.read_byte()? else {
                return Ok(None);
            };
            match b {
                b'$' => break,
                0x03 => return Ok(Some("?".into())),
                _ => continue,
            }
        }
        let mut data = Vec::new();
        loop {
            let Some(b) = self.read_byte()? else {
                return Ok(None);
            };
            if b == b'#' {
                break;
            }
            data.push(b);
        }
        // Consume 2-byte hex checksum.
        let _ = self.read_byte()?;
        let _ = self.read_byte()?;
        if !self.no_ack {
            self.stream.write_all(b"+")?;
            self.stream.flush()?;
        }
        Ok(Some(String::from_utf8_lossy(&data).into_owned()))
    }

    fn send_packet(&mut self, payload: &str) -> io::Result<()> {
        let sum: u8 = payload.bytes().fold(0u8, u8::wrapping_add);
        let msg = format!("${payload}#{sum:02x}");
        self.stream.write_all(msg.as_bytes())?;
        self.stream.flush()
    }

    fn dispatch(&mut self, machine: &mut Machine, pkt: &str) -> io::Result<GdbAction> {
        let hart_idx = self
            .selected_hart
            .min(machine.harts.len().saturating_sub(1));

        if pkt == "?" {
            self.send_packet("S05")?;
        } else if pkt.starts_with("qSupported") {
            self.send_packet(
                "PacketSize=4096;QStartNoAckMode+;swbreak+;hwbreak+;qXfer:features:read+",
            )?;
        } else if pkt == "QStartNoAckMode" {
            self.no_ack = true;
            self.send_packet("OK")?;
        } else if let Some(rest) = pkt.strip_prefix("qXfer:features:read:target.xml:") {
            let (off, len) = parse_pair(rest).unwrap_or((0, TARGET_XML.len() as u64));
            let bytes = TARGET_XML.as_bytes();
            let start = (off as usize).min(bytes.len());
            let end = start.saturating_add(len as usize).min(bytes.len());
            let prefix = if end < bytes.len() { "m" } else { "l" };
            let slice = std::str::from_utf8(&bytes[start..end]).unwrap_or_default();
            self.send_packet(&format!("{prefix}{slice}"))?;
        } else if pkt == "qAttached" {
            self.send_packet("1")?;
        } else if pkt == "qC" {
            self.send_packet(&format!("QC{:x}", hart_idx + 1))?;
        } else if pkt == "qfThreadInfo" {
            let list: Vec<String> = (1..=machine.harts.len())
                .map(|i| format!("{i:x}"))
                .collect();
            self.send_packet(&format!("m{}", list.join(",")))?;
        } else if pkt == "qsThreadInfo" {
            self.send_packet("l")?;
        } else if let Some(rest) = pkt.strip_prefix("Hg").or_else(|| pkt.strip_prefix("Hc")) {
            if let Ok(tid) = usize::from_str_radix(rest.trim_start_matches('-'), 16) {
                if tid >= 1 && tid <= machine.harts.len() {
                    self.selected_hart = tid - 1;
                }
            }
            self.send_packet("OK")?;
        } else if pkt.starts_with('T') {
            self.send_packet("OK")?;
        } else if pkt == "vCont?" {
            self.send_packet("vCont;c;C;s;S")?;
        } else if let Some(rest) = pkt.strip_prefix("vCont;") {
            let action = rest.split(';').next().unwrap_or_default();
            if action.starts_with('s') || action.starts_with('S') {
                step_one(machine, hart_idx);
                self.send_packet("S05")?;
            } else if action.starts_with('c') || action.starts_with('C') {
                step_past_breakpoint(machine);
                return Ok(GdbAction::Continue);
            } else {
                self.send_packet("")?;
            }
        } else if pkt == "g" {
            let h = &machine.harts[hart_idx];
            let mut out = String::with_capacity(33 * 16);
            for &r in &h.x {
                push_le64(&mut out, r);
            }
            push_le64(&mut out, h.pc);
            self.send_packet(&out)?;
        } else if let Some(hex) = pkt.strip_prefix('G') {
            if let Some(bytes) = decode_hex(hex) {
                let h = &mut machine.harts[hart_idx];
                for (i, chunk) in bytes.chunks_exact(8).take(33).enumerate() {
                    let v = u64::from_le_bytes(chunk.try_into().unwrap());
                    if i < 32 {
                        if i != 0 {
                            h.x[i] = v;
                        }
                    } else {
                        h.pc = v;
                    }
                }
                self.send_packet("OK")?;
            } else {
                self.send_packet("E01")?;
            }
        } else if let Some(reg_hex) = pkt.strip_prefix('p') {
            if let Ok(reg) = usize::from_str_radix(reg_hex, 16) {
                let h = &machine.harts[hart_idx];
                if let Some(v) = read_reg(h, reg) {
                    let mut out = String::with_capacity(16);
                    push_le64(&mut out, v);
                    self.send_packet(&out)?;
                } else {
                    self.send_packet("E01")?;
                }
            } else {
                self.send_packet("E01")?;
            }
        } else if let Some(rest) = pkt.strip_prefix('P') {
            if let Some((reg_s, val_s)) = rest.split_once('=') {
                if let (Ok(reg), Some(bytes)) =
                    (usize::from_str_radix(reg_s, 16), decode_hex(val_s))
                {
                    let mut raw = [0u8; 8];
                    let n = bytes.len().min(8);
                    raw[..n].copy_from_slice(&bytes[..n]);
                    let v = u64::from_le_bytes(raw);
                    if write_reg(&mut machine.harts[hart_idx], reg, v) {
                        self.send_packet("OK")?;
                    } else {
                        self.send_packet("E01")?;
                    }
                } else {
                    self.send_packet("E01")?;
                }
            } else {
                self.send_packet("E01")?;
            }
        } else if let Some(rest) = pkt.strip_prefix('m') {
            if let Some((addr, len)) = parse_pair(rest) {
                let mut out = String::with_capacity(len as usize * 2);
                let mut ok = true;
                for i in 0..len {
                    let va = addr.wrapping_add(i);
                    let pa = machine.harts[hart_idx]
                        .debug_translate(&machine.bus, va)
                        .unwrap_or(va);
                    match machine.bus.read(pa, AccessSize::Byte) {
                        Ok(b) => out.push_str(&format!("{:02x}", b as u8)),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    self.send_packet(&out)?;
                } else {
                    self.send_packet("E14")?;
                }
            } else {
                self.send_packet("E01")?;
            }
        } else if let Some(rest) = pkt.strip_prefix('M') {
            if let Some((spec, hex)) = rest.split_once(':') {
                if let (Some((addr, len)), Some(bytes)) = (parse_pair(spec), decode_hex(hex)) {
                    if bytes.len() as u64 == len {
                        let mut ok = true;
                        for (i, &b) in bytes.iter().enumerate() {
                            let va = addr.wrapping_add(i as u64);
                            let pa = machine.harts[hart_idx]
                                .debug_translate(&machine.bus, va)
                                .unwrap_or(va);
                            if machine.bus.load_bytes(pa, &[b]).is_err() {
                                ok = false;
                                break;
                            }
                        }
                        for h in &mut machine.harts {
                            h.invalidate_icache();
                        }
                        self.send_packet(if ok { "OK" } else { "E14" })?;
                    } else {
                        self.send_packet("E01")?;
                    }
                } else {
                    self.send_packet("E01")?;
                }
            } else {
                self.send_packet("E01")?;
            }
        } else if let Some(rest) = pkt.strip_prefix("Z0,").or_else(|| pkt.strip_prefix("Z1,")) {
            if let Some((addr, _)) = parse_pair(rest) {
                for h in &mut machine.harts {
                    if !h.breakpoints.contains(&addr) {
                        h.breakpoints.push(addr);
                    }
                }
                self.send_packet("OK")?;
            } else {
                self.send_packet("E01")?;
            }
        } else if let Some(rest) = pkt.strip_prefix("z0,").or_else(|| pkt.strip_prefix("z1,")) {
            if let Some((addr, _)) = parse_pair(rest) {
                for h in &mut machine.harts {
                    h.breakpoints.retain(|&bp| bp != addr);
                }
                self.send_packet("OK")?;
            } else {
                self.send_packet("E01")?;
            }
        } else if let Some(rest) = pkt.strip_prefix('s') {
            if !rest.is_empty() {
                if let Ok(pc) = u64::from_str_radix(rest, 16) {
                    machine.harts[hart_idx].pc = pc;
                }
            }
            step_one(machine, hart_idx);
            self.send_packet("S05")?;
        } else if let Some(rest) = pkt.strip_prefix('c') {
            if !rest.is_empty() {
                if let Ok(pc) = u64::from_str_radix(rest, 16) {
                    machine.harts[hart_idx].pc = pc;
                }
            }
            step_past_breakpoint(machine);
            return Ok(GdbAction::Continue);
        } else if pkt == "D" {
            self.send_packet("OK")?;
            return Ok(GdbAction::Detach);
        } else if pkt == "k" {
            return Ok(GdbAction::Kill);
        } else {
            // Unsupported packet: empty reply per RSP spec.
            self.send_packet("")?;
        }
        Ok(GdbAction::Paused)
    }
}

fn step_one(machine: &mut Machine, hart_idx: usize) {
    machine.clint.update();
    let h = &mut machine.harts[hart_idx];
    h.hit_breakpoint = false;
    h.take_pending_interrupt();
    if !h.wfi {
        h.step(&machine.bus);
    }
}

fn step_past_breakpoint(machine: &mut Machine) {
    for h in &mut machine.harts {
        h.hit_breakpoint = false;
        if h.breakpoints.contains(&h.pc) {
            h.take_pending_interrupt();
            if !h.wfi {
                h.step(&machine.bus);
            }
        }
    }
}

fn read_reg(h: &rivemu_cpu_riscv::Hart, reg: usize) -> Option<u64> {
    match reg {
        0..=31 => Some(h.x[reg]),
        32 => Some(h.pc),
        33..=64 => Some(h.f[reg - 33]),
        65..=4160 => h.csr_read((reg - 65) as u16),
        _ => None,
    }
}

fn write_reg(h: &mut rivemu_cpu_riscv::Hart, reg: usize, val: u64) -> bool {
    match reg {
        0 => true,
        1..=31 => {
            h.x[reg] = val;
            true
        }
        32 => {
            h.pc = val;
            true
        }
        33..=64 => {
            h.f[reg - 33] = val;
            true
        }
        65..=4160 => h.csr_write((reg - 65) as u16, val).is_some(),
        _ => false,
    }
}

fn push_le64(out: &mut String, v: u64) {
    for b in v.to_le_bytes() {
        out.push_str(&format!("{b:02x}"));
    }
}

fn parse_pair(s: &str) -> Option<(u64, u64)> {
    let (a, b) = s.split_once(',')?;
    Some((
        u64::from_str_radix(a, 16).ok()?,
        u64::from_str_radix(b, 16).ok()?,
    ))
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rivemu_machine::MachineConfig;

    fn send_and_recv(stream: &mut TcpStream, cmd: &str) -> String {
        let sum: u8 = cmd.bytes().fold(0u8, u8::wrapping_add);
        write!(stream, "${cmd}#{sum:02x}").unwrap();
        stream.flush().unwrap();
        let mut buf = Vec::new();
        let mut b = [0u8; 1];
        loop {
            stream.read_exact(&mut b).unwrap();
            if b[0] == b'$' {
                break;
            }
        }
        loop {
            stream.read_exact(&mut b).unwrap();
            if b[0] == b'#' {
                break;
            }
            buf.push(b[0]);
        }
        let mut csum = [0u8; 2];
        stream.read_exact(&mut csum).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn spec_parsing() {
        assert_eq!(parse_tcp_spec("tcp::1234").unwrap(), "127.0.0.1:1234");
        assert_eq!(parse_tcp_spec(":4321").unwrap(), "127.0.0.1:4321");
        assert_eq!(
            parse_tcp_spec("localhost:9000,server,nowait").unwrap(),
            "127.0.0.1:9000"
        );
        assert!(parse_tcp_spec("invalid").is_err());
    }

    #[test]
    fn rsp_session_step_break_and_regs() {
        let mut machine = Machine::new(MachineConfig::default()).unwrap();
        // Place two instructions at DRAM_BASE (0x8000_0000):
        //   addi a0, zero, 42   (0x02a0_0513)
        //   addi a0, a0, 1      (0x0015_0513)
        let code: [u8; 8] = [0x13, 0x05, 0xa0, 0x02, 0x13, 0x05, 0x15, 0x00];
        machine.bus.load_bytes(0x8000_0000, &code).unwrap();
        machine.harts[0].reset(0x8000_0000);

        let mut server = GdbServer::bind("tcp::0").unwrap();
        let addr = server.local_addr().unwrap();

        let handle = std::thread::spawn(move || {
            let mut client = TcpStream::connect(addr).unwrap();
            assert_eq!(send_and_recv(&mut client, "QStartNoAckMode"), "OK");
            assert_eq!(send_and_recv(&mut client, "?"), "S05");
            // Read PC (reg 32 = 0x20): should be 0x8000_0000 in LE hex.
            assert_eq!(send_and_recv(&mut client, "p20"), "0000008000000000");
            // Single-step one instruction: a0 (reg 10 = 0x0a) becomes 42 (0x2a).
            assert_eq!(send_and_recv(&mut client, "s"), "S05");
            assert_eq!(send_and_recv(&mut client, "p0a"), "2a00000000000000");
            // Read 4 bytes of memory at 0x8000_0004.
            assert_eq!(send_and_recv(&mut client, "m80000004,4"), "13051500");
            // Set a breakpoint at 0x8000_0008 and continue.
            assert_eq!(send_and_recv(&mut client, "Z0,80000008,4"), "OK");
            write!(client, "$c#63").unwrap();
            client.flush().unwrap();
            client
        });

        let action = server.handle_paused(&mut machine).unwrap();
        assert_eq!(action, GdbAction::Continue);
        // Run slice: hart should execute the second instruction and stop at breakpoint 0x8000_0008.
        machine.run_slice();
        assert!(machine.harts[0].hit_breakpoint);
        assert_eq!(machine.harts[0].pc, 0x8000_0008);
        assert_eq!(machine.harts[0].x[10], 43);

        let mut client = handle.join().unwrap();
        server.notify_stop().unwrap();
        // Detach cleanly.
        let handle2 = std::thread::spawn(move || {
            // Read the S05 stop packet first, then send D.
            let mut b = [0u8; 7];
            client.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"$S05#b8");
            assert_eq!(send_and_recv(&mut client, "D"), "OK");
        });
        assert_eq!(
            server.handle_paused(&mut machine).unwrap(),
            GdbAction::Detach
        );
        handle2.join().unwrap();
    }
}
