//! A RISC-V hart: architectural state, interpreter loop, CSRs and traps.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rivemu_core::{AccessSize, Timebase};
use rivemu_mem::AddressSpace;

use crate::csr::{self, Csrs, envcfg, irq, status};
use crate::decode::{Insn, Op, amo, decode, decode_compressed};
use crate::log::{LogFlags, Logger};
use crate::mmu::{Access, Tlb};

/// Exception cause codes.
#[allow(missing_docs)]
pub mod cause {
    pub const INSN_MISALIGNED: u64 = 0;
    pub const INSN_ACCESS: u64 = 1;
    pub const ILLEGAL_INSN: u64 = 2;
    pub const BREAKPOINT: u64 = 3;
    pub const LOAD_MISALIGNED: u64 = 4;
    pub const LOAD_ACCESS: u64 = 5;
    pub const STORE_MISALIGNED: u64 = 6;
    pub const STORE_ACCESS: u64 = 7;
    pub const ECALL_U: u64 = 8;
    pub const ECALL_S: u64 = 9;
    pub const ECALL_M: u64 = 11;
    pub const INSN_PAGE_FAULT: u64 = 12;
    pub const LOAD_PAGE_FAULT: u64 = 13;
    pub const STORE_PAGE_FAULT: u64 = 15;
}

/// A synchronous exception raised by an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exception {
    /// Cause code (see [`cause`]).
    pub cause: u64,
    /// Value for `xtval`.
    pub tval: u64,
}

impl Exception {
    /// Creates an exception.
    #[inline]
    pub const fn new(cause: u64, tval: u64) -> Self {
        Self { cause, tval }
    }
}

/// Privilege level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priv {
    /// User mode.
    User = 0,
    /// Supervisor mode.
    Supervisor = 1,
    /// Machine mode.
    Machine = 3,
}

impl Priv {
    fn from_bits(v: u64) -> Self {
        match v & 3 {
            0 => Self::User,
            1 => Self::Supervisor,
            _ => Self::Machine,
        }
    }
}

/// Interrupt lines driven by platform devices (CLINT, PLIC) into one hart.
///
/// Holds the hardware-controlled bits of `mip` (MSIP, MTIP, MEIP, SEIP).
#[derive(Debug, Default)]
pub struct HartIrqs {
    pending: AtomicU64,
}

impl HartIrqs {
    /// Creates a new set of lines, all de-asserted.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Sets or clears `mip` bit `bit` (e.g. [`irq::MTI`]).
    pub fn set(&self, bit: u64, level: bool) {
        if level {
            self.pending.fetch_or(1 << bit, Ordering::Relaxed);
        } else {
            self.pending.fetch_and(!(1 << bit), Ordering::Relaxed);
        }
    }

    /// Current hardware pending bits.
    #[inline]
    pub fn get(&self) -> u64 {
        self.pending.load(Ordering::Relaxed)
    }
}

const ICACHE_SIZE: usize = 8192;

#[derive(Clone, Copy, Debug)]
struct IcacheEntry {
    /// `(pa << 16) | icache_gen` (0 means invalid).
    tag: u64,
    insn: Insn,
}

impl Default for IcacheEntry {
    fn default() -> Self {
        Self {
            tag: 0,
            insn: Insn::ILLEGAL,
        }
    }
}

/// One RISC-V hart.
pub struct Hart {
    /// Hart ID (`mhartid`).
    pub id: u64,
    /// Program counter.
    pub pc: u64,
    /// Integer registers (`x[0]` is always zero).
    pub x: [u64; 32],
    /// Floating-point registers (raw bits, NaN-boxed singles).
    pub f: [u64; 32],
    /// Current privilege level.
    pub prv: Priv,
    /// CSR state.
    pub csr: Csrs,
    /// Hardware interrupt lines.
    pub irqs: Arc<HartIrqs>,
    /// Waiting for an interrupt (`wfi`).
    pub wfi: bool,
    timebase: Arc<dyn Timebase>,
    /// LR reservation: (physical address, loaded value).
    reservation: Option<(u64, u64)>,
    pub(crate) tlb: Tlb,
    icache: Box<[IcacheEntry; ICACHE_SIZE]>,
    icache_gen: u16,
    /// Cached instruction-fetch page: `(pc >> 12) + 1` (0 = invalid).
    fetch_vpn: u64,
    /// Physical base (`pa & !0xfff`) of [`Self::fetch_vpn`].
    fetch_pa_page: u64,
    /// Re-check interrupts before the next instruction.
    check_irq: bool,
    /// Yield the current scheduling slice early (e.g. on `pause` or IPI send).
    yield_slice: bool,
    /// The current instruction wrote `minstret` (suppresses its increment).
    instret_written: bool,
    /// Active `-d` flags (`0` when logging is disabled).
    pub(crate) log_flags: LogFlags,
    pub(crate) logger: Option<Arc<Logger>>,
    /// Active debugger PC breakpoints.
    pub breakpoints: Vec<u64>,
    /// Set by [`Self::run`] when execution stops on a breakpoint.
    pub hit_breakpoint: bool,
}

impl std::fmt::Debug for Hart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hart")
            .field("id", &self.id)
            .field("pc", &format_args!("{:#x}", self.pc))
            .field("prv", &self.prv)
            .finish_non_exhaustive()
    }
}

#[inline]
fn sext32(v: u64) -> u64 {
    v as i32 as i64 as u64
}

