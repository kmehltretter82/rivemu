//! Diagnostic logging (`-d` / `-D`).

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
        })
    }

    /// Creates a logger writing to `path` (`-D`).
    pub fn file(flags: LogFlags, path: &Path) -> io::Result<Arc<Self>> {
        let f = std::fs::File::create(path)?;
        Ok(Arc::new(Self {
            flags,
            sink: Mutex::new(Box::new(io::BufWriter::new(f))),
        }))
    }

    /// Creates a logger writing to a custom writer (used in tests).
    pub fn custom(flags: LogFlags, writer: Box<dyn Write + Send>) -> Arc<Self> {
        Arc::new(Self {
            flags,
            sink: Mutex::new(writer),
        })
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
            l.log(format_args!(
                "INT [hart {}]: {kind} code={code:#x} epc={old_pc:#018x} ({old_prv:?}) tval={tval:#018x} -> pc={:#018x} ({:?})\n",
                self.id, self.pc, self.prv
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
