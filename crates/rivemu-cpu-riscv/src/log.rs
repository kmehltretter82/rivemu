//! Diagnostic logging (`-d` / `-D`).

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::decode::Insn;
use crate::hart::{Hart, Priv};

/// Bitmask of enabled `-d` log items.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogFlags(pub u32);

impl LogFlags {
    /// `in_asm`: log decoded instructions when first fetched into the icache.
    pub const IN_ASM: u32 = 1 << 0;
    /// `exec`: log PC before each executed instruction.
    pub const EXEC: u32 = 1 << 1;
    /// `cpu`: dump integer registers and key CSRs before each instruction.
    pub const CPU: u32 = 1 << 2;
    /// `int`: log traps (exceptions and interrupts).
    pub const INT: u32 = 1 << 3;
    /// `mmu`: log page-table walks and TLB flushes.
    pub const MMU: u32 = 1 << 4;
    /// `guest_errors`: log invalid guest operations (bus faults, illegal CSRs).
    pub const GUEST_ERRORS: u32 = 1 << 5;
    /// `unimp`: log unimplemented functionality.
    pub const UNIMP: u32 = 1 << 6;
    /// `sbi`: log Supervisor Binary Interface (SBI) calls.
    pub const SBI: u32 = 1 << 7;

    /// Parses a list of `-d` item names into flags.
    pub fn from_items<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut bits = 0;
        for item in items {
            bits |= match item.as_ref() {
                "in_asm" => Self::IN_ASM,
                "exec" => Self::EXEC,
                "cpu" => Self::CPU,
                "int" => Self::INT,
                "mmu" => Self::MMU,
                "guest_errors" => Self::GUEST_ERRORS,
                "unimp" => Self::UNIMP,
                "sbi" => Self::SBI,
                _ => 0,
            };
        }
        Self(bits)
    }

    /// Whether all flags are clear.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether `mask` is enabled.
    #[inline]
    pub const fn has(self, mask: u32) -> bool {
        self.0 & mask != 0
    }
}

/// Thread-safe sink for `-d` diagnostic output.
pub struct Logger {
    flags: LogFlags,
    sink: Mutex<Box<dyn Write + Send>>,
    symbols: Mutex<Vec<(u64, String)>>,
}

impl fmt::Debug for Logger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logger")
            .field("flags", &self.flags)
            .finish()
    }
}

impl Logger {
    /// Creates a logger writing to stderr.
    pub fn stderr(flags: LogFlags) -> Arc<Self> {
        Arc::new(Self {
            flags,
            sink: Mutex::new(Box::new(io::stderr())),
            symbols: Mutex::new(Vec::new()),
        })
    }

    /// Creates a logger writing to `path` (`-D`).
    pub fn file(flags: LogFlags, path: &Path) -> io::Result<Arc<Self>> {
        let f = std::fs::File::create(path)?;
        Ok(Arc::new(Self {
            flags,
            sink: Mutex::new(Box::new(io::BufWriter::new(f))),
            symbols: Mutex::new(Vec::new()),
        }))
    }

    /// Creates a logger writing to a custom writer (used in tests).
    pub fn custom(flags: LogFlags, writer: Box<dyn Write + Send>) -> Arc<Self> {
        Arc::new(Self {
            flags,
            sink: Mutex::new(writer),
            symbols: Mutex::new(Vec::new()),
        })
    }

    /// Loads an ELF symbol table (`name -> address`) for symbolicated trap and
    /// trace output.
    pub fn set_symbols(&self, map: &HashMap<String, u64>) {
        if let Ok(mut syms) = self.symbols.lock() {
            syms.clear();
            syms.extend(
                map.iter()
                    .filter(|&(_, &addr)| addr != 0)
                    .map(|(name, &addr)| (addr, name.clone())),
            );
            syms.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        }
    }

    /// Resolves `addr` to the nearest preceding symbol `(name, offset)` within
    /// 1 MiB.
    pub fn resolve_symbol(&self, addr: u64) -> Option<(String, u64)> {
        let syms = self.symbols.lock().ok()?;
        if syms.is_empty() {
            return None;
        }
        let idx = syms.partition_point(|(a, _)| *a <= addr);
        let (sym_addr, name) = syms.get(idx.checked_sub(1)?)?;
        let offset = addr - *sym_addr;
        (offset < 0x10_0000).then(|| (name.clone(), offset))
    }