impl Hart {
    /// Creates a hart in its reset state (M-mode, `pc` = 0).
    pub fn new(id: u64, timebase: Arc<dyn Timebase>) -> Self {
        Self {
            id,
            pc: 0,
            x: [0; 32],
            f: [0; 32],
            prv: Priv::Machine,
            csr: Csrs::default(),
            irqs: HartIrqs::new(),
            wfi: false,
            timebase,
            reservation: None,
            tlb: Tlb::default(),
            icache: Box::new([IcacheEntry::default(); ICACHE_SIZE]),
            icache_gen: 1,
            fetch_vpn: 0,
            fetch_pa_page: 0,
            check_irq: true,
            yield_slice: false,
            instret_written: false,
            log_flags: LogFlags::default(),
            logger: None,
            breakpoints: Vec::new(),
            hit_breakpoint: false,
        }
    }

    /// Resets architectural state and starts execution at `pc`.
    pub fn reset(&mut self, pc: u64) {
        self.pc = pc;
        self.x = [0; 32];
        self.f = [0; 32];
        self.prv = Priv::Machine;
        self.csr = Csrs::default();
        self.wfi = false;
        self.reservation = None;
        self.flush_tlb();
        self.flush_icache();
        self.check_irq = true;
        self.yield_slice = false;
        self.hit_breakpoint = false;
    }

    /// Flushes the decoded-instruction cache (e.g. after debugger memory writes).
    #[inline]
    pub fn invalidate_icache(&mut self) {
        self.flush_icache();
    }

    #[inline]
    fn flush_tlb(&mut self) {
        self.tlb.flush();
        self.fetch_vpn = 0;
    }

    #[inline]
    fn flush_icache(&mut self) {
        self.icache_gen = self.icache_gen.wrapping_add(1);
        if self.icache_gen == 0 {
            self.icache.fill(IcacheEntry::default());
            self.icache_gen = 1;
        }
    }

    /// Number of retired instructions.
    pub fn instret(&self) -> u64 {
        self.csr.minstret
    }

    #[inline]
    pub(crate) fn set_x(&mut self, rd: u8, v: u64) {
        if rd != 0 {
            self.x[rd as usize] = v;
        }
    }

    // ---------------------------------------------------------------------
    // Run loop
    // ---------------------------------------------------------------------

    /// Executes up to `budget` instructions. Returns the number of
    /// instructions attempted (including those that trapped). Returns early
    /// (possibly with 0) when the hart is waiting for an interrupt or hits a
    /// debugger breakpoint.
    pub fn run(&mut self, bus: &AddressSpace, budget: u64) -> u64 {
        self.hit_breakpoint = false;
        self.yield_slice = false;
        self.take_pending_interrupt();
        if self.wfi {
            return 0;
        }
        let has_bp = !self.breakpoints.is_empty();
        let mut n = 0;
        while n < budget {
            if has_bp && self.breakpoints.contains(&self.pc) {
                self.hit_breakpoint = true;
                break;
            }
            self.step(bus);
            n += 1;
            if self.check_irq {
                self.take_pending_interrupt();
            }
            if self.wfi || self.yield_slice {
                break;
            }
        }
        self.reservation = None;
        n
    }

    /// Executes a single instruction (or takes the resulting trap).
    pub fn step(&mut self, bus: &AddressSpace) {
        if self.log_flags.has(LogFlags::EXEC | LogFlags::CPU) {
            self.log_exec_cpu();
        }
        let result = self.fetch(bus).and_then(|insn| self.execute(bus, &insn));
        match result {
            Ok(()) => {
                // An explicit write to minstret suppresses that instruction's increment.
                if self.csr.mcountinhibit & 4 == 0 && !std::mem::take(&mut self.instret_written) {
                    self.csr.minstret = self.csr.minstret.wrapping_add(1);
                }
            }
            Err(e) => self.trap(e.cause, false, e.tval),
        }
    }

    /// Takes the highest-priority pending and enabled interrupt, if any.
    /// Also wakes the hart from `wfi` when any enabled interrupt is pending.
    pub fn take_pending_interrupt(&mut self) {
        self.check_irq = false;
        let pending = self.mip() & self.csr.mie;
        if pending == 0 {
            return;
        }
        self.wfi = false;
        if let Some(cause) = self.select_interrupt(pending) {
            self.trap(cause, true, 0);
        }
    }

    fn select_interrupt(&self, pending: u64) -> Option<u64> {
        let mstatus = self.csr.mstatus;
        let m_enabled = self.prv < Priv::Machine || mstatus & status::MIE != 0;
        let s_enabled = self.prv < Priv::Supervisor
            || (self.prv == Priv::Supervisor && mstatus & status::SIE != 0);
        let m = pending & !self.csr.mideleg;
        let s = pending & self.csr.mideleg;
        if m_enabled && m != 0 {
            return irq::PRIORITY.into_iter().find(|&c| m & (1 << c) != 0);
        }
        if s_enabled && s != 0 {
            return irq::PRIORITY.into_iter().find(|&c| s & (1 << c) != 0);
        }
        None
    }

    /// Effective `mip`: software bits, device lines and the Sstc timer.
    pub fn mip(&self) -> u64 {
        let mut v = self.csr.mip | self.irqs.get();
        if self.csr.menvcfg & envcfg::STCE != 0 {
            v &= !irq::STIP;
            if self.timebase.now() >= self.csr.stimecmp {
                v |= irq::STIP;
            }
        }
        v
    }

    /// Enters a trap handler.
    pub fn trap(&mut self, code: u64, interrupt: bool, tval: u64) {
        let old_pc = self.pc;
        let old_prv = self.prv;
        self.reservation = None;
        self.wfi = false;
        let deleg = if interrupt {
            self.csr.mideleg
        } else {
            self.csr.medeleg
        };
        let cause = code | (u64::from(interrupt) << 63);
        let s = &mut self.csr.mstatus;
        if self.prv <= Priv::Supervisor && (deleg >> code) & 1 != 0 {
            self.csr.scause = cause;
            self.csr.sepc = self.pc;
            self.csr.stval = tval;
            let sie = *s & status::SIE != 0;
            *s &= !(status::SPIE | status::SIE | status::SPP);
            if sie {
                *s |= status::SPIE;
            }
            if self.prv == Priv::Supervisor {
                *s |= status::SPP;
            }
            self.prv = Priv::Supervisor;
            self.pc = trap_vector(self.csr.stvec, code, interrupt);
        } else {
            self.csr.mcause = cause;
            self.csr.mepc = self.pc;
            self.csr.mtval = tval;
            let mie = *s & status::MIE != 0;
            *s &= !(status::MPIE | status::MIE | status::MPP);
            if mie {
                *s |= status::MPIE;
            }
            *s |= (self.prv as u64) << status::MPP_SHIFT;
            self.prv = Priv::Machine;
            self.pc = trap_vector(self.csr.mtvec, code, interrupt);
        }
        self.fetch_vpn = 0;
        self.check_irq = true;
        if self.log_flags.has(LogFlags::INT) {
            self.log_trap(code, interrupt, tval, old_pc, old_prv);
        }
    }

    // ---------------------------------------------------------------------
    // Fetch / memory
    // ---------------------------------------------------------------------

    #[inline]
    fn fetch(&mut self, bus: &AddressSpace) -> Result<Insn, Exception> {
        let pc = self.pc;
        if pc & 0xfff <= 0xffc {
            let vpn_tag = (pc >> 12) + 1;
            let pa = if self.fetch_vpn == vpn_tag {
                self.fetch_pa_page | (pc & 0xfff)
            } else {
                let pa = self.translate(bus, pc, Access::Fetch)?;
                self.fetch_vpn = vpn_tag;
                self.fetch_pa_page = pa & !0xfff;
                pa
            };
            if pa >> 48 == 0 {
                let idx = ((pa >> 1) as usize) & (ICACHE_SIZE - 1);
                let tag = (pa << 16) | u64::from(self.icache_gen);
                let entry = &self.icache[idx];
                if entry.tag == tag {
                    return Ok(entry.insn);
                }
                let insn = self.fetch_decode_same_page(bus, pc, pa)?;
                if self.log_flags.has(LogFlags::IN_ASM) {
                    self.log_in_asm(pc, pa, &insn);
                }
                self.icache[idx] = IcacheEntry { tag, insn };
                return Ok(insn);
            }
        }
        let insn = self.fetch_slow(bus, pc)?;
        if self.log_flags.has(LogFlags::IN_ASM) {
            self.log_in_asm(pc, pc, &insn);
        }
        Ok(insn)
    }

    #[cold]
    fn fetch_decode_same_page(
        &self,
        bus: &AddressSpace,
        pc: u64,
        pa: u64,
    ) -> Result<Insn, Exception> {
        let lo = bus
            .read(pa, AccessSize::Half)
            .map_err(|_| Exception::new(cause::INSN_ACCESS, pc))? as u16;
        if lo & 3 != 3 {
            return Ok(decode_compressed(lo));
        }
        let hi = bus
            .read(pa + 2, AccessSize::Half)
            .map_err(|_| Exception::new(cause::INSN_ACCESS, pc + 2))?;
        Ok(decode(u32::from(lo) | (hi as u32) << 16))
    }

    #[cold]
    fn fetch_slow(&mut self, bus: &AddressSpace, pc: u64) -> Result<Insn, Exception> {
        let pa = self.translate(bus, pc, Access::Fetch)?;
        let lo = bus
            .read(pa, AccessSize::Half)
            .map_err(|_| Exception::new(cause::INSN_ACCESS, pc))? as u16;
        if lo & 3 != 3 {
            return Ok(decode_compressed(lo));
        }
        let pa_hi = if pc & 0xfff == 0xffe {
            self.translate(bus, pc + 2, Access::Fetch)?
        } else {
            pa + 2
        };
        let hi = bus
            .read(pa_hi, AccessSize::Half)
            .map_err(|_| Exception::new(cause::INSN_ACCESS, pc + 2))?;
        Ok(decode(u32::from(lo) | (hi as u32) << 16))
    }

    /// Virtual-memory load (handles page-crossing misaligned accesses).
    #[inline]
    pub(crate) fn load(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        size: AccessSize,
    ) -> Result<u64, Exception> {
        let n = size.bytes();
        if (va & 0xfff) + n > 0x1000 {
            return self.load_page_cross(bus, va, n);
        }
        let pa = self.translate(bus, va, Access::Load)?;
        bus.read(pa, size).map_err(|e| {
            if self.log_flags.has(LogFlags::GUEST_ERRORS) {
                self.log_guest_error(format_args!("load fault at va={va:#x} pa={pa:#x}: {e}"));
            }
            Exception::new(cause::LOAD_ACCESS, va)
        })
    }