    /// Formats an address with optional `<symbol+offset>` annotation.
    pub fn fmt_addr(&self, addr: u64) -> String {
        match self.resolve_symbol(addr) {
            Some((name, 0)) => format!("{addr:#018x} <{name}>"),
            Some((name, off)) => format!("{addr:#018x} <{name}+{off:#x}>"),
            None => format!("{addr:#018x}"),
        }
    }

    /// Enabled log categories.
    #[inline]
    pub fn flags(&self) -> LogFlags {
        self.flags
    }

    /// Writes a formatted log line.
    pub fn log(&self, args: fmt::Arguments<'_>) {
        if let Ok(mut w) = self.sink.lock() {
            let _ = w.write_fmt(args);
            let _ = w.flush();
        }
    }
}

impl Hart {
    /// Attaches a diagnostic logger to this hart.
    pub fn set_logger(&mut self, logger: Option<Arc<Logger>>) {
        self.log_flags = logger.as_ref().map_or(LogFlags::default(), |l| l.flags());
        self.logger = logger;
    }

    #[cold]
    pub(crate) fn log_in_asm(&self, pc: u64, pa: u64, insn: &Insn) {
        if let Some(l) = &self.logger {
            l.log(format_args!(
                "IN: [hart {}] va={pc:#018x} pa={pa:#012x}: raw={:#010x} {:?} rd={} rs1={} rs2={} imm={}\n",
                self.id, insn.raw, insn.op, insn.rd, insn.rs1, insn.rs2, insn.imm
            ));
        }
    }

    #[cold]
    pub(crate) fn log_exec_cpu(&self) {
        let Some(l) = &self.logger else { return };
        if self.log_flags.has(LogFlags::EXEC) {
            l.log(format_args!(
                "Trace {}: pc={:#018x} prv={:?}\n",
                self.id, self.pc, self.prv
            ));
        }
        if self.log_flags.has(LogFlags::CPU) {
            l.log(format_args!(
                "CPU {}: pc={:#018x} mstatus={:#018x} satp={:#018x}\n\
                 x00={:#018x} ra ={:#018x} sp ={:#018x} gp ={:#018x}\n\
                 tp ={:#018x} t0 ={:#018x} t1 ={:#018x} t2 ={:#018x}\n\
                 s0 ={:#018x} s1 ={:#018x} a0 ={:#018x} a1 ={:#018x}\n\
                 a2 ={:#018x} a3 ={:#018x} a4 ={:#018x} a5 ={:#018x}\n",
                self.id,
                self.pc,
                self.csr.mstatus,
                self.csr.satp,
                self.x[0],
                self.x[1],
                self.x[2],
                self.x[3],
                self.x[4],
                self.x[5],
                self.x[6],
                self.x[7],
                self.x[8],
                self.x[9],
                self.x[10],
                self.x[11],
                self.x[12],
                self.x[13],
                self.x[14],
                self.x[15],
            ));
        }
    }

    #[cold]
    pub(crate) fn log_trap(
        &self,
        code: u64,
        interrupt: bool,
        tval: u64,
        old_pc: u64,
        old_prv: Priv,
    ) {
        if let Some(l) = &self.logger {
            let kind = if interrupt { "irq" } else { "exc" };
            let epc = l.fmt_addr(old_pc);
            let ra = l.fmt_addr(self.x[1]);
            l.log(format_args!(
                "INT [hart {}]: {kind} code={code:#x} epc={epc} ({old_prv:?}) tval={tval:#018x} ra={ra} -> pc={:#018x} ({:?})\n",
                self.id, self.pc, self.prv
            ));
        }
    }

    /// Logs a serviced SBI call when `-d sbi` is enabled.
    #[cold]
    pub fn log_sbi(&self, eid: u64, fid: u64, a0: u64, a1: u64, err: u64, val: u64) {
        if !self.log_flags.has(LogFlags::SBI) {
            return;
        }
        if let Some(l) = &self.logger {
            l.log(format_args!(
                "SBI [hart {}]: eid={eid:#x} fid={fid:#x} a0={a0:#x} a1={a1:#x} -> err={err:#x} val={val:#x}\n",
                self.id
            ));
        }
    }

    #[cold]
    pub(crate) fn log_mmu(&self, msg: fmt::Arguments<'_>) {
        if let Some(l) = &self.logger {
            l.log(format_args!("MMU [hart {}]: {msg}\n", self.id));
        }
    }

    #[cold]
    pub(crate) fn log_guest_error(&self, msg: fmt::Arguments<'_>) {
        if let Some(l) = &self.logger {
            l.log(format_args!(
                "GUEST_ERROR [hart {} pc={:#x}]: {msg}\n",
                self.id, self.pc
            ));
        }
    }
}