    #[cold]
    fn load_page_cross(&mut self, bus: &AddressSpace, va: u64, n: u64) -> Result<u64, Exception> {
        let mut v = 0;
        for i in 0..n {
            v |= self.load(bus, va.wrapping_add(i), AccessSize::Byte)? << (8 * i);
        }
        Ok(v)
    }

    /// Virtual-memory store (handles page-crossing misaligned accesses).
    #[inline]
    pub(crate) fn store(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        size: AccessSize,
        value: u64,
    ) -> Result<(), Exception> {
        let n = size.bytes();
        if (va & 0xfff) + n > 0x1000 {
            return self.store_page_cross(bus, va, n, value);
        }
        let pa = self.translate(bus, va, Access::Store)?;
        self.invalidate_reservation(pa);
        if pa < 0x8000_0000 {
            self.check_irq = true;
            if (0x0200_0000..0x0200_4000).contains(&pa) && value & 1 != 0 {
                self.yield_slice = true;
            }
        }
        bus.write(pa, size, value).map_err(|e| {
            if self.log_flags.has(LogFlags::GUEST_ERRORS) {
                self.log_guest_error(format_args!("store fault at va={va:#x} pa={pa:#x}: {e}"));
            }
            Exception::new(cause::STORE_ACCESS, va)
        })
    }

    #[cold]
    fn store_page_cross(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        n: u64,
        value: u64,
    ) -> Result<(), Exception> {
        // Translate both pages first so a fault leaves memory untouched.
        self.translate(bus, va, Access::Store)?;
        self.translate(bus, (va | 0xfff).wrapping_add(1), Access::Store)?;
        for i in 0..n {
            self.store(bus, va.wrapping_add(i), AccessSize::Byte, value >> (8 * i))?;
        }
        Ok(())
    }

    #[inline]
    fn invalidate_reservation(&mut self, pa: u64) {
        if let Some((r, _)) = self.reservation {
            if r & !7 == pa & !7 {
                self.reservation = None;
            }
        }
    }

    // ---------------------------------------------------------------------
    // Execute
    // ---------------------------------------------------------------------

    #[inline]
    fn illegal(insn: &Insn) -> Exception {
        Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw))
    }

    fn execute(&mut self, bus: &AddressSpace, insn: &Insn) -> Result<(), Exception> {
        let pc = self.pc;
        let next = pc.wrapping_add(u64::from(insn.len));
        let rs1 = self.x[insn.rs1 as usize];
        let rs2 = self.x[insn.rs2 as usize];
        let imm = insn.imm as u64;
        let rd = insn.rd;
        let mut new_pc = next;

        macro_rules! branch {
            ($cond:expr) => {
                if $cond {
                    new_pc = pc.wrapping_add(imm);
                }
            };
        }

        match insn.op {
            Op::Illegal => return Err(Self::illegal(insn)),
            Op::Lui => self.set_x(rd, imm),
            Op::Auipc => self.set_x(rd, pc.wrapping_add(imm)),
            Op::Jal => {
                self.set_x(rd, next);
                new_pc = pc.wrapping_add(imm);
            }
            Op::Jalr => {
                new_pc = rs1.wrapping_add(imm) & !1;
                self.set_x(rd, next);
            }
            Op::Beq => branch!(rs1 == rs2),
            Op::Bne => branch!(rs1 != rs2),
            Op::Blt => branch!((rs1 as i64) < (rs2 as i64)),
            Op::Bge => branch!((rs1 as i64) >= (rs2 as i64)),
            Op::Bltu => branch!(rs1 < rs2),
            Op::Bgeu => branch!(rs1 >= rs2),
            Op::Lb | Op::Lh | Op::Lw | Op::Ld | Op::Lbu | Op::Lhu | Op::Lwu => {
                let addr = rs1.wrapping_add(imm);
                let v = match insn.op {
                    Op::Lb => self.load(bus, addr, AccessSize::Byte)? as i8 as u64,
                    Op::Lh => self.load(bus, addr, AccessSize::Half)? as i16 as u64,
                    Op::Lw => self.load(bus, addr, AccessSize::Word)? as i32 as u64,
                    Op::Ld => self.load(bus, addr, AccessSize::Double)?,
                    Op::Lbu => self.load(bus, addr, AccessSize::Byte)?,
                    Op::Lhu => self.load(bus, addr, AccessSize::Half)?,
                    _ => self.load(bus, addr, AccessSize::Word)?,
                };
                self.set_x(rd, v);
            }
            Op::Sb => self.store(bus, rs1.wrapping_add(imm), AccessSize::Byte, rs2)?,
            Op::Sh => self.store(bus, rs1.wrapping_add(imm), AccessSize::Half, rs2)?,
            Op::Sw => self.store(bus, rs1.wrapping_add(imm), AccessSize::Word, rs2)?,
            Op::Sd => self.store(bus, rs1.wrapping_add(imm), AccessSize::Double, rs2)?,
            Op::Addi => self.set_x(rd, rs1.wrapping_add(imm)),
            Op::Slti => self.set_x(rd, u64::from((rs1 as i64) < i64::from(insn.imm))),
            Op::Sltiu => self.set_x(rd, u64::from(rs1 < imm)),
            Op::Xori => self.set_x(rd, rs1 ^ imm),
            Op::Ori => self.set_x(rd, rs1 | imm),
            Op::Andi => self.set_x(rd, rs1 & imm),
            Op::Slli => self.set_x(rd, rs1 << (imm & 63)),
            Op::Srli => self.set_x(rd, rs1 >> (imm & 63)),
            Op::Srai => self.set_x(rd, ((rs1 as i64) >> (imm & 63)) as u64),
            Op::Add => self.set_x(rd, rs1.wrapping_add(rs2)),
            Op::Sub => self.set_x(rd, rs1.wrapping_sub(rs2)),
            Op::Sll => self.set_x(rd, rs1 << (rs2 & 63)),
            Op::Slt => self.set_x(rd, u64::from((rs1 as i64) < (rs2 as i64))),
            Op::Sltu => self.set_x(rd, u64::from(rs1 < rs2)),
            Op::Xor => self.set_x(rd, rs1 ^ rs2),
            Op::Srl => self.set_x(rd, rs1 >> (rs2 & 63)),
            Op::Sra => self.set_x(rd, ((rs1 as i64) >> (rs2 & 63)) as u64),
            Op::Or => self.set_x(rd, rs1 | rs2),
            Op::And => self.set_x(rd, rs1 & rs2),
            Op::Addiw => self.set_x(rd, sext32(rs1.wrapping_add(imm))),
            Op::Slliw => self.set_x(rd, sext32(rs1 << (imm & 31))),
            Op::Srliw => self.set_x(rd, sext32(u64::from((rs1 as u32) >> (imm & 31)))),
            Op::Sraiw => self.set_x(rd, ((rs1 as i32) >> (imm & 31)) as i64 as u64),
            Op::Addw => self.set_x(rd, sext32(rs1.wrapping_add(rs2))),
            Op::Subw => self.set_x(rd, sext32(rs1.wrapping_sub(rs2))),
            Op::Sllw => self.set_x(rd, sext32(rs1 << (rs2 & 31))),
            Op::Srlw => self.set_x(rd, sext32(u64::from((rs1 as u32) >> (rs2 & 31)))),
            Op::Sraw => self.set_x(rd, ((rs1 as i32) >> (rs2 & 31)) as i64 as u64),
            Op::Fence => {
                std::sync::atomic::fence(Ordering::SeqCst);
                if insn.raw == 0x0100_000f {
                    // Zihintpause `pause` (`fence w, 0` in `cpu_relax()`).
                    self.yield_slice = true;
                }
            }
            Op::FenceI => self.flush_icache(),
            Op::Ecall => {
                let c = match self.prv {
                    Priv::User => cause::ECALL_U,
                    Priv::Supervisor => cause::ECALL_S,
                    Priv::Machine => cause::ECALL_M,
                };
                return Err(Exception::new(c, 0));
            }
            Op::Ebreak => return Err(Exception::new(cause::BREAKPOINT, pc)),

            // M extension.
            Op::Mul => self.set_x(rd, rs1.wrapping_mul(rs2)),
            Op::Mulh => self.set_x(
                rd,
                ((i128::from(rs1 as i64) * i128::from(rs2 as i64)) >> 64) as u64,
            ),
            Op::Mulhsu => self.set_x(
                rd,
                ((i128::from(rs1 as i64) * i128::from(rs2)) >> 64) as u64,
            ),
            Op::Mulhu => self.set_x(rd, ((u128::from(rs1) * u128::from(rs2)) >> 64) as u64),
            Op::Div => {
                let (a, b) = (rs1 as i64, rs2 as i64);
                let v = if b == 0 { -1 } else { a.wrapping_div(b) };
                self.set_x(rd, v as u64);
            }
            Op::Divu => self.set_x(rd, rs1.checked_div(rs2).unwrap_or(u64::MAX)),
            Op::Rem => {
                let (a, b) = (rs1 as i64, rs2 as i64);
                let v = if b == 0 { a } else { a.wrapping_rem(b) };
                self.set_x(rd, v as u64);
            }
            Op::Remu => self.set_x(rd, if rs2 == 0 { rs1 } else { rs1 % rs2 }),
            Op::Mulw => self.set_x(rd, sext32(rs1.wrapping_mul(rs2))),
            Op::Divw => {
                let (a, b) = (rs1 as i32, rs2 as i32);
                let v = if b == 0 { -1 } else { a.wrapping_div(b) };
                self.set_x(rd, v as i64 as u64);
            }
            Op::Divuw => {
                let (a, b) = (rs1 as u32, rs2 as u32);
                let v = a.checked_div(b).unwrap_or(u32::MAX);
                self.set_x(rd, sext32(u64::from(v)));
            }
            Op::Remw => {
                let (a, b) = (rs1 as i32, rs2 as i32);
                let v = if b == 0 { a } else { a.wrapping_rem(b) };
                self.set_x(rd, v as i64 as u64);
            }
            Op::Remuw => {
                let (a, b) = (rs1 as u32, rs2 as u32);
                let v = if b == 0 { a } else { a % b };
                self.set_x(rd, sext32(u64::from(v)));
            }

            // A extension.
            Op::Lr | Op::Sc | Op::Amo => self.exec_atomic(bus, insn, rs1, rs2)?,

            // Zicsr.
            Op::Csrrw | Op::Csrrs | Op::Csrrc | Op::Csrrwi | Op::Csrrsi | Op::Csrrci => {
                self.exec_csr(insn, rs1)?;
            }

            // Privileged.
            Op::Mret => {
                if self.prv != Priv::Machine {
                    return Err(Self::illegal(insn));
                }
                let s = self.csr.mstatus;
                let mpp = Priv::from_bits(s >> status::MPP_SHIFT);
                let mut n = s & !(status::MIE | status::MPP);
                if s & status::MPIE != 0 {
                    n |= status::MIE;
                }
                n |= status::MPIE;
                if mpp != Priv::Machine {
                    n &= !status::MPRV;
                }
                self.csr.mstatus = n;
                self.prv = mpp;
                self.reservation = None;
                self.fetch_vpn = 0;
                self.check_irq = true;
                new_pc = self.csr.mepc;
            }
            Op::Sret => {
                let s = self.csr.mstatus;
                if self.prv < Priv::Supervisor
                    || (self.prv == Priv::Supervisor && s & status::TSR != 0)
                {
                    return Err(Self::illegal(insn));
                }
                let spp = if s & status::SPP != 0 {
                    Priv::Supervisor
                } else {
                    Priv::User
                };
                let mut n = s & !(status::SIE | status::SPP | status::MPRV);
                if s & status::SPIE != 0 {
                    n |= status::SIE;
                }
                n |= status::SPIE;
                self.csr.mstatus = n;
                self.prv = spp;
                self.reservation = None;
                self.fetch_vpn = 0;
                self.check_irq = true;
                new_pc = self.csr.sepc;
            }
            Op::Wfi => {
                let s = self.csr.mstatus;
                if self.prv == Priv::User || (self.prv == Priv::Supervisor && s & status::TW != 0) {
                    return Err(Self::illegal(insn));
                }
                if self.mip() & self.csr.mie == 0 {
                    self.wfi = true;
                } else {
                    self.check_irq = true;
                    self.yield_slice = true;
                }
            }
            Op::SfenceVma => {
                if self.prv == Priv::User
                    || (self.prv == Priv::Supervisor && self.csr.mstatus & status::TVM != 0)
                {
                    return Err(Self::illegal(insn));
                }
                self.flush_tlb();
            }

            // F/D extensions.
            _ => self.exec_fp(bus, insn)?,
        }
        self.pc = new_pc;
        Ok(())
    }

    fn exec_atomic(
        &mut self,
        bus: &AddressSpace,
        insn: &Insn,
        addr: u64,
        src: u64,
    ) -> Result<(), Exception> {
        let size = if insn.dbl {
            AccessSize::Double
        } else {
            AccessSize::Word
        };
        let fix = |v: u64| if insn.dbl { v } else { sext32(v) };
        if addr & (size.bytes() - 1) != 0 {
            let c = if insn.op == Op::Lr {
                cause::LOAD_MISALIGNED
            } else {
                cause::STORE_MISALIGNED
            };
            return Err(Exception::new(c, addr));
        }
        match insn.op {
            Op::Lr => {
                let pa = self.translate(bus, addr, Access::Load)?;
                let v = bus
                    .read(pa, size)
                    .map_err(|_| Exception::new(cause::LOAD_ACCESS, addr))?;
                self.reservation = Some((pa, v));
                self.set_x(insn.rd, fix(v));
            }
            Op::Sc => {
                let pa = self.translate(bus, addr, Access::Store)?;
                let ok = match self.reservation.take() {
                    Some((r, v)) if r == pa => bus
                        .compare_exchange(pa, size, v, src)
                        .map_err(|_| Exception::new(cause::STORE_ACCESS, addr))?,
                    _ => false,
                };
                self.set_x(insn.rd, u64::from(!ok));
            }
            _ => {
                let pa = self.translate(bus, addr, Access::Store)?;
                let b = fix(src);
                let funct = insn.funct;
                let dbl = insn.dbl;
                let old = bus
                    .atomic_rmw(pa, size, |old_raw| {
                        let a = fix(old_raw);
                        match funct {
                            amo::SWAP => b,
                            amo::ADD => a.wrapping_add(b),
                            amo::XOR => a ^ b,
                            amo::AND => a & b,
                            amo::OR => a | b,
                            amo::MIN => (a as i64).min(b as i64) as u64,
                            amo::MAX => (a as i64).max(b as i64) as u64,
                            amo::MINU if dbl => a.min(b),
                            amo::MAXU if dbl => a.max(b),
                            amo::MINU => u64::from((a as u32).min(b as u32)),
                            _ => u64::from((a as u32).max(b as u32)),
                        }
                    })
                    .map_err(|_| Exception::new(cause::STORE_ACCESS, addr))?;
                self.invalidate_reservation(pa);
                self.set_x(insn.rd, fix(old));
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // CSRs
    // ---------------------------------------------------------------------

    fn exec_csr(&mut self, insn: &Insn, rs1_val: u64) -> Result<(), Exception> {
        let num = insn.imm as u16;
        let illegal = || Self::illegal(insn);
        let (src, writes) = match insn.op {
            Op::Csrrw => (rs1_val, true),
            Op::Csrrwi => (u64::from(insn.rs1), true),
            Op::Csrrs | Op::Csrrc => (rs1_val, insn.rs1 != 0),
            _ => (u64::from(insn.rs1), insn.rs1 != 0),
        };
        // Privilege and read-only checks.
        if (self.prv as u16) < (num >> 8) & 3 {
            return Err(illegal());
        }
        if writes && num >> 10 == 3 {
            return Err(illegal());
        }
        let reads = !matches!(insn.op, Op::Csrrw | Op::Csrrwi) || insn.rd != 0;
        let old = if reads || writes {
            self.csr_read(num).ok_or_else(illegal)?
        } else {
            0
        };
        if writes {
            // Per RISC-V Privileged Spec 3.1.9, RMW operations on `mip` use only
            // the software-writable bits as their input, not external IRQ lines.
            let base = if num == csr::MIP { self.csr.mip } else { old };
            let new = match insn.op {
                Op::Csrrw | Op::Csrrwi => src,
                Op::Csrrs | Op::Csrrsi => base | src,
                _ => base & !src,
            };
            self.csr_write(num, new).ok_or_else(illegal)?;
            self.check_irq = true;
        }
        self.set_x(insn.rd, old);
        Ok(())
    }

    fn fs_enabled(&self) -> bool {
        self.csr.mstatus & status::FS != 0
    }

    /// Whether counter `idx` (0 = cycle, 1 = time, 2 = instret, ...) is
    /// accessible at the current privilege level.
    fn counter_enabled(&self, idx: u16) -> bool {
        let bit = 1 << idx;
        match self.prv {
            Priv::Machine => true,
            Priv::Supervisor => self.csr.mcounteren & bit != 0,
            Priv::User => self.csr.mcounteren & self.csr.scounteren & bit != 0,
        }
    }

    /// Reads a CSR; `None` if it does not exist or is inaccessible.
    pub fn csr_read(&self, num: u16) -> Option<u64> {
        let c = &self.csr;
        Some(match num {
            csr::FFLAGS | csr::FRM | csr::FCSR if !self.fs_enabled() => return None,
            csr::FFLAGS => c.fflags,
            csr::FRM => c.frm,
            csr::FCSR => (c.frm << 5) | c.fflags,
            csr::CYCLE..=csr::HPMCOUNTER31 => {
                let idx = num - csr::CYCLE;
                if !self.counter_enabled(idx) {
                    return None;
                }
                match num {
                    csr::CYCLE | csr::INSTRET => c.minstret,
                    csr::TIME => self.timebase.now(),
                    _ => 0,
                }
            }
            csr::SSTATUS => self.read_mstatus() & status::S_READABLE,
            csr::SIE => c.mie & c.mideleg,
            csr::STVEC => c.stvec,
            csr::SCOUNTEREN => c.scounteren,
            csr::SENVCFG => c.senvcfg,
            csr::SSCRATCH => c.sscratch,
            csr::SEPC => c.sepc,
            csr::SCAUSE => c.scause,
            csr::STVAL => c.stval,
            csr::SIP => self.mip() & c.mideleg,
            csr::STIMECMP => {
                if !self.stimecmp_accessible() {
                    return None;
                }
                c.stimecmp
            }
            csr::SATP => {
                if self.prv == Priv::Supervisor && c.mstatus & status::TVM != 0 {
                    return None;
                }
                c.satp
            }
            csr::MVENDORID | csr::MARCHID | csr::MIMPID | csr::MCONFIGPTR => 0,
            csr::MHARTID => self.id,
            csr::MSTATUS => self.read_mstatus(),
            csr::MISA => csr::MISA_VALUE,
            csr::MEDELEG => c.medeleg,
            csr::MIDELEG => c.mideleg,
            csr::MIE => c.mie,
            csr::MTVEC => c.mtvec,
            csr::MCOUNTEREN => c.mcounteren,
            csr::MENVCFG => c.menvcfg,
            csr::MCOUNTINHIBIT => c.mcountinhibit,
            csr::MHPMEVENT3..=csr::MHPMEVENT31 => 0,
            csr::MSCRATCH => c.mscratch,
            csr::MEPC => c.mepc,
            csr::MCAUSE => c.mcause,
            csr::MTVAL => c.mtval,
            csr::MIP => self.mip(),
            csr::PMPCFG0..=csr::PMPCFG15 => {
                let n = (num - csr::PMPCFG0) as usize;
                if n % 2 == 1 {
                    return None;
                }
                let mut v = 0u64;
                for i in 0..8 {
                    let e = n * 4 + i;
                    if e < csr::PMP_ENTRIES {
                        v |= u64::from(c.pmpcfg[e]) << (8 * i);
                    }
                }
                v
            }
            csr::PMPADDR0..=csr::PMPADDR63 => {
                let n = (num - csr::PMPADDR0) as usize;
                c.pmpaddr.get(n).copied().unwrap_or(0)
            }
            csr::TSELECT..=csr::TDATA3 | csr::TINFO => 0,
            csr::MCYCLE | csr::MINSTRET => c.minstret,
            csr::MHPMCOUNTER3..=csr::MHPMCOUNTER31 => 0,
            _ => return None,
        })
    }

    fn stimecmp_accessible(&self) -> bool {
        match self.prv {
            Priv::Machine => true,
            Priv::Supervisor => {
                self.csr.menvcfg & envcfg::STCE != 0 && self.csr.mcounteren & 2 != 0
            }
            Priv::User => false,
        }
    }

    fn read_mstatus(&self) -> u64 {
        let s = self.csr.mstatus;
        if s & status::FS == status::FS {
            s | status::SD
        } else {
            s & !status::SD
        }
    }

    /// Writes a CSR; `None` if it does not exist or is inaccessible.
    pub fn csr_write(&mut self, num: u16, v: u64) -> Option<()> {
        match num {
            csr::FFLAGS | csr::FRM | csr::FCSR if !self.fs_enabled() => return None,
            csr::FFLAGS => {
                self.csr.fflags = v & 0x1f;
                self.csr.mstatus |= status::FS;
            }
            csr::FRM => {
                self.csr.frm = v & 7;
                self.csr.mstatus |= status::FS;
            }
            csr::FCSR => {
                self.csr.fflags = v & 0x1f;
                self.csr.frm = (v >> 5) & 7;
                self.csr.mstatus |= status::FS;
            }
            csr::SSTATUS => {
                let m = status::S_WRITABLE;
                self.csr.mstatus = (self.csr.mstatus & !m) | (v & m);
            }
            csr::SIE => {
                let m = self.csr.mideleg & irq::S_MASK;
                self.csr.mie = (self.csr.mie & !m) | (v & m);
            }
            csr::STVEC => self.csr.stvec = v & !2,
            csr::SCOUNTEREN => self.csr.scounteren = v & 0xffff_ffff,
            csr::SENVCFG => self.csr.senvcfg = v & envcfg::FIOM,
            csr::SSCRATCH => self.csr.sscratch = v,
            csr::SEPC => self.csr.sepc = v & !1,
            csr::SCAUSE => self.csr.scause = v,
            csr::STVAL => self.csr.stval = v,
            csr::SIP => {
                let m = self.csr.mideleg & irq::SSIP;
                self.csr.mip = (self.csr.mip & !m) | (v & m);
            }
            csr::STIMECMP => {
                if !self.stimecmp_accessible() {
                    return None;
                }
                self.csr.stimecmp = v;
            }
            csr::SATP => {
                if self.prv == Priv::Supervisor && self.csr.mstatus & status::TVM != 0 {
                    return None;
                }
                let mode = v >> 60;
                if matches!(mode, 0 | 8 | 9) {
                    // MODE, ASID (ASIDLEN = 16) and PPN cover all 64 bits.
                    self.csr.satp = v;
                    self.flush_tlb();
                }
            }
            csr::MVENDORID | csr::MARCHID | csr::MIMPID | csr::MHARTID | csr::MCONFIGPTR => {
                return None;
            }
            csr::MSTATUS => {
                let m = status::M_WRITABLE;
                let mut n = (self.csr.mstatus & !m) | (v & m);
                if (n >> status::MPP_SHIFT) & 3 == 2 {
                    // Reserved MPP value: keep the old one (WARL).
                    n = (n & !status::MPP) | (self.csr.mstatus & status::MPP);
                }
                self.csr.mstatus = n;
            }
            csr::MISA => {}
            csr::MEDELEG => self.csr.medeleg = v & csr::MEDELEG_MASK,
            csr::MIDELEG => self.csr.mideleg = v & irq::S_MASK,
            csr::MIE => self.csr.mie = v & irq::ALL,
            csr::MTVEC => self.csr.mtvec = v & !2,
            csr::MCOUNTEREN => self.csr.mcounteren = v & 0xffff_ffff,
            csr::MENVCFG => self.csr.menvcfg = v & (envcfg::FIOM | envcfg::STCE),
            csr::MCOUNTINHIBIT => self.csr.mcountinhibit = v & 0xffff_fffd,
            csr::MHPMEVENT3..=csr::MHPMEVENT31 => {}
            csr::MSCRATCH => self.csr.mscratch = v,
            csr::MEPC => self.csr.mepc = v & !1,
            csr::MCAUSE => self.csr.mcause = v,
            csr::MTVAL => self.csr.mtval = v,
            csr::MIP => {
                let mut m = irq::SSIP | irq::SEIP;
                if self.csr.menvcfg & envcfg::STCE == 0 {
                    m |= irq::STIP;
                }
                self.csr.mip = (self.csr.mip & !m) | (v & m);
            }
            csr::PMPCFG0..=csr::PMPCFG15 => {
                let n = (num - csr::PMPCFG0) as usize;
                if n % 2 == 1 {
                    return None;
                }
                for i in 0..8 {
                    let e = n * 4 + i;
                    if e >= csr::PMP_ENTRIES || self.csr.pmpcfg[e] & 0x80 != 0 {
                        continue;
                    }
                    let b = (v >> (8 * i)) as u8 & 0x9f;
                    // R=0, W=1 is reserved: ignore the write for this entry.
                    if b & 3 == 2 {
                        continue;
                    }
                    self.csr.pmpcfg[e] = b;
                }
            }
            csr::PMPADDR0..=csr::PMPADDR63 => {
                let n = (num - csr::PMPADDR0) as usize;
                if n < csr::PMP_ENTRIES && self.csr.pmpcfg[n] & 0x80 == 0 {
                    self.csr.pmpaddr[n] = v & ((1 << 54) - 1);
                }
            }
            csr::TSELECT..=csr::TDATA3 | csr::TINFO => {}
            csr::MCYCLE | csr::MINSTRET => {
                self.csr.minstret = v;
                self.instret_written = true;
            }
            csr::MHPMCOUNTER3..=csr::MHPMCOUNTER31 => {}
            _ => return None,
        }
        Some(())
    }
}

fn trap_vector(tvec: u64, code: u64, interrupt: bool) -> u64 {
    let base = tvec & !3;
    if interrupt && tvec & 1 != 0 {
        base + 4 * code
    } else {
        base
    }
}
