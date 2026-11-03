//! A RISC-V hart: architectural state, interpreter loop, CSRs and traps.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rivemu_core::{AccessSize, Timebase};
use rivemu_mem::{AddressSpace, RamView, RegionKind};

use crate::csr::{self, Csrs, envcfg, hstatus, irq, status};
use crate::decode::{Insn, Op, amo, decode, decode_compressed};
use crate::log::{LogFlags, Logger};
use crate::mmu::{Access, FAST_TLB_SLOTS, FastTlbSlot, PmpCache, Tlb};

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
    pub const DOUBLE_TRAP: u64 = 16;
    pub const SW_CHECK: u64 = 18;
    /// H extension.
    pub const ECALL_VS: u64 = 10;
    pub const INSN_GUEST_PAGE_FAULT: u64 = 20;
    pub const LOAD_GUEST_PAGE_FAULT: u64 = 21;
    pub const VIRTUAL_INSN: u64 = 22;
    pub const STORE_GUEST_PAGE_FAULT: u64 = 23;
}

/// A synchronous exception raised by an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exception {
    /// Cause code (see [`cause`]).
    pub cause: u64,
    /// Value for `xtval`.
    pub tval: u64,
    /// H extension: the guest physical address shifted right by 2, for
    /// `htval`/`mtval2` (guest-page faults; 0 otherwise).
    pub tval2: u64,
    /// H extension: `tval` is a guest virtual address (`hstatus.GVA`,
    /// `mstatus.GVA`).
    pub gva: bool,
}

impl Exception {
    /// Creates an exception.
    #[inline]
    pub const fn new(cause: u64, tval: u64) -> Self {
        Self {
            cause,
            tval,
            tval2: 0,
            gva: false,
        }
    }

    /// The same exception with another `tval`.
    #[inline]
    pub const fn with_tval(self, tval: u64) -> Self {
        Self { tval, ..self }
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
/// Slots in the block index. A Linux boot runs about 64 K distinct blocks;
/// the old 4096-entry cache refilled each one about 30 times.
const BBCACHE_SIZE: usize = 32768;
/// Decoded instructions the block arena holds before it is cleared. Each
/// block takes its length in slots; slots of replaced blocks are reclaimed
/// only when the arena is cleared. The memory is reserved, not touched, up
/// front.
const BB_ARENA_SIZE: usize = 1 << 19;

/// Direct-mapped index into a decode cache of `size` entries: the physical
/// address above the 2-byte instruction alignment, folded with the bits above
/// it. Without the fold, blocks at the same offset in every `2 * size`-byte
/// window collide. A Linux 6.12 boot took 7.8 M conflict misses (96% of all
/// misses) in the old 1024-entry, unfolded block cache. Tags hold the full
/// address, so the index function does not affect correctness.
#[inline]
fn decode_cache_index(pa: u64, size: usize) -> usize {
    (((pa >> 1) ^ (pa >> 13)) as usize) & (size - 1)
}
const BB_MAX_INSNS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BbOpKind {
    Seq,
    /// A conditional branch: the block goes on after it, and a taken branch
    /// leaves the block early.
    Branch,
    Term,
    Abort,
}

#[inline]
const fn bb_op_kind(op: Op) -> BbOpKind {
    match op {
        Op::Illegal
        | Op::FenceI
        | Op::Ecall
        | Op::Ebreak
        | Op::Csrrw
        | Op::Csrrs
        | Op::Csrrc
        | Op::Csrrwi
        | Op::Csrrsi
        | Op::Csrrci
        | Op::Mret
        | Op::Sret
        | Op::Wfi
        | Op::WrsNto
        | Op::WrsSto
        | Op::SfenceVma
        | Op::SinvalVma
        | Op::HfenceVvma
        | Op::HfenceGvma
        | Op::SfenceWInval
        | Op::SfenceInvalIr
        | Op::CboInval => BbOpKind::Abort,
        Op::Beq | Op::Bne | Op::Blt | Op::Bge | Op::Bltu | Op::Bgeu => BbOpKind::Branch,
        Op::Jal | Op::Jalr => BbOpKind::Term,
        _ => BbOpKind::Seq,
    }
}

/// The result of a Zba, Zbb, Zbs or Zicond operation (the same as in
/// `execute`), or `None` for any other operation. Kept out of
/// [`Hart::exec_inline`]'s `match` (which slows the common path when it
/// grows) but still far cheaper than `execute`; kernels use these in their
/// string functions (strcmp_zbb and others).
#[inline(never)]
fn bitmanip_value(op: Op, rs1: u64, rs2: u64, imm: u64) -> Option<u64> {
    Some(match op {
        Op::AddUw => rs2.wrapping_add(u64::from(rs1 as u32)),
        Op::Sh1add => rs2.wrapping_add(rs1 << 1),
        Op::Sh2add => rs2.wrapping_add(rs1 << 2),
        Op::Sh3add => rs2.wrapping_add(rs1 << 3),
        Op::Sh1addUw => rs2.wrapping_add(u64::from(rs1 as u32) << 1),
        Op::Sh2addUw => rs2.wrapping_add(u64::from(rs1 as u32) << 2),
        Op::Sh3addUw => rs2.wrapping_add(u64::from(rs1 as u32) << 3),
        Op::SlliUw => u64::from(rs1 as u32) << (imm & 63),
        Op::Andn => rs1 & !rs2,
        Op::Orn => rs1 | !rs2,
        Op::Xnor => !(rs1 ^ rs2),
        Op::Clz => u64::from(rs1.leading_zeros()),
        Op::Ctz => u64::from(rs1.trailing_zeros()),
        Op::Cpop => u64::from(rs1.count_ones()),
        Op::Clzw => u64::from((rs1 as u32).leading_zeros()),
        Op::Ctzw => u64::from((rs1 as u32).trailing_zeros()),
        Op::Cpopw => u64::from((rs1 as u32).count_ones()),
        Op::SextB => rs1 as i8 as i64 as u64,
        Op::SextH => rs1 as i16 as i64 as u64,
        Op::ZextH => u64::from(rs1 as u16),
        Op::Min => (rs1 as i64).min(rs2 as i64) as u64,
        Op::Minu => rs1.min(rs2),
        Op::Max => (rs1 as i64).max(rs2 as i64) as u64,
        Op::Maxu => rs1.max(rs2),
        Op::Rol => rs1.rotate_left((rs2 & 63) as u32),
        Op::Ror => rs1.rotate_right((rs2 & 63) as u32),
        Op::Rolw => sext32(u64::from((rs1 as u32).rotate_left((rs2 & 31) as u32))),
        Op::Rorw => sext32(u64::from((rs1 as u32).rotate_right((rs2 & 31) as u32))),
        Op::Rori => rs1.rotate_right((imm & 63) as u32),
        Op::Roriw => sext32(u64::from((rs1 as u32).rotate_right((imm & 31) as u32))),
        Op::OrcB => {
            let mut out = 0u64;
            for i in 0..8 {
                if (rs1 >> (i * 8)) & 0xff != 0 {
                    out |= 0xff << (i * 8);
                }
            }
            out
        }
        Op::Rev8 => rs1.swap_bytes(),
        Op::Bclr => rs1 & !(1u64 << (rs2 & 63)),
        Op::Bext => (rs1 >> (rs2 & 63)) & 1,
        Op::Binv => rs1 ^ (1u64 << (rs2 & 63)),
        Op::Bset => rs1 | (1u64 << (rs2 & 63)),
        Op::Bclri => rs1 & !(1u64 << (imm & 63)),
        Op::Bexti => (rs1 >> (imm & 63)) & 1,
        Op::Binvi => rs1 ^ (1u64 << (imm & 63)),
        Op::Bseti => rs1 | (1u64 << (imm & 63)),
        Op::CzeroEqz => {
            if rs2 == 0 {
                0
            } else {
                rs1
            }
        }
        Op::CzeroNez => {
            if rs2 != 0 {
                0
            } else {
                rs1
            }
        }
        _ => return None,
    })
}

/// What [`Hart::exec_inline`] did with an instruction.
enum Inline {
    /// Ran it; execution goes on at this PC, the next instruction.
    Next(u64),
    /// Ran a taken branch or a jump to this PC, which leaves the block.
    Jump(u64),
    /// Did nothing: the instruction needs `execute`.
    Slow,
}

/// What a block's inline loads and stores use: the fast-TLB slot of the
/// block's translation context with its epoch, and a view of guest RAM. All
/// of it stays valid for the whole block (see [`Hart::try_step_bb`]).
#[derive(Clone, Copy)]
struct FastMem<'a> {
    slot: usize,
    epoch_hi: u64,
    ram: RamView<'a>,
}

/// Whether `op` is an integer operation whose only effect is writing `rd`
/// (the value-producing arms of [`Hart::exec_inline`]).
const fn writes_only_rd(op: Op) -> bool {
    matches!(
        op,
        Op::Lui
            | Op::Auipc
            | Op::Addi
            | Op::Slti
            | Op::Sltiu
            | Op::Xori
            | Op::Ori
            | Op::Andi
            | Op::Slli
            | Op::Srli
            | Op::Srai
            | Op::Add
            | Op::Sub
            | Op::Sll
            | Op::Slt
            | Op::Sltu
            | Op::Xor
            | Op::Srl
            | Op::Sra
            | Op::Or
            | Op::And
            | Op::Addiw
            | Op::Slliw
            | Op::Srliw
            | Op::Sraiw
            | Op::Addw
            | Op::Subw
            | Op::Sllw
            | Op::Srlw
            | Op::Sraw
            | Op::Mul
            | Op::Mulh
            | Op::Mulhsu
            | Op::Mulhu
            | Op::Mulw
            | Op::Div
            | Op::Divu
            | Op::Rem
            | Op::Remu
            | Op::Divw
            | Op::Divuw
            | Op::Remw
            | Op::Remuw
            | Op::AddUw
            | Op::Sh1add
            | Op::Sh2add
            | Op::Sh3add
            | Op::Sh1addUw
            | Op::Sh2addUw
            | Op::Sh3addUw
            | Op::SlliUw
            | Op::Andn
            | Op::Orn
            | Op::Xnor
            | Op::Clz
            | Op::Ctz
            | Op::Cpop
            | Op::Clzw
            | Op::Ctzw
            | Op::Cpopw
            | Op::SextB
            | Op::SextH
            | Op::ZextH
            | Op::Min
            | Op::Minu
            | Op::Max
            | Op::Maxu
            | Op::Rol
            | Op::Ror
            | Op::Rolw
            | Op::Rorw
            | Op::Rori
            | Op::Roriw
            | Op::OrcB
            | Op::Rev8
            | Op::Bclr
            | Op::Bext
            | Op::Binv
            | Op::Bset
            | Op::Bclri
            | Op::Bexti
            | Op::Binvi
            | Op::Bseti
            | Op::CzeroEqz
            | Op::CzeroNez
    )
}

/// An instruction as the block cache stores it: an integer operation that
/// writes x0 has no effect and becomes [`Op::Nop`], so that
/// [`Hart::exec_inline`] can write `rd` without testing for x0. The raw
/// encoding stays (fence.i re-validation compares it).
#[inline]
fn block_insn(insn: Insn) -> Insn {
    if insn.rd == 0 && writes_only_rd(insn.op) {
        Insn {
            op: Op::Nop,
            ..insn
        }
    } else {
        insn
    }
}

/// A general-purpose register number that the compiler knows is below 32:
/// indexing `Hart::x` with it needs neither a mask nor a bounds check (the
/// block loop indexes it two or three times per instruction).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Reg {
    R0,
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
    R7,
    R8,
    R9,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
    R16,
    R17,
    R18,
    R19,
    R20,
    R21,
    R22,
    R23,
    R24,
    R25,
    R26,
    R27,
    R28,
    R29,
    R30,
    R31,
}

impl Reg {
    const ALL: [Reg; 32] = [
        Reg::R0,
        Reg::R1,
        Reg::R2,
        Reg::R3,
        Reg::R4,
        Reg::R5,
        Reg::R6,
        Reg::R7,
        Reg::R8,
        Reg::R9,
        Reg::R10,
        Reg::R11,
        Reg::R12,
        Reg::R13,
        Reg::R14,
        Reg::R15,
        Reg::R16,
        Reg::R17,
        Reg::R18,
        Reg::R19,
        Reg::R20,
        Reg::R21,
        Reg::R22,
        Reg::R23,
        Reg::R24,
        Reg::R25,
        Reg::R26,
        Reg::R27,
        Reg::R28,
        Reg::R29,
        Reg::R30,
        Reg::R31,
    ];

    /// The register `v & 31`.
    #[inline]
    const fn of(v: u8) -> Self {
        Self::ALL[(v & 31) as usize]
    }

    #[inline(always)]
    const fn i(self) -> usize {
        self as usize
    }
}

/// A decoded instruction as the block arena holds it: [`Insn`] with the
/// integer register fields typed as [`Reg`] (same size, 16 bytes).
#[derive(Clone, Copy, Debug)]
struct BlockInsn {
    imm: i32,
    raw: u32,
    op: Op,
    rd: Reg,
    rs1: Reg,
    rs2: Reg,
    rs3: u8,
    funct: u8,
    dbl: bool,
    len: u8,
}

impl BlockInsn {
    fn new(i: Insn) -> Self {
        Self {
            imm: i.imm,
            raw: i.raw,
            op: i.op,
            rd: Reg::of(i.rd),
            rs1: Reg::of(i.rs1),
            rs2: Reg::of(i.rs2),
            rs3: i.rs3,
            funct: i.funct,
            dbl: i.dbl,
            len: i.len,
        }
    }

    /// The [`Insn`] for `execute` (instructions that do not run inline).
    #[inline]
    fn insn(&self) -> Insn {
        Insn {
            op: self.op,
            rd: self.rd as u8,
            rs1: self.rs1 as u8,
            rs2: self.rs2 as u8,
            rs3: self.rs3,
            funct: self.funct,
            dbl: self.dbl,
            len: self.len,
            imm: self.imm,
            raw: self.raw,
        }
    }
}

/// Decoded basic-block index entry, indexed by
/// `decode_cache_index(pa, BBCACHE_SIZE)`. The instructions are
/// `bb_arena[start..start + len]`.
#[derive(Clone, Copy, Debug, Default)]
struct BbCacheEntry {
    /// `(pa << 16) | icache_gen` (0 means invalid).
    tag: u64,
    /// First instruction in `bb_arena`.
    start: u32,
    /// Number of decoded instructions (`0..=BB_MAX_INSNS`).
    len: u8,
}

/// Macro-op fusion descriptor cached alongside the first instruction in [`IcacheEntry`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FusedOp {
    #[default]
    None,
    /// `auipc rd, imm20` + `addi rd, rd, imm12`
    AuipcAddi { rd: u8, total_len: u8, offset: i64 },
    /// `lui rd, imm20` + `addi rd, rd, imm12`
    LuiAddi { rd: u8, total_len: u8, imm: u64 },
    /// `lui rd, imm20` + `addiw rd, rd, imm12`
    LuiAddiw { rd: u8, total_len: u8, imm: u64 },
    /// `slli rd, rs1, 32` + `srli rd, rd, 32`
    ZextW { rd: u8, rs1: u8, total_len: u8 },
    /// `auipc t, imm20` + `jalr rd, imm12(t)`
    AuipcJalr {
        t_reg: u8,
        link_rd: u8,
        total_len: u8,
        auipc_imm: i32,
        jalr_offset: i64,
    },
}

pub(crate) fn try_fuse(first: &Insn, second: &Insn) -> FusedOp {
    let rd = first.rd;
    if rd == 0 {
        return FusedOp::None;
    }
    let total_len = first.len + second.len;
    match (first.op, second.op) {
        (Op::Auipc, Op::Addi) if second.rd == rd && second.rs1 == rd => FusedOp::AuipcAddi {
            rd,
            total_len,
            offset: i64::from(first.imm).wrapping_add(i64::from(second.imm)),
        },
        (Op::Lui, Op::Addi) if second.rd == rd && second.rs1 == rd => FusedOp::LuiAddi {
            rd,
            total_len,
            imm: (first.imm as u64).wrapping_add(second.imm as u64),
        },
        (Op::Lui, Op::Addiw) if second.rd == rd && second.rs1 == rd => FusedOp::LuiAddiw {
            rd,
            total_len,
            imm: sext32((first.imm as u64).wrapping_add(second.imm as u64)),
        },
        (Op::Slli, Op::Srli)
            if first.imm == 32 && second.imm == 32 && second.rd == rd && second.rs1 == rd =>
        {
            FusedOp::ZextW {
                rd,
                rs1: first.rs1,
                total_len,
            }
        }
        (Op::Auipc, Op::Jalr) if second.rs1 == rd => FusedOp::AuipcJalr {
            t_reg: rd,
            link_rd: second.rd,
            total_len,
            auipc_imm: first.imm,
            jalr_offset: i64::from(first.imm).wrapping_add(i64::from(second.imm)),
        },
        _ => FusedOp::None,
    }
}

#[derive(Clone, Copy, Debug)]
struct IcacheEntry {
    /// `(pa << 16) | icache_gen` (0 means invalid).
    tag: u64,
    insn: Insn,
    fused: FusedOp,
}

impl Default for IcacheEntry {
    fn default() -> Self {
        Self {
            tag: 0,
            insn: Insn::ILLEGAL,
            fused: FusedOp::None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Reservation {
    pa: u64,
    size: AccessSize,
    val: u64,
    seq: u64,
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
    /// Vector registers (`v0..v31`, 256 bits = 32 bytes each, little-endian).
    pub v: [[u8; 32]; 32],
    /// Current privilege level.
    pub prv: Priv,
    /// CSR state.
    pub csr: Csrs,
    /// Hardware interrupt lines.
    pub irqs: Arc<HartIrqs>,
    /// Waiting for an interrupt (`wfi`).
    pub wfi: bool,
    pub(crate) timebase: Arc<dyn Timebase>,
    /// Active 64-byte cache-line `LR` reservation.
    reservation: Option<Reservation>,
    pub(crate) tlb: Tlb,
    pub(crate) pmp_cache: PmpCache,
    icache: Box<[IcacheEntry; ICACHE_SIZE]>,
    bbcache: Box<[BbCacheEntry; BBCACHE_SIZE]>,
    bb_arena: Vec<BlockInsn>,
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
    /// The current instruction wrote `mcycle` (suppresses its increment).
    cycle_written: bool,
    /// The current instruction wrote an `icount` trigger in `tdata1`.
    icount_written: bool,
    /// Bitmask of `mhpmcounter3..31` written by the current instruction.
    hpm_written_mask: u32,
    /// Active `-d` flags (`0` when logging is disabled).
    pub(crate) log_flags: LogFlags,
    pub(crate) logger: Option<Arc<Logger>>,
    /// Active debugger PC breakpoints.
    pub breakpoints: Vec<u64>,
    /// Active debugger data watchpoints: `(addr, len, kind)` where `kind` is `1` (read), `2` (write), `3` (access).
    pub watchpoints: Vec<(u64, u64, u8)>,
    /// Set by [`Self::run`] when execution stops on a breakpoint or watchpoint.
    pub hit_breakpoint: bool,
    /// Intercept S-mode `ecall` instructions with the built-in Rust SBI handler.
    pub handle_sbi: bool,
    /// Set when an S-mode `ecall` needs machine-level SBI servicing (`IPI`, `RFENCE`, `HSM`, `SRST`, `DBCN`).
    pub pending_sbi: bool,
    /// Set when an `ebreak` matches the RISC-V Semihosting magic sequence.
    pub pending_semihost: bool,
    /// Service RISC-V Semihosting calls (`-semihosting`). When `false`, the
    /// magic sequence's `ebreak` raises a breakpoint exception like any other
    /// `ebreak`, as on QEMU without `-semihosting` or on hardware without a
    /// debugger. OpenSBI probes for semihosting this way and, when the probe
    /// succeeds, uses the semihosting console instead of the UART.
    pub semihosting: bool,
    /// Whether the hart is parked in the SBI HSM `STOPPED` state.
    pub hsm_stopped: bool,
    /// Implement Smdbltrp (`-cpu rv64,smdbltrp=on`). Off by default, as in
    /// Sail and QEMU's `rv64` model: software that predates Smdbltrp, such
    /// as riscv-tests, takes M-mode traps before clearing the MDT bit that
    /// Smdbltrp sets at reset. When off, `mstatus.MDT` is read-only zero.
    pub smdbltrp: bool,
    /// Implement the H extension (`-cpu rv64,h=on`). Off by default.
    pub h_ext: bool,
    /// Virtualization mode V (H extension): `true` in VS- and VU-mode.
    /// While it is set, the S-level CSRs hold the VS copies (see
    /// [`csr::SBank`] and [`Hart::set_virt`]).
    pub virt: bool,
    /// Smdbltrp critical-error state: a trap to M-mode arrived while
    /// `mstatus.MDT` was 1. The hart stops without updating any architectural
    /// state (there is no Smrnmi to take the trap instead).
    pub critical_error: bool,
    /// Set when an instruction read a value that depends on device timing or
    /// on the implementation rather than on architectural state: a load
    /// below DRAM (MMIO), or a read of a counter, `time`, `seed`, `tinfo`,
    /// `mip`/`sip` or `scountovf`. Co-simulation clears it before a step and,
    /// when it is set afterwards, takes the reference model's result instead
    /// of comparing it. The hart itself never clears it.
    pub nondeterministic_read: bool,
    /// Per-hart SBI FWFT locks: bit 0 for landing pads, bit 1 for shadow stacks.
    pub fwft_locks: u8,
    /// Trap on any unaligned scalar load or store (`LOAD_MISALIGNED` / `STORE_MISALIGNED`).
    pub strict_align: bool,
    /// Enforce `Svade` page-table rules (page fault when `pte.A == 0` or `pte.D == 0` on store).
    pub svade: bool,
    /// Spec PMP rule: an S/U-mode access that matches no active entry fails.
    /// `MachineConfig` turns this on by default; a bare `Hart::new` leaves it
    /// off so directed CPU tests can run S/U code without programming PMP.
    pub strict_pmp: bool,
    /// `Zicfilp` expected landing pad state (`true` = `LP_EXPECTED`).
    pub elp: bool,
    /// Cached flag indicating that PMP permission checks are active.
    pub(crate) pmp_enforced: bool,
    /// True when any active PMP entry has boundaries not aligned to 4 KiB (`0x1000`).
    pub(crate) pmp_subpage: bool,
    /// True when any active PMP entry has the lock bit (`L = 0x80`) set.
    pub(crate) pmp_any_locked: bool,
    /// Number of active entries in [`Self::pmp_ranges`].
    pub(crate) pmp_count: u8,
    /// Precomputed `(lo, hi_inclusive, cfg)` for active PMP entries in priority order.
    pub(crate) pmp_ranges: [(u64, u64, u8); csr::PMP_ENTRIES],
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
            v: [[0; 32]; 32],
            prv: Priv::Machine,
            csr: Csrs::default(),
            irqs: HartIrqs::new(),
            wfi: false,
            timebase,
            reservation: None,
            tlb: Tlb::default(),
            pmp_cache: PmpCache::default(),
            icache: Box::new([IcacheEntry::default(); ICACHE_SIZE]),
            // Built on the heap: the index is 512 KiB, more than fits on a
            // test thread's stack.
            bbcache: vec![BbCacheEntry::default(); BBCACHE_SIZE]
                .into_boxed_slice()
                .try_into()
                .expect("BBCACHE_SIZE entries"),
            bb_arena: Vec::with_capacity(BB_ARENA_SIZE),
            icache_gen: 1,
            fetch_vpn: 0,
            fetch_pa_page: 0,
            check_irq: true,
            yield_slice: false,
            instret_written: false,
            cycle_written: false,
            icount_written: false,
            hpm_written_mask: 0,
            log_flags: LogFlags::default(),
            logger: None,
            breakpoints: Vec::new(),
            watchpoints: Vec::new(),
            hit_breakpoint: false,
            handle_sbi: false,
            pending_sbi: false,
            pending_semihost: false,
            semihosting: false,
            hsm_stopped: false,
            smdbltrp: false,
            h_ext: false,
            virt: false,
            critical_error: false,
            nondeterministic_read: false,
            fwft_locks: 0,
            strict_align: false,
            svade: false,
            strict_pmp: false,
            elp: false,
            pmp_enforced: false,
            pmp_subpage: false,
            pmp_any_locked: false,
            pmp_count: 0,
            pmp_ranges: [(0, 0, 0); csr::PMP_ENTRIES],
        }
    }

    /// Selects Svade (`true`: missing A/D bits raise page faults) or Svadu
    /// (`false`: hardware updates them) as the reset scheme by setting the
    /// reset value of `menvcfg.ADUE`. Software can still change `ADUE`;
    /// the MMU follows the current `ADUE` value.
    pub fn set_svade(&mut self, svade: bool) {
        self.svade = svade;
        if svade {
            self.csr.menvcfg &= !csr::envcfg::ADUE;
        } else {
            self.csr.menvcfg |= csr::envcfg::ADUE;
        }
    }

    /// Resets architectural state and starts execution at `pc`.
    pub fn reset(&mut self, pc: u64) {
        self.pc = pc;
        self.x = [0; 32];
        self.f = [0; 32];
        self.v = [[0; 32]; 32];
        self.prv = Priv::Machine;
        self.csr = Csrs::default();
        self.virt = false;
        // Smdbltrp: "Upon reset, the MDT field is set to 1."
        if self.smdbltrp {
            self.csr.mstatus |= status::MDT;
        }
        self.critical_error = false;
        if self.svade {
            self.csr.menvcfg &= !csr::envcfg::ADUE;
        }
        self.wfi = false;
        self.reservation = None;
        self.flush_tlb();
        self.flush_icache();
        self.check_irq = true;
        self.yield_slice = false;
        self.hit_breakpoint = false;
        self.pending_sbi = false;
        self.pending_semihost = false;
        self.hsm_stopped = false;
        self.fwft_locks = 0;
        self.elp = false;
        self.update_pmp_state();
    }

    /// Resets the hart directly into S-mode for built-in Rust SBI boot or
    /// `sbi_hsm_hart_start`, setting `a0 = hartid` and `a1 = opaque`.
    pub fn init_smode_boot(&mut self, entry: u64, opaque: u64) {
        self.reset(entry);
        self.prv = Priv::Supervisor;
        self.x[10] = self.id;
        self.x[11] = opaque;
        // Entering S-mode directly stands in for firmware that ended with
        // MRET, which clears MDT.
        self.csr.mstatus = (2 << 32) | (2 << 34);
        self.critical_error = false;
        self.csr.medeleg = 0xb1ff | (1 << cause::SW_CHECK);
        self.csr.mideleg = irq::S_MASK;
        self.csr.mcounteren = 0xffff_ffff;
        self.csr.scounteren = 0xffff_ffff;
        self.csr.menvcfg =
            envcfg::STCE | envcfg::PBMTE | envcfg::CBIE_MASK | envcfg::CBCFE | envcfg::CBZE;
        self.csr.senvcfg = envcfg::CBIE_MASK | envcfg::CBCFE | envcfg::CBZE;
        self.csr.mseccfg = csr::mseccfg::SSEED;
        // Like OpenSBI's mstatus_init: give S-mode its stateen-gated state.
        self.csr.mstateen = [
            csr::stateen::M0_MASK,
            csr::stateen::M1_3_MASK,
            csr::stateen::M1_3_MASK,
            csr::stateen::M1_3_MASK,
        ];
        self.csr.pmpaddr[0] = (1 << 54) - 1;
        self.csr.pmpcfg[0] = 0x1f;
        self.update_pmp_state();
        self.handle_sbi = true;
        self.hsm_stopped = false;
    }

    /// Resumes an S-mode hart after a non-retentive SBI HSM suspend.
    ///
    /// Architectural state is reset as for a fresh S-mode entry, but local
    /// firmware-feature values and their locks last until hart reset and must
    /// therefore survive a non-retentive suspend.
    pub fn resume_smode_nonretentive(&mut self, entry: u64, opaque: u64) {
        let fwft_values = self.csr.menvcfg & (envcfg::LPE | envcfg::SSE);
        let fwft_locks = self.fwft_locks;
        self.init_smode_boot(entry, opaque);
        self.csr.menvcfg |= fwft_values;
        self.fwft_locks = fwft_locks;
    }

    /// Flushes the per-hart TLB (e.g. on remote `sfence.vma`).
    #[inline]
    pub fn invalidate_tlb(&mut self) {
        self.flush_tlb();
    }

    /// Marks interrupts for re-evaluation and wakes from `wfi` if not HSM-stopped.
    #[inline]
    pub fn notify_irq(&mut self) {
        self.check_irq = true;
        if !self.hsm_stopped {
            self.wfi = false;
        }
    }

    /// Flushes the decoded-instruction cache (e.g. after debugger memory writes).
    #[inline]
    pub fn invalidate_icache(&mut self) {
        self.flush_icache();
    }

    #[inline]
    pub(crate) fn flush_tlb(&mut self) {
        self.tlb.flush();
        self.fetch_vpn = 0;
    }

    #[inline]
    fn sfence_vma(&mut self, rs1_idx: u8, rs2_idx: u8, va: u64, asid: u64) {
        self.tlb.sfence_vma(rs1_idx, rs2_idx, va, asid);
        self.fetch_vpn = 0;
    }

    #[inline]
    fn flush_icache(&mut self) {
        self.icache_gen = self.icache_gen.wrapping_add(1);
        if self.icache_gen == 0 {
            self.icache.fill(IcacheEntry::default());
            self.bbcache.fill(BbCacheEntry::default());
            self.bb_arena.clear();
            self.icache_gen = 1;
        }
    }

    /// Number of retired instructions.
    pub fn instret(&self) -> u64 {
        self.csr.minstret
    }

    /// Clears the implementation-defined LR/SC reservation after external
    /// machine state replacement (for example, snapshot restore).
    pub fn clear_reservation(&mut self) {
        self.reservation = None;
    }

    /// Register indices are masked with `& 31` here and in the hot paths:
    /// decoded fields are always below 32, and the mask lets the compiler
    /// drop the bounds check.
    #[inline]
    pub(crate) fn set_x(&mut self, rd: u8, v: u64) {
        if rd != 0 {
            self.x[usize::from(rd & 31)] = v;
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
        if self.hsm_stopped || self.critical_error {
            return 0;
        }
        self.take_pending_interrupt();
        if self.wfi {
            return 0;
        }
        let has_bp = !self.breakpoints.is_empty();
        // Whether blocks and fused pairs may run. Its inputs change only
        // through instructions that `step` executes (CSR writes, xRET) or
        // through a change of privilege mode (traps), so it is recomputed
        // after every `step` and whenever a block returns in another mode.
        let mut fast_ok = !has_bp && self.blocks_allowed();
        let mut fast_prv = self.prv;
        let mut n = 0;
        while n < budget {
            if has_bp && self.breakpoints.contains(&self.pc) {
                self.hit_breakpoint = true;
                break;
            }
            if fast_ok && n + 1 < budget {
                // Blocks first: they run up to BB_MAX_INSNS instructions per
                // dispatch, and a fusible pair inside a block just runs as two
                // ordinary instructions with the same result. Fused pairs
                // still help where no block of 2+ instructions starts.
                let bb_steps = self.try_step_bb(bus, budget - n);
                if bb_steps > 0 {
                    n += bb_steps;
                    if self.wfi || self.yield_slice {
                        break;
                    }
                    if self.prv != fast_prv {
                        fast_prv = self.prv;
                        fast_ok = self.blocks_allowed();
                    }
                    continue;
                }
                if self.try_step_fused(bus) {
                    n += 2;
                    continue;
                }
            }
            self.step(bus);
            n += 1;
            if self.check_irq {
                self.take_pending_interrupt();
            }
            if self.wfi || self.yield_slice || self.hit_breakpoint {
                break;
            }
            fast_ok = !has_bp && self.blocks_allowed();
            fast_prv = self.prv;
        }
        self.reservation = None;
        n
    }

    /// Whether `run` may use blocks and fused pairs: they skip the landing
    /// pad, sub-page PMP, logging, HPM event and trigger checks.
    #[inline]
    fn blocks_allowed(&self) -> bool {
        !self.elp
            && !self.csr.landing_pad_enabled(self.prv)
            && !self.pmp_subpage
            && self.log_flags.is_empty()
            && self.csr.active_hpm_mask == 0
            && self.csr.active_trigger_mask == 0
            && self.watchpoints.is_empty()
    }

    /// Advances `minstret` and `mcycle` by `delta` retired instructions,
    /// honouring `mcountinhibit.IR`/`.CY` and the Smcntrpmf mode filters.
    #[inline]
    fn bump_counters(&mut self, delta: u64) {
        // Common case: neither counter inhibited or filtered by mode.
        const MODE_INH: u64 = csr::hpmevent::FILTER_MASK_H;
        if self.csr.mcountinhibit & 5 == 0
            && (self.csr.minstretcfg | self.csr.mcyclecfg) & MODE_INH == 0
        {
            self.csr.minstret = self.csr.minstret.wrapping_add(delta);
            self.csr.mcycle = self.csr.mcycle.wrapping_add(delta);
            return;
        }
        if self.csr.mcountinhibit & 4 == 0 && !self.mode_inhibited(self.csr.minstretcfg) {
            self.csr.minstret = self.csr.minstret.wrapping_add(delta);
        }
        if self.csr.mcountinhibit & 1 == 0 && !self.mode_inhibited(self.csr.mcyclecfg) {
            self.csr.mcycle = self.csr.mcycle.wrapping_add(delta);
        }
    }

    #[inline]
    fn try_step_bb(&mut self, bus: &AddressSpace, max_insns: u64) -> u64 {
        let pc = self.pc;
        if pc & 0xfff > 0xffc {
            return 0;
        }
        // The fetch page cache holds only addresses the decode caches can tag
        // (below 2^48), so a hit needs no check.
        let vpn_tag = (pc >> 12) + 1;
        let pa = if self.fetch_vpn == vpn_tag {
            self.fetch_pa_page | (pc & 0xfff)
        } else {
            let Ok(pa) = self.translate(bus, pc, Access::Fetch) else {
                return 0;
            };
            if pa >> 48 != 0 {
                return 0;
            }
            self.fetch_vpn = vpn_tag;
            self.fetch_pa_page = pa & !0xfff;
            pa
        };
        let bb_idx = decode_cache_index(pa, BBCACHE_SIZE);
        let tag = (pa << 16) | u64::from(self.icache_gen);
        let old_tag = self.bbcache[bb_idx].tag;
        let avail = if old_tag == tag {
            usize::from(self.bbcache[bb_idx].len)
        } else if old_tag != 0 && old_tag >> 16 == pa && self.bb_matches_memory(bus, pc, pa, bb_idx)
        {
            // Same block, decoded before the last fence.i, and memory still
            // holds the same instructions: keep the decode.
            self.bbcache[bb_idx].tag = tag;
            usize::from(self.bbcache[bb_idx].len)
        } else {
            self.populate_bbcache_same_page(bus, pc, pa, bb_idx, tag)
        };
        if avail == 0 {
            return 0;
        }
        // Everything the inline load/store path depends on besides the fast
        // TLB itself is constant within a block: the privilege mode,
        // mstatus, senvcfg and the pointer-masking CSRs change only through
        // CSR writes, traps and xRET, and those end a block. `run` already
        // excluded triggers, watchpoints, logging and sub-page PMP.
        let mem = if !self.strict_align
            && (self.csr.menvcfg | self.csr.senvcfg | self.csr.mseccfg | self.csr.h.henvcfg)
                & envcfg::PMM_MASK
                == 0
        {
            bus.fast_ram_view().map(|ram| {
                let ctx = self.fast_tlb_ctx();
                let slot = self.tlb.fast.slot_for(ctx);
                FastMem {
                    slot,
                    epoch_hi: self.tlb.fast.slots[slot].epoch_hi,
                    ram,
                }
            })
        } else {
            None
        };
        // The arena moves out of `self` while the block runs, so that the
        // loop walks a plain slice. Nothing a block can contain touches the
        // decode caches: fence.i, CSR accesses and the other flushing
        // instructions end a block (`BbOpKind::Abort`).
        let arena = std::mem::take(&mut self.bb_arena);
        let gen_before = self.icache_gen;
        let mut total = 0u64;
        // Instructions of completed blocks, counted into minstret/mcycle once
        // when the chain ends (nothing in a chain reads them: CSR accesses
        // end blocks, and a block that stops early counts its own).
        let mut completed_insns = 0u64;
        let (mut idx, mut avail) = (bb_idx, avail);
        loop {
            let block_pc = self.pc;
            let start = self.bbcache[idx].start as usize;
            let limit = avail.min((max_insns - total) as usize);
            let (done, completed) = self.run_block(bus, &arena[start..start + limit], mem);
            total += done;
            if !completed || total >= max_insns {
                if completed {
                    completed_insns += done;
                }
                break;
            }
            completed_insns += done;
            // A loop that branches back to its own start runs the same
            // block again (the decode caches do not change within a chain).
            if self.pc == block_pc {
                continue;
            }
            // Chain into the next block while it is already decoded: the
            // block-invariant state above (privilege, mstatus, the envcfg
            // CSRs) only changes through something that ends the chain.
            let fetch_slot = mem.as_ref().map(|m| (m.slot, m.epoch_hi));
            match self.cached_block_at_pc(bus, fetch_slot) {
                Some(next) => (idx, avail) = next,
                None => break,
            }
        }
        if completed_insns != 0 {
            self.bump_counters(completed_insns);
        }
        debug_assert_eq!(
            self.icache_gen, gen_before,
            "a block flushed the decode caches"
        );
        self.bb_arena = arena;
        total
    }

    /// The decoded block at `self.pc` if the cache holds it for the current
    /// decode generation (no re-validation or decoding: the arena may be
    /// moved out of the hart while this runs), as `(index, length)`.
    /// `fetch_slot` is the chain's fast-TLB slot and its epoch: a fetch from
    /// another page looks there before it takes the full translation.
    #[inline(always)]
    fn cached_block_at_pc(
        &mut self,
        bus: &AddressSpace,
        fetch_slot: Option<(usize, u64)>,
    ) -> Option<(usize, usize)> {
        // No block starts within the last 3 bytes of a page (`try_step_bb`
        // decodes none there), so such a PC simply finds no block.
        let pc = self.pc;
        let vpn_tag = (pc >> 12) + 1;
        let pa = if self.fetch_vpn == vpn_tag {
            self.fetch_pa_page | (pc & 0xfff)
        } else {
            let fast = fetch_slot.and_then(|(slot, epoch_hi)| {
                let table = &self.tlb.fast.slots[slot % FAST_TLB_SLOTS].fetch;
                FastTlbSlot::hit(table, epoch_hi, pc).map(|e| e.pa_page | (pc & 0xfff))
            });
            let pa = match fast {
                Some(pa) => pa,
                None => {
                    let (pa, ram_off) = self.translate_with_ram_off(bus, pc, Access::Fetch).ok()?;
                    if pa >> 48 != 0 {
                        return None;
                    }
                    if fetch_slot.is_some() {
                        self.fast_tlb_fill_fetch(pc, pa, ram_off);
                    }
                    pa
                }
            };
            self.fetch_vpn = vpn_tag;
            self.fetch_pa_page = pa & !0xfff;
            pa
        };
        let idx = decode_cache_index(pa, BBCACHE_SIZE);
        let entry = &self.bbcache[idx];
        (entry.tag == (pa << 16) | u64::from(self.icache_gen) && entry.len != 0)
            .then_some((idx, usize::from(entry.len)))
    }

    /// Runs the decoded instructions of one block (see [`Self::try_step_bb`])
    /// and returns the number attempted, including one that trapped, and
    /// whether all of `insns` ran without a trap, interrupt check, `wfi` or
    /// yield.
    #[inline(always)]
    fn run_block(
        &mut self,
        bus: &AddressSpace,
        insns: &[BlockInsn],
        mem: Option<FastMem<'_>>,
    ) -> (u64, bool) {
        // The PC stays in a local while inline instructions run and is
        // written back before anything that reads it. This removes a
        // store-to-load dependency through `self.pc` between consecutive
        // instructions.
        let mut pc = self.pc;
        let mut i = 0;
        while i < insns.len() {
            let insn = &insns[i];
            i += 1;
            // Integer ALU, Lui/Auipc, Jal/Jalr and branches cannot trap,
            // touch memory or set check_irq/wfi/yield_slice, so they skip the
            // `execute` call and the per-instruction flag check. Neither can
            // a RAM load or store that hits the fast TLB. Blocks run only
            // with landing pads disabled and `elp` clear (see `run`), and
            // nothing in a block sets `elp`.
            debug_assert!(!self.elp);
            match self.exec_inline(bus, insn, mem, pc) {
                Inline::Next(next) => {
                    pc = next;
                    continue;
                }
                Inline::Jump(target) => {
                    // Blocks go on past conditional branches: a taken one
                    // ends the block here (`try_step_bb` counts it).
                    self.pc = target;
                    return (i as u64, true);
                }
                Inline::Slow => {}
            }
            self.pc = pc;
            match self.execute(bus, &insn.insn()) {
                Ok(()) => {
                    pc = self.pc;
                    if self.check_irq || self.wfi || self.yield_slice {
                        let done = i as u64;
                        self.bump_counters(done);
                        if self.check_irq {
                            self.take_pending_interrupt();
                        }
                        return (done, false);
                    }
                }
                Err(e) => {
                    let retired = (i - 1) as u64;
                    if retired > 0 {
                        self.bump_counters(retired);
                    }
                    self.instret_written = false;
                    self.cycle_written = false;
                    self.hpm_written_mask = 0;
                    self.trap_exception(e);
                    return (retired + 1, false);
                }
            }
        }
        self.pc = pc;
        // The caller counts completed blocks into minstret/mcycle.
        (insns.len() as u64, true)
    }

    /// Executes `insn` if it is one of the simple integer instructions, or
    /// (with `mem_ctx`, see [`Self::exec_load_inline`]) an integer load or
    /// store that hits the fast TLB, with the same semantics as the matching
    /// arm of [`Self::execute`]; returns `false` (and does nothing) for
    /// every other instruction. One `match` dispatches both kinds.
    #[inline(always)]
    fn exec_inline(
        &mut self,
        bus: &AddressSpace,
        insn: &BlockInsn,
        mem: Option<FastMem<'_>>,
        pc: u64,
    ) -> Inline {
        let next = pc.wrapping_add(u64::from(insn.len));
        let rs1 = self.x[insn.rs1.i()];
        let rs2 = self.x[insn.rs2.i()];
        let imm = insn.imm as u64;
        let rd = insn.rd;
        let v = match insn.op {
            Op::Addi => rs1.wrapping_add(imm),
            Op::Add => rs1.wrapping_add(rs2),
            Op::Sub => rs1.wrapping_sub(rs2),
            Op::Lui => imm,
            Op::Auipc => pc.wrapping_add(imm),
            Op::Andi => rs1 & imm,
            Op::Ori => rs1 | imm,
            Op::Xori => rs1 ^ imm,
            Op::Slli => rs1 << (imm & 63),
            Op::Srli => rs1 >> (imm & 63),
            Op::Srai => ((rs1 as i64) >> (imm & 63)) as u64,
            Op::Slti => u64::from((rs1 as i64) < i64::from(insn.imm)),
            Op::Sltiu => u64::from(rs1 < imm),
            Op::And => rs1 & rs2,
            Op::Or => rs1 | rs2,
            Op::Xor => rs1 ^ rs2,
            Op::Sll => rs1 << (rs2 & 63),
            Op::Srl => rs1 >> (rs2 & 63),
            Op::Sra => ((rs1 as i64) >> (rs2 & 63)) as u64,
            Op::Slt => u64::from((rs1 as i64) < (rs2 as i64)),
            Op::Sltu => u64::from(rs1 < rs2),
            Op::Addiw => sext32(rs1.wrapping_add(imm)),
            Op::Slliw => sext32(rs1 << (imm & 31)),
            Op::Srliw => sext32(u64::from((rs1 as u32) >> (imm & 31))),
            Op::Sraiw => ((rs1 as i32) >> (imm & 31)) as i64 as u64,
            Op::Addw => sext32(rs1.wrapping_add(rs2)),
            Op::Subw => sext32(rs1.wrapping_sub(rs2)),
            Op::Sllw => sext32(rs1 << (rs2 & 31)),
            Op::Srlw => sext32(u64::from((rs1 as u32) >> (rs2 & 31))),
            Op::Sraw => ((rs1 as i32) >> (rs2 & 31)) as i64 as u64,
            // M: multiplication and division never trap (division by zero
            // and overflow have defined results). Bignum code (RSA and ECC
            // signature checks while Linux loads modules) is full of these.
            Op::Mul => rs1.wrapping_mul(rs2),
            Op::Mulh => ((i128::from(rs1 as i64) * i128::from(rs2 as i64)) >> 64) as u64,
            Op::Mulhsu => ((i128::from(rs1 as i64) * i128::from(rs2)) >> 64) as u64,
            Op::Mulhu => ((u128::from(rs1) * u128::from(rs2)) >> 64) as u64,
            Op::Mulw => sext32(rs1.wrapping_mul(rs2)),
            Op::Div => {
                let (a, b) = (rs1 as i64, rs2 as i64);
                (if b == 0 { -1 } else { a.wrapping_div(b) }) as u64
            }
            Op::Divu => rs1.checked_div(rs2).unwrap_or(u64::MAX),
            Op::Rem => {
                let (a, b) = (rs1 as i64, rs2 as i64);
                (if b == 0 { a } else { a.wrapping_rem(b) }) as u64
            }
            Op::Remu => {
                if rs2 == 0 {
                    rs1
                } else {
                    rs1 % rs2
                }
            }
            Op::Divw => {
                let (a, b) = (rs1 as i32, rs2 as i32);
                (if b == 0 { -1 } else { a.wrapping_div(b) }) as i64 as u64
            }
            Op::Divuw => {
                let (a, b) = (rs1 as u32, rs2 as u32);
                sext32(u64::from(a.checked_div(b).unwrap_or(u32::MAX)))
            }
            Op::Remw => {
                let (a, b) = (rs1 as i32, rs2 as i32);
                (if b == 0 { a } else { a.wrapping_rem(b) }) as i64 as u64
            }
            Op::Remuw => {
                let (a, b) = (rs1 as u32, rs2 as u32);
                sext32(u64::from(if b == 0 { a } else { a % b }))
            }
            Op::Beq | Op::Bne | Op::Blt | Op::Bge | Op::Bltu | Op::Bgeu => {
                let taken = match insn.op {
                    Op::Beq => rs1 == rs2,
                    Op::Bne => rs1 != rs2,
                    Op::Blt => (rs1 as i64) < (rs2 as i64),
                    Op::Bge => (rs1 as i64) >= (rs2 as i64),
                    Op::Bltu => rs1 < rs2,
                    _ => rs1 >= rs2,
                };
                return if taken {
                    Inline::Jump(pc.wrapping_add(imm))
                } else {
                    Inline::Next(next)
                };
            }
            Op::Jal => {
                self.set_x(rd as u8, next);
                return Inline::Jump(pc.wrapping_add(imm));
            }
            // Blocks run only with landing pads disabled (see `run`), so
            // Jalr never sets `elp` here.
            Op::Jalr => {
                self.set_x(rd as u8, next);
                return Inline::Jump(rs1.wrapping_add(imm) & !1);
            }
            // One arm per opcode, so that each inlined access has a
            // constant size.
            Op::Ld => {
                let ok = self.exec_load_inline(insn, mem, rs1, AccessSize::Double, |v| v);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Lw => {
                let ok =
                    self.exec_load_inline(insn, mem, rs1, AccessSize::Word, |v| v as i32 as u64);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Lwu => {
                let ok = self.exec_load_inline(insn, mem, rs1, AccessSize::Word, |v| v);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Lbu => {
                let ok = self.exec_load_inline(insn, mem, rs1, AccessSize::Byte, |v| v);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Lb => {
                let ok =
                    self.exec_load_inline(insn, mem, rs1, AccessSize::Byte, |v| v as i8 as u64);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Lhu => {
                let ok = self.exec_load_inline(insn, mem, rs1, AccessSize::Half, |v| v);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Lh => {
                let ok =
                    self.exec_load_inline(insn, mem, rs1, AccessSize::Half, |v| v as i16 as u64);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Sd => {
                let ok = self.exec_store_inline(bus, insn, mem, rs1, rs2, AccessSize::Double);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Sw => {
                let ok = self.exec_store_inline(bus, insn, mem, rs1, rs2, AccessSize::Word);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Sb => {
                let ok = self.exec_store_inline(bus, insn, mem, rs1, rs2, AccessSize::Byte);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Sh => {
                let ok = self.exec_store_inline(bus, insn, mem, rs1, rs2, AccessSize::Half);
                return if ok { Inline::Next(next) } else { Inline::Slow };
            }
            Op::Nop => return Inline::Next(next),
            _ => match bitmanip_value(insn.op, rs1, rs2, imm) {
                Some(v) => v,
                None => return Inline::Slow,
            },
        };
        // Blocks store operations that write x0 as `Nop` (`block_insn`).
        debug_assert!(rd != Reg::R0);
        self.x[rd.i()] = v;
        Inline::Next(next)
    }

    /// An integer load in a block that stays within one page and hits the
    /// fast TLB: loads, extends with `ext` and writes `rd`. Returns `false`
    /// (and does nothing) otherwise, or when `mem_ctx` is `None`.
    /// `mem_ctx` is [`Self::fast_tlb_ctx`] when the block may access memory
    /// inline: no pointer masking and no strict alignment. Blocks never run
    /// with triggers, watchpoints or logging.
    #[inline(always)]
    fn exec_load_inline(
        &mut self,
        insn: &BlockInsn,
        mem: Option<FastMem<'_>>,
        rs1: u64,
        size: AccessSize,
        ext: impl Fn(u64) -> u64,
    ) -> bool {
        let Some(m) = mem else {
            return false;
        };
        let va = rs1.wrapping_add(insn.imm as u64);
        // A naturally aligned access stays within its page; `read_aligned`
        // refuses the others, which take the full path.
        let slot = &self.tlb.fast.slots[m.slot % FAST_TLB_SLOTS];
        let Some(e) = FastTlbSlot::hit(&slot.load, m.epoch_hi, va) else {
            return false;
        };
        let Some(v) = m.ram.read_aligned(e.ram_page_off | (va & 0xfff), size) else {
            return false;
        };
        if insn.rd != Reg::R0 {
            self.x[insn.rd.i()] = ext(v);
        }
        true
    }

    /// The store counterpart of [`Self::exec_load_inline`].
    #[inline(always)]
    fn exec_store_inline(
        &mut self,
        bus: &AddressSpace,
        insn: &BlockInsn,
        mem: Option<FastMem<'_>>,
        rs1: u64,
        rs2: u64,
        size: AccessSize,
    ) -> bool {
        let Some(m) = mem else {
            return false;
        };
        let va = rs1.wrapping_add(insn.imm as u64);
        let slot = &self.tlb.fast.slots[m.slot % FAST_TLB_SLOTS];
        let Some(e) = FastTlbSlot::hit(&slot.store, m.epoch_hi, va) else {
            return false;
        };
        let (pa, off) = (e.pa_page | (va & 0xfff), e.ram_page_off | (va & 0xfff));
        // The page's dirty bit is set (see `FastTlb`); `write_aligned`
        // refuses misaligned accesses, which take the full path.
        if m.ram.write_aligned(off, size, rs2).is_none() {
            return false;
        }
        self.invalidate_reservation(pa);
        bus.invalidate_cache_line(pa, size);
        true
    }

    /// Whether the block cached at `bb_idx` for `pa` still describes memory:
    /// every instruction's encoding is unchanged and its fetch still passes
    /// PMP, as when [`Self::populate_bbcache_same_page`] decoded it. A
    /// fence.i (Linux runs thousands while booting, for example whenever it
    /// maps an executable page) invalidates every decoded block, and this
    /// check is much cheaper than decoding the block again.
    #[cold]
    fn bb_matches_memory(&self, bus: &AddressSpace, pc: u64, pa: u64, bb_idx: usize) -> bool {
        let entry = &self.bbcache[bb_idx];
        let start = entry.start as usize;
        let mut off = 0u64;
        for insn in &self.bb_arena[start..start + usize::from(entry.len)] {
            let (cur_pc, cur_pa) = (pc.wrapping_add(off), pa + off);
            let size = if insn.len == 2 {
                AccessSize::Half
            } else {
                AccessSize::Word
            };
            let fetch_ok = self
                .check_pmp_access(cur_pc, cur_pa, 2, Access::Fetch)
                .is_ok()
                && (insn.len == 2
                    || self
                        .check_pmp_access(cur_pc + 2, cur_pa + 2, 2, Access::Fetch)
                        .is_ok());
            if !fetch_ok || bus.read(cur_pa, size).ok() != Some(u64::from(insn.raw)) {
                return false;
            }
            off += u64::from(insn.len);
        }
        true
    }

    #[cold]
    fn populate_bbcache_same_page(
        &mut self,
        bus: &AddressSpace,
        pc: u64,
        pa: u64,
        bb_idx: usize,
        tag: u64,
    ) -> usize {
        // Decode straight into the arena. When it is full, clear it with
        // every index entry that points into it.
        if self.bb_arena.len() + BB_MAX_INSNS > BB_ARENA_SIZE {
            self.bb_arena.clear();
            self.bbcache.fill(BbCacheEntry::default());
        }
        let start = self.bb_arena.len();
        let mut cur_pc = pc;
        let mut cur_pa = pa;
        while self.bb_arena.len() - start < BB_MAX_INSNS
            && cur_pc >> 12 == pc >> 12
            && (cur_pc & 0xfff) <= 0xffc
        {
            let Ok(insn) = self.fetch_decode_same_page(bus, cur_pc, cur_pa) else {
                break;
            };
            match bb_op_kind(insn.op) {
                BbOpKind::Abort => break,
                BbOpKind::Term => {
                    self.bb_arena.push(BlockInsn::new(insn));
                    break;
                }
                BbOpKind::Branch => {
                    self.bb_arena.push(BlockInsn::new(insn));
                    let step_len = u64::from(insn.len);
                    cur_pc = cur_pc.wrapping_add(step_len);
                    cur_pa = cur_pa.wrapping_add(step_len);
                }
                BbOpKind::Seq => {
                    self.bb_arena.push(BlockInsn::new(block_insn(insn)));
                    let step_len = u64::from(insn.len);
                    cur_pc = cur_pc.wrapping_add(step_len);
                    cur_pa = cur_pa.wrapping_add(step_len);
                }
            }
        }
        let len = self.bb_arena.len() - start;
        self.bbcache[bb_idx] = BbCacheEntry {
            tag,
            start: start as u32,
            len: len as u8,
        };
        len
    }

    #[inline]
    fn try_step_fused(&mut self, bus: &AddressSpace) -> bool {
        let pc = self.pc;
        if pc & 0xfff > 0xffc {
            return false;
        }
        let vpn_tag = (pc >> 12) + 1;
        let pa = if self.fetch_vpn == vpn_tag {
            self.fetch_pa_page | (pc & 0xfff)
        } else {
            let Ok(pa) = self.translate(bus, pc, Access::Fetch) else {
                return false;
            };
            if pa >> 48 != 0 {
                return false;
            }
            self.fetch_vpn = vpn_tag;
            self.fetch_pa_page = pa & !0xfff;
            pa
        };
        let idx = decode_cache_index(pa, ICACHE_SIZE);
        let tag = (pa << 16) | u64::from(self.icache_gen);
        let fused = if self.icache[idx].tag == tag {
            self.icache[idx].fused
        } else {
            let Ok(entry) = self.populate_icache_same_page(bus, pc, pa, idx, tag) else {
                return false;
            };
            entry.fused
        };
        match fused {
            FusedOp::None => false,
            FusedOp::AuipcAddi {
                rd,
                total_len,
                offset,
            } => {
                self.x[rd as usize] = pc.wrapping_add(offset as u64);
                self.pc = pc.wrapping_add(u64::from(total_len));
                self.bump_counters(2);
                true
            }
            FusedOp::LuiAddi { rd, total_len, imm } | FusedOp::LuiAddiw { rd, total_len, imm } => {
                self.x[rd as usize] = imm;
                self.pc = pc.wrapping_add(u64::from(total_len));
                self.bump_counters(2);
                true
            }
            FusedOp::ZextW { rd, rs1, total_len } => {
                let v = u64::from(self.x[rs1 as usize] as u32);
                self.x[rd as usize] = v;
                self.pc = pc.wrapping_add(u64::from(total_len));
                self.bump_counters(2);
                true
            }
            FusedOp::AuipcJalr {
                t_reg,
                link_rd,
                total_len,
                auipc_imm,
                jalr_offset,
            } => {
                self.x[t_reg as usize] = pc.wrapping_add(auipc_imm as i64 as u64);
                self.set_x(link_rd, pc.wrapping_add(u64::from(total_len)));
                self.pc = pc.wrapping_add(jalr_offset as u64) & !1;
                self.bump_counters(2);
                true
            }
        }
    }

    /// Executes a single instruction (or takes the resulting trap).
    pub fn step(&mut self, bus: &AddressSpace) {
        if self.log_flags.has(LogFlags::EXEC | LogFlags::CPU) {
            self.log_exec_cpu();
        }
        self.instret_written = false;
        self.cycle_written = false;
        self.icount_written = false;
        self.hpm_written_mask = 0;
        let step_prv = self.prv;
        let result = if self.csr.active_trigger_mask != 0 {
            self.check_exec_triggers()
                .and_then(|()| self.fetch(bus))
                .and_then(|insn| self.execute(bus, &insn))
        } else {
            self.fetch(bus).and_then(|insn| self.execute(bus, &insn))
        };
        match result {
            Ok(()) => {
                // An explicit write to minstret or mcycle suppresses that
                // instruction's increment of the written counter.
                if self.csr.mcountinhibit & 4 == 0
                    && !self.mode_inhibited(self.csr.minstretcfg)
                    && !std::mem::take(&mut self.instret_written)
                {
                    self.csr.minstret = self.csr.minstret.wrapping_add(1);
                }
                if self.csr.mcountinhibit & 1 == 0
                    && !self.mode_inhibited(self.csr.mcyclecfg)
                    && !std::mem::take(&mut self.cycle_written)
                {
                    self.csr.mcycle = self.csr.mcycle.wrapping_add(1);
                }
                if self.csr.active_hpm_mask != 0 {
                    self.step_hpm_counters();
                } else {
                    self.hpm_written_mask = 0;
                }
                if self.csr.active_trigger_mask != 0 && !self.icount_written {
                    self.step_icount_triggers(step_prv);
                }
            }
            Err(e) => {
                self.instret_written = false;
                self.cycle_written = false;
                self.icount_written = false;
                self.hpm_written_mask = 0;
                self.trap_exception(e);
            }
        }
    }

    #[cold]
    fn check_exec_triggers(&mut self) -> Result<(), Exception> {
        let pc = self.pc;
        let prv = self.prv;
        for i in 0..csr::TRIGGER_COUNT {
            let t1 = self.csr.tdata1[i];
            if (t1 >> csr::trig::TYPE_SHIFT) == csr::trig::TYPE_ICOUNT
                && (t1 & csr::trig::ICOUNT_PENDING) != 0
                && self.trigger_reentrancy_ok(prv)
            {
                self.csr.tdata1[i] = (t1 & !csr::trig::ICOUNT_PENDING) | csr::trig::ICOUNT_HIT;
                self.csr.update_active_trigger_mask();
                return Err(Exception::new(cause::BREAKPOINT, pc));
            }
        }
        let mut chain_ok = true;
        for i in 0..csr::TRIGGER_COUNT {
            let t1 = self.csr.tdata1[i];
            let ty = t1 >> csr::trig::TYPE_SHIFT;
            if !matches!(ty, csr::trig::TYPE_MCONTROL | csr::trig::TYPE_MCONTROL6) {
                chain_ok = true;
                continue;
            }
            let chain = (t1 & csr::trig::CHAIN) != 0;
            let matched = chain_ok
                && (t1 & csr::trig::EXECUTE) != 0
                && self.trigger_mode_and_context_match(t1, self.csr.tdata3[i], prv)
                && Self::trigger_addr_match(t1, self.csr.tdata2[i], pc);
            if chain {
                chain_ok = matched;
            } else {
                chain_ok = true;
                if matched && self.trigger_reentrancy_ok(prv) {
                    return Err(Exception::new(cause::BREAKPOINT, pc));
                }
            }
        }
        Ok(())
    }

    #[cold]
    fn step_icount_triggers(&mut self, prv: Priv) {
        let mode_bit = match (prv, self.virt) {
            (Priv::Machine, _) => csr::trig::ICOUNT_M,
            (Priv::Supervisor, false) => csr::trig::ICOUNT_S,
            (Priv::User, false) => csr::trig::ICOUNT_U,
            (Priv::Supervisor, true) => csr::trig::ICOUNT_VS,
            (Priv::User, true) => csr::trig::ICOUNT_VU,
        };
        for i in 0..csr::TRIGGER_COUNT {
            let t1 = self.csr.tdata1[i];
            if (t1 >> csr::trig::TYPE_SHIFT) != csr::trig::TYPE_ICOUNT {
                continue;
            }
            if (t1 & mode_bit) == 0 || !self.trigger_context_match(self.csr.tdata3[i]) {
                continue;
            }
            let count = (t1 & csr::trig::ICOUNT_COUNT_MASK) >> csr::trig::ICOUNT_COUNT_SHIFT;
            if count > 0 {
                let next_count = count - 1;
                let mut next_t1 = (t1 & !csr::trig::ICOUNT_COUNT_MASK)
                    | (next_count << csr::trig::ICOUNT_COUNT_SHIFT);
                if next_count == 0 {
                    next_t1 |= csr::trig::ICOUNT_PENDING;
                }
                self.csr.tdata1[i] = next_t1;
                self.csr.update_active_trigger_mask();
            }
        }
    }

    #[inline]
    fn trigger_reentrancy_ok(&self, prv: Priv) -> bool {
        match prv {
            Priv::Machine => (self.csr.mstatus & status::MIE) != 0,
            Priv::Supervisor if (self.csr.medeleg >> cause::BREAKPOINT) & 1 != 0 => {
                (self.csr.mstatus & status::SIE) != 0
            }
            _ => true,
        }
    }

    #[inline]
    fn trigger_context_match(&self, t3: u64) -> bool {
        let sselect = t3 & 3;
        if sselect == 1 && self.csr.scontext != ((t3 >> 2) & 0xffff_ffff) {
            return false;
        }
        let mhselect = (t3 >> 48) & 7;
        if matches!(mhselect, 1 | 4) && self.csr.mcontext != (t3 >> 51) {
            return false;
        }
        true
    }

    #[inline]
    fn trigger_mode_and_context_match(&self, t1: u64, t3: u64, prv: Priv) -> bool {
        // With V = 1 only mcontrol6 matches, by its vs/vu bits (as QEMU).
        let mode_bit = match (prv, self.virt) {
            (Priv::Machine, _) => csr::trig::M,
            (Priv::Supervisor, false) => csr::trig::S,
            (Priv::User, false) => csr::trig::U,
            _ if t1 >> csr::trig::TYPE_SHIFT != csr::trig::TYPE_MCONTROL6 => return false,
            (Priv::Supervisor, true) => csr::trig::VS6,
            (Priv::User, true) => csr::trig::VU6,
        };
        (t1 & mode_bit) != 0 && self.trigger_context_match(t3)
    }

    #[inline]
    fn trigger_addr_match(t1: u64, t2: u64, addr: u64) -> bool {
        let m = (t1 & csr::trig::MATCH_MASK) >> csr::trig::MATCH_SHIFT;
        let invert = (m & 8) != 0;
        let base = match m & 7 {
            0 => addr == t2,
            1 => {
                let ones = t2.trailing_ones();
                if ones >= 63 {
                    true
                } else {
                    let mask = !((1u64 << (ones + 1)) - 1);
                    (addr & mask) == (t2 & mask)
                }
            }
            2 => addr >= t2,
            3 => addr < t2,
            4 => {
                let mask = t2 >> 32;
                (addr & 0xffff_ffff & mask) == (t2 & 0xffff_ffff & mask)
            }
            5 => {
                let mask = t2 >> 32;
                ((addr >> 32) & mask) == (t2 & 0xffff_ffff & mask)
            }
            _ => false,
        };
        base ^ invert
    }

    #[inline]
    pub(crate) fn check_data_triggers(
        &mut self,
        va: u64,
        len: u64,
        is_load: bool,
        is_store: bool,
    ) -> Result<(), Exception> {
        if self.csr.active_trigger_mask == 0 && self.watchpoints.is_empty() {
            return Ok(());
        }
        self.check_data_triggers_slow(va, len, is_load, is_store)
    }

    #[cold]
    fn check_data_triggers_slow(
        &mut self,
        va: u64,
        len: u64,
        is_load: bool,
        is_store: bool,
    ) -> Result<(), Exception> {
        if self.csr.active_trigger_mask != 0 {
            let access_mask = (if is_load { csr::trig::LOAD } else { 0 })
                | (if is_store { csr::trig::STORE } else { 0 });
            let prv = self.prv;
            let mut chain_ok = true;
            for i in 0..csr::TRIGGER_COUNT {
                let t1 = self.csr.tdata1[i];
                let ty = t1 >> csr::trig::TYPE_SHIFT;
                if !matches!(ty, csr::trig::TYPE_MCONTROL | csr::trig::TYPE_MCONTROL6) {
                    chain_ok = true;
                    continue;
                }
                let chain = (t1 & csr::trig::CHAIN) != 0;
                let matched = chain_ok
                    && (t1 & access_mask) != 0
                    && self.trigger_mode_and_context_match(t1, self.csr.tdata3[i], prv)
                    && Self::trigger_addr_match(t1, self.csr.tdata2[i], va);
                if chain {
                    chain_ok = matched;
                } else {
                    chain_ok = true;
                    if matched && self.trigger_reentrancy_ok(prv) {
                        return Err(Exception::new(cause::BREAKPOINT, va));
                    }
                }
            }
        }
        if !self.watchpoints.is_empty() {
            let req_kind = (if is_load { 1u8 } else { 0 }) | (if is_store { 2u8 } else { 0 });
            let va_end = va.saturating_add(len.max(1) - 1);
            for &(wp_addr, wp_len, wp_kind) in &self.watchpoints {
                if (wp_kind & req_kind) != 0 {
                    let wp_end = wp_addr.saturating_add(wp_len.max(1) - 1);
                    if va <= wp_end && va_end >= wp_addr {
                        self.hit_breakpoint = true;
                        self.yield_slice = true;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// PMA check for the cache block at `pa`, after the PMP check (Sail
    /// `phys_access_check`): cbo.clean/flush/inval need a readable or
    /// writable region and cbo.zero needs RAM; otherwise the instruction
    /// raises a store/AMO access fault (Zicbom/Zicboz).
    fn cbo_pma_check(bus: &AddressSpace, pa: u64, va: u64, zero: bool) -> Result<(), Exception> {
        let ok = match bus.find(pa).map(|r| &r.kind) {
            None => false,
            Some(RegionKind::Ram(_)) => true,
            Some(RegionKind::Rom(_) | RegionKind::Mmio(_)) => !zero,
        };
        if ok {
            Ok(())
        } else {
            Err(Exception::new(cause::STORE_ACCESS, va))
        }
    }

    /// Whether the Sscofpmf/Smcntrpmf filter `cfg` stops counting in the
    /// current mode (VS/VU-mode use VSINH/VUINH).
    #[inline]
    fn mode_inhibited(&self, cfg: u64) -> bool {
        let bit = match (self.prv, self.virt) {
            (Priv::Machine, _) => csr::hpmevent::MINH,
            (Priv::Supervisor, false) => csr::hpmevent::SINH,
            (Priv::User, false) => csr::hpmevent::UINH,
            (Priv::Supervisor, true) => csr::hpmevent::VSINH,
            (Priv::User, true) => csr::hpmevent::VUINH,
        };
        cfg & bit != 0
    }

    /// The writable mode filters of `mhpmevent`, `mcyclecfg` and
    /// `minstretcfg`.
    fn hpm_filter_mask(&self) -> u64 {
        if self.h_ext {
            csr::hpmevent::FILTER_MASK_H
        } else {
            csr::hpmevent::FILTER_MASK
        }
    }

    #[cold]
    fn step_hpm_counters(&mut self) {
        let written = std::mem::take(&mut self.hpm_written_mask);
        let mut mask = self.csr.active_hpm_mask & !written;
        while mask != 0 {
            let idx = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            let i = idx - 3;
            let ev = self.csr.mhpmevent[i];
            if self.mode_inhibited(ev) {
                continue;
            }
            let (next, overflowed) = self.csr.mhpmcounter[i].overflowing_add(1);
            self.csr.mhpmcounter[i] = next;
            if overflowed && ev & csr::hpmevent::OF == 0 {
                self.csr.mhpmevent[i] |= csr::hpmevent::OF;
                self.csr.mip |= irq::LCOFIP;
                self.check_irq = true;
            }
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
        if self.h_ext {
            return self.h_select_interrupt(pending);
        }
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
        if self.vstimer_pending() {
            v |= irq::VSTIP;
        }
        v
    }

    /// Enters a trap handler.
    pub fn trap(&mut self, code: u64, interrupt: bool, tval: u64) {
        self.trap_full(code, interrupt, tval, 0, false);
    }

    /// Takes the trap for exception `e`, with its H-extension values.
    #[inline]
    pub(crate) fn trap_exception(&mut self, e: Exception) {
        self.trap_full(e.cause, false, e.tval, e.tval2, e.gva);
    }

    /// Enters a trap handler. `tval2` (`htval`/`mtval2`) and `gva`
    /// (`hstatus.GVA`/`mstatus.GVA`) matter only with the H extension.
    fn trap_full(&mut self, code: u64, interrupt: bool, tval: u64, tval2: u64, gva: bool) {
        let old_pc = self.pc;
        let old_prv = self.prv;
        let old_virt = self.virt;
        self.reservation = None;
        self.wfi = false;
        let deleg = if interrupt {
            self.mideleg_eff()
        } else {
            self.csr.medeleg
        };
        let delegated_to_s = self.prv <= Priv::Supervisor && (deleg >> code) & 1 != 0;
        let hdeleg = if interrupt {
            self.csr.h.hideleg
        } else {
            self.csr.h.hedeleg
        };
        let to_vs = delegated_to_s && old_virt && (hdeleg >> code) & 1 != 0;
        // Ssdbltrp: VS-mode uses henvcfg.DTE and vsstatus.SDT; a trap from
        // V = 1 into HS-mode uses HS-mode's SDT, which is in the bank.
        let dte = if to_vs {
            self.csr.h.henvcfg & envcfg::DTE != 0
        } else {
            self.csr.menvcfg & envcfg::DTE != 0
        };
        let sdt = if old_virt && !to_vs {
            self.csr.h.bank.status & status::SDT != 0
        } else {
            self.csr.mstatus & status::SDT != 0
        };
        let s_double_trap = delegated_to_s && dte && sdt;
        let (code, interrupt, tval, to_s) = if s_double_trap {
            (
                cause::DOUBLE_TRAP,
                false,
                if interrupt { 0 } else { tval },
                false,
            )
        } else {
            (code, interrupt, tval, delegated_to_s)
        };
        let to_vs = to_vs && to_s;
        // Smdbltrp: a trap into M-mode while MDT = 1 is an unexpected trap.
        // Without Smrnmi "the hart enters a critical-error state without
        // updating any architectural state, including the pc".
        if self.smdbltrp && !to_s && self.csr.mstatus & status::MDT != 0 {
            self.critical_error = true;
            return;
        }
        // VS-mode sees its interrupts as the S-level ones (VSSI -> SSI, ...).
        let code = if to_vs && interrupt && matches!(code, irq::VSSI | irq::VSTI | irq::VSEI) {
            code - 1
        } else {
            code
        };
        if self.h_ext && !to_vs {
            // Leaving V = 1 restores HS-mode's S-level CSRs first.
            self.set_virt(false);
            let h = &mut self.csr.h;
            if to_s {
                h.hstatus &= !(hstatus::SPV | hstatus::SPVP | hstatus::GVA);
                if old_virt {
                    h.hstatus |= hstatus::SPV;
                    if old_prv == Priv::Supervisor {
                        h.hstatus |= hstatus::SPVP;
                    }
                }
                if gva {
                    h.hstatus |= hstatus::GVA;
                }
                h.htval = tval2;
                h.htinst = 0;
            } else {
                self.csr.mstatus &= !(status::MPV | status::GVA);
                if old_virt {
                    self.csr.mstatus |= status::MPV;
                }
                if gva {
                    self.csr.mstatus |= status::GVA;
                }
                h.mtval2 = tval2;
                h.mtinst = 0;
            }
        }
        let cause = code | (u64::from(interrupt) << 63);
        let save_elp = self.elp && self.csr.landing_pad_enabled(old_prv);
        self.elp = false;
        let s = &mut self.csr.mstatus;
        if to_s {
            self.csr.scause = cause;
            self.csr.sepc = self.pc;
            self.csr.stval = tval;
            let sie = *s & status::SIE != 0;
            *s &= !(status::SPIE | status::SIE | status::SPP | status::SPELP);
            if sie {
                *s |= status::SPIE;
            }
            if self.prv == Priv::Supervisor {
                *s |= status::SPP;
            }
            if save_elp {
                *s |= status::SPELP;
            }
            if dte {
                *s |= status::SDT;
            }
            self.prv = Priv::Supervisor;
            self.pc = trap_vector(self.csr.stvec, code, interrupt);
        } else {
            self.csr.mcause = cause;
            self.csr.mepc = self.pc;
            self.csr.mtval = tval;
            let mie = *s & status::MIE != 0;
            *s &= !(status::MPIE | status::MIE | status::MPP | status::MPELP);
            if mie {
                *s |= status::MPIE;
            }
            *s |= (self.prv as u64) << status::MPP_SHIFT;
            if save_elp {
                *s |= status::MPELP;
            }
            // Smdbltrp: taking a trap into M-mode sets MDT.
            if self.smdbltrp {
                *s |= status::MDT;
            }
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
            let pa = if !self.pmp_subpage && self.fetch_vpn == vpn_tag {
                self.fetch_pa_page | (pc & 0xfff)
            } else {
                let pa = self.translate(bus, pc, Access::Fetch)?;
                if pa >> 48 == 0 {
                    self.fetch_vpn = vpn_tag;
                    self.fetch_pa_page = pa & !0xfff;
                }
                pa
            };
            if pa >> 48 == 0 {
                let idx = decode_cache_index(pa, ICACHE_SIZE);
                let tag = (pa << 16) | u64::from(self.icache_gen);
                let entry = &self.icache[idx];
                if entry.tag == tag {
                    if self.pmp_subpage {
                        self.check_pmp_access(pc, pa, u64::from(entry.insn.len), Access::Fetch)?;
                    }
                    return Ok(entry.insn);
                }
                let entry = self.populate_icache_same_page(bus, pc, pa, idx, tag)?;
                if self.log_flags.has(LogFlags::IN_ASM) {
                    self.log_in_asm(pc, pa, &entry.insn);
                }
                return Ok(entry.insn);
            }
        }
        let insn = self.fetch_slow(bus, pc)?;
        if self.log_flags.has(LogFlags::IN_ASM) {
            self.log_in_asm(pc, pc, &insn);
        }
        Ok(insn)
    }

    #[cold]
    fn populate_icache_same_page(
        &mut self,
        bus: &AddressSpace,
        pc: u64,
        pa: u64,
        idx: usize,
        tag: u64,
    ) -> Result<IcacheEntry, Exception> {
        let insn = self.fetch_decode_same_page(bus, pc, pa)?;
        let fused = if !self.pmp_subpage
            && insn.rd != 0
            && matches!(insn.op, Op::Auipc | Op::Lui | Op::Slli)
            && (pc & 0xfff) + u64::from(insn.len) + 4 <= 0x1000
        {
            let len1 = u64::from(insn.len);
            match self.fetch_decode_same_page(bus, pc + len1, pa + len1) {
                Ok(next) => try_fuse(&insn, &next),
                Err(_) => FusedOp::None,
            }
        } else {
            FusedOp::None
        };
        let entry = IcacheEntry { tag, insn, fused };
        self.icache[idx] = entry;
        Ok(entry)
    }

    #[cold]
    fn fetch_decode_same_page(
        &self,
        bus: &AddressSpace,
        pc: u64,
        pa: u64,
    ) -> Result<Insn, Exception> {
        self.check_pmp_access(pc, pa, 2, Access::Fetch)?;
        let lo = bus
            .read(pa, AccessSize::Half)
            .map_err(|_| Exception::new(cause::INSN_ACCESS, pc))? as u16;
        if lo & 3 != 3 {
            return Ok(decode_compressed(lo));
        }
        self.check_pmp_access(pc + 2, pa + 2, 2, Access::Fetch)?;
        let hi = bus
            .read(pa + 2, AccessSize::Half)
            .map_err(|_| Exception::new(cause::INSN_ACCESS, pc + 2))?;
        Ok(decode(u32::from(lo) | (hi as u32) << 16))
    }

    #[cold]
    fn fetch_slow(&mut self, bus: &AddressSpace, pc: u64) -> Result<Insn, Exception> {
        let pa = self.translate(bus, pc, Access::Fetch)?;
        self.check_pmp_access(pc, pa, 2, Access::Fetch)?;
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
        self.check_pmp_access(pc + 2, pa_hi, 2, Access::Fetch)?;
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
        let va = self.mask_data_addr(va);
        let n = size.bytes();
        self.check_data_triggers(va, n, true, false)?;
        if self.strict_align && va & (n - 1) != 0 {
            return Err(Exception::new(cause::LOAD_MISALIGNED, va));
        }
        if (va & 0xfff) + n > 0x1000 {
            return self.load_page_cross(bus, va, n);
        }
        if let Some(off) = self.fast_load_ram_off(va) {
            if let Some(v) = bus.read_fast_ram(off, size) {
                return Ok(v);
            }
        }
        let (pa, ram_page_off) = self.translate_with_ram_off(bus, va, Access::Load)?;
        self.check_pmp_access(va, pa, n, Access::Load)?;
        if ram_page_off != u64::MAX {
            if let Some(v) = bus.read_fast_ram(ram_page_off | (pa & 0xfff), size) {
                self.fast_tlb_fill(va, pa, ram_page_off, false);
                return Ok(v);
            }
        }
        if pa < 0x8000_0000 {
            self.nondeterministic_read = true;
        }
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
        let va = self.mask_data_addr(va);
        let n = size.bytes();
        self.check_data_triggers(va, n, false, true)?;
        if self.strict_align && va & (n - 1) != 0 {
            return Err(Exception::new(cause::STORE_MISALIGNED, va));
        }
        if (va & 0xfff) + n > 0x1000 {
            return self.store_page_cross(bus, va, n, value);
        }
        if let Some((pa, off)) = self.fast_store_ram_off(va) {
            self.invalidate_reservation(pa);
            if bus.write_fast_ram(pa, off, size, value).is_some() {
                return Ok(());
            }
        }
        let (pa, ram_page_off) = self.translate_with_ram_off(bus, va, Access::Store)?;
        self.check_pmp_access(va, pa, n, Access::Store)?;
        self.invalidate_reservation(pa);
        if ram_page_off != u64::MAX
            && bus
                .write_fast_ram(pa, ram_page_off | (pa & 0xfff), size, value)
                .is_some()
        {
            self.fast_tlb_fill(va, pa, ram_page_off, true);
            return Ok(());
        }
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
        if self.reservation.is_some_and(|r| r.pa & !63 == pa & !63) {
            self.reservation = None;
        }
    }

    // ---------------------------------------------------------------------
    // Execute
    // ---------------------------------------------------------------------

    #[inline]
    fn illegal(insn: &Insn) -> Exception {
        Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw))
    }

    /// Whether a Zicbom/Zicboz instruction is enabled by the `envcfg` field
    /// `bits` (`CBIE`, `CBCFE` or `CBZE`), in QEMU's order: `menvcfg`
    /// (illegal instruction), then with V = 1 `henvcfg` and, in VU-mode,
    /// `senvcfg` (virtual instruction), then `senvcfg` in U-mode (illegal).
    fn cbo_enabled(&self, bits: u64, insn: &Insn) -> Result<(), Exception> {
        if self.prv == Priv::Machine {
            return Ok(());
        }
        let c = &self.csr;
        if c.menvcfg & bits == 0 {
            return Err(Self::illegal(insn));
        }
        if self.virt
            && (c.h.henvcfg & bits == 0 || (self.prv == Priv::User && c.senvcfg & bits == 0))
        {
            return Err(Exception::new(cause::VIRTUAL_INSN, u64::from(insn.raw)));
        }
        if self.prv == Priv::User && c.senvcfg & bits == 0 {
            return Err(Self::illegal(insn));
        }
        Ok(())
    }

    fn execute(&mut self, bus: &AddressSpace, insn: &Insn) -> Result<(), Exception> {
        let pc = self.pc;
        if self.elp {
            if self.csr.landing_pad_enabled(self.prv) {
                let label = u64::from((insn.raw >> 12) & 0xfffff);
                let expected = (self.x[7] >> 12) & 0xfffff;
                let valid_lpad = insn.len == 4
                    && insn.op == Op::Auipc
                    && insn.rd == 0
                    && (pc & 3) == 0
                    && (label == 0 || label == expected);
                if !valid_lpad {
                    return Err(Exception::new(cause::SW_CHECK, 2));
                }
            }
            self.elp = false;
        }
        let next = pc.wrapping_add(u64::from(insn.len));
        let rs1 = self.x[usize::from(insn.rs1 & 31)];
        let rs2 = self.x[usize::from(insn.rs2 & 31)];
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
            Op::Nop => {}
            Op::Lui => self.set_x(rd, imm),
            Op::Auipc => self.set_x(rd, pc.wrapping_add(imm)),
            Op::Jal => {
                self.set_x(rd, next);
                new_pc = pc.wrapping_add(imm);
            }
            Op::Jalr => {
                new_pc = rs1.wrapping_add(imm) & !1;
                self.set_x(rd, next);
                if self.csr.landing_pad_enabled(self.prv) && !matches!(insn.rs1, 1 | 5 | 7) {
                    self.elp = true;
                }
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
                if self.handle_sbi && self.prv == Priv::Supervisor {
                    if !self.try_local_sbi() {
                        self.pending_sbi = true;
                        self.yield_slice = true;
                    }
                } else {
                    let c = match self.prv {
                        Priv::User => cause::ECALL_U,
                        Priv::Supervisor if self.virt => cause::ECALL_VS,
                        Priv::Supervisor => cause::ECALL_S,
                        Priv::Machine => cause::ECALL_M,
                    };
                    return Err(Exception::new(c, 0));
                }
            }
            Op::Ebreak => {
                if self.semihosting && insn.len == 4 && self.is_semihost_sequence(bus, pc) {
                    self.pending_semihost = true;
                    self.yield_slice = true;
                } else {
                    return Err(Exception::new(cause::BREAKPOINT, pc));
                }
            }

            // Zba (address generation).
            Op::AddUw => self.set_x(rd, rs2.wrapping_add(u64::from(rs1 as u32))),
            Op::Sh1add => self.set_x(rd, rs2.wrapping_add(rs1 << 1)),
            Op::Sh2add => self.set_x(rd, rs2.wrapping_add(rs1 << 2)),
            Op::Sh3add => self.set_x(rd, rs2.wrapping_add(rs1 << 3)),
            Op::Sh1addUw => self.set_x(rd, rs2.wrapping_add(u64::from(rs1 as u32) << 1)),
            Op::Sh2addUw => self.set_x(rd, rs2.wrapping_add(u64::from(rs1 as u32) << 2)),
            Op::Sh3addUw => self.set_x(rd, rs2.wrapping_add(u64::from(rs1 as u32) << 3)),
            Op::SlliUw => self.set_x(rd, u64::from(rs1 as u32) << (imm & 63)),

            // Zbb (basic bit-manipulation).
            Op::Andn => self.set_x(rd, rs1 & !rs2),
            Op::Orn => self.set_x(rd, rs1 | !rs2),
            Op::Xnor => self.set_x(rd, !(rs1 ^ rs2)),
            Op::Clz => self.set_x(rd, u64::from(rs1.leading_zeros())),
            Op::Ctz => self.set_x(rd, u64::from(rs1.trailing_zeros())),
            Op::Cpop => self.set_x(rd, u64::from(rs1.count_ones())),
            Op::Clzw => self.set_x(rd, u64::from((rs1 as u32).leading_zeros())),
            Op::Ctzw => self.set_x(rd, u64::from((rs1 as u32).trailing_zeros())),
            Op::Cpopw => self.set_x(rd, u64::from((rs1 as u32).count_ones())),
            Op::SextB => self.set_x(rd, rs1 as i8 as i64 as u64),
            Op::SextH => self.set_x(rd, rs1 as i16 as i64 as u64),
            Op::ZextH => self.set_x(rd, u64::from(rs1 as u16)),
            Op::Min => self.set_x(rd, (rs1 as i64).min(rs2 as i64) as u64),
            Op::Minu => self.set_x(rd, rs1.min(rs2)),
            Op::Max => self.set_x(rd, (rs1 as i64).max(rs2 as i64) as u64),
            Op::Maxu => self.set_x(rd, rs1.max(rs2)),
            Op::Rol => self.set_x(rd, rs1.rotate_left((rs2 & 63) as u32)),
            Op::Ror => self.set_x(rd, rs1.rotate_right((rs2 & 63) as u32)),
            Op::Rolw => self.set_x(
                rd,
                sext32(u64::from((rs1 as u32).rotate_left((rs2 & 31) as u32))),
            ),
            Op::Rorw => self.set_x(
                rd,
                sext32(u64::from((rs1 as u32).rotate_right((rs2 & 31) as u32))),
            ),
            Op::Rori => self.set_x(rd, rs1.rotate_right((imm & 63) as u32)),
            Op::Roriw => self.set_x(
                rd,
                sext32(u64::from((rs1 as u32).rotate_right((imm & 31) as u32))),
            ),
            Op::OrcB => {
                let mut out = 0u64;
                for i in 0..8 {
                    if (rs1 >> (i * 8)) & 0xff != 0 {
                        out |= 0xff << (i * 8);
                    }
                }
                self.set_x(rd, out);
            }
            Op::Rev8 => self.set_x(rd, rs1.swap_bytes()),

            // Zbc (carry-less multiply).
            Op::Clmul => {
                let mut out = 0u64;
                for i in 0..64 {
                    if (rs2 >> i) & 1 != 0 {
                        out ^= rs1 << i;
                    }
                }
                self.set_x(rd, out);
            }
            Op::Clmulh => {
                let mut out = 0u64;
                for i in 1..64 {
                    if (rs2 >> i) & 1 != 0 {
                        out ^= rs1 >> (64 - i);
                    }
                }
                self.set_x(rd, out);
            }
            Op::Clmulr => {
                let mut out = 0u64;
                for i in 0..64 {
                    if (rs2 >> i) & 1 != 0 {
                        out ^= rs1 >> (63 - i);
                    }
                }
                self.set_x(rd, out);
            }

            // Zbs (single-bit operations).
            Op::Bclr => self.set_x(rd, rs1 & !(1u64 << (rs2 & 63))),
            Op::Bext => self.set_x(rd, (rs1 >> (rs2 & 63)) & 1),
            Op::Binv => self.set_x(rd, rs1 ^ (1u64 << (rs2 & 63))),
            Op::Bset => self.set_x(rd, rs1 | (1u64 << (rs2 & 63))),
            Op::Bclri => self.set_x(rd, rs1 & !(1u64 << (imm & 63))),
            Op::Bexti => self.set_x(rd, (rs1 >> (imm & 63)) & 1),
            Op::Binvi => self.set_x(rd, rs1 ^ (1u64 << (imm & 63))),
            Op::Bseti => self.set_x(rd, rs1 | (1u64 << (imm & 63))),

            // Zicond (integer conditional operations).
            Op::CzeroEqz => self.set_x(rd, if rs2 == 0 { 0 } else { rs1 }),
            Op::CzeroNez => self.set_x(rd, if rs2 != 0 { 0 } else { rs1 }),

            // Zbkb / Zbkx (cryptography bit-manipulation).
            Op::Pack => self.set_x(rd, (rs1 & 0xffff_ffff) | (rs2 << 32)),
            Op::Packh => self.set_x(rd, (rs1 & 0xff) | ((rs2 & 0xff) << 8)),
            Op::Packw => self.set_x(rd, sext32((rs1 & 0xffff) | ((rs2 & 0xffff) << 16))),
            Op::Brev8 => {
                let mut out = 0u64;
                for i in 0..8 {
                    let b = ((rs1 >> (i * 8)) & 0xff) as u8;
                    out |= u64::from(b.reverse_bits()) << (i * 8);
                }
                self.set_x(rd, out);
            }
            Op::Xperm4 => {
                let mut out = 0u64;
                for i in 0..16 {
                    let idx = (rs2 >> (i * 4)) & 0xf;
                    let nib = (rs1 >> (idx * 4)) & 0xf;
                    out |= nib << (i * 4);
                }
                self.set_x(rd, out);
            }
            Op::Xperm8 => {
                let mut out = 0u64;
                for i in 0..8 {
                    let idx = (rs2 >> (i * 8)) & 0xff;
                    if idx < 8 {
                        let b = (rs1 >> (idx * 8)) & 0xff;
                        out |= b << (i * 8);
                    }
                }
                self.set_x(rd, out);
            }

            // Zknd / Zkne (NIST AES scalar cryptography).
            Op::Aes64es => self.set_x(rd, aes64_shift_sub(rs1, rs2, false)),
            Op::Aes64esm => {
                let v = aes64_shift_sub(rs1, rs2, false);
                let lo = aes_mix_col(v as u32);
                let hi = aes_mix_col((v >> 32) as u32);
                self.set_x(rd, u64::from(lo) | (u64::from(hi) << 32));
            }
            Op::Aes64ds => self.set_x(rd, aes64_shift_sub(rs1, rs2, true)),
            Op::Aes64dsm => {
                let v = aes64_shift_sub(rs1, rs2, true);
                let lo = aes_inv_mix_col(v as u32);
                let hi = aes_inv_mix_col((v >> 32) as u32);
                self.set_x(rd, u64::from(lo) | (u64::from(hi) << 32));
            }
            Op::Aes64im => {
                let lo = aes_inv_mix_col(rs1 as u32);
                let hi = aes_inv_mix_col((rs1 >> 32) as u32);
                self.set_x(rd, u64::from(lo) | (u64::from(hi) << 32));
            }
            Op::Aes64ks1i => self.set_x(rd, aes64_ks1i(rs1, (imm as u32) & 0xf)),
            Op::Aes64ks2 => {
                let w0 = ((rs1 >> 32) as u32) ^ (rs2 as u32);
                let w1 = w0 ^ ((rs2 >> 32) as u32);
                self.set_x(rd, u64::from(w0) | (u64::from(w1) << 32));
            }

            // Zknh (NIST SHA-256 / SHA-512 scalar cryptography).
            Op::Sha256sum0 => {
                let x = rs1 as u32;
                let v = x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22);
                self.set_x(rd, sext32(u64::from(v)));
            }
            Op::Sha256sum1 => {
                let x = rs1 as u32;
                let v = x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25);
                self.set_x(rd, sext32(u64::from(v)));
            }
            Op::Sha256sig0 => {
                let x = rs1 as u32;
                let v = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
                self.set_x(rd, sext32(u64::from(v)));
            }
            Op::Sha256sig1 => {
                let x = rs1 as u32;
                let v = x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10);
                self.set_x(rd, sext32(u64::from(v)));
            }
            Op::Sha512sum0 => {
                self.set_x(
                    rd,
                    rs1.rotate_right(28) ^ rs1.rotate_right(34) ^ rs1.rotate_right(39),
                );
            }
            Op::Sha512sum1 => {
                self.set_x(
                    rd,
                    rs1.rotate_right(14) ^ rs1.rotate_right(18) ^ rs1.rotate_right(41),
                );
            }
            Op::Sha512sig0 => {
                self.set_x(rd, rs1.rotate_right(1) ^ rs1.rotate_right(8) ^ (rs1 >> 7));
            }
            Op::Sha512sig1 => {
                self.set_x(rd, rs1.rotate_right(19) ^ rs1.rotate_right(61) ^ (rs1 >> 6));
            }

            // Zksed / Zksh (SM4 / SM3 ShangMi scalar cryptography).
            Op::Sm4ed => {
                let shamt = ((insn.imm as u32) & 3) * 8;
                let x = u32::from(SM4_SBOX[((rs2 >> shamt) & 0xff) as usize]);
                let t =
                    x ^ (x << 8) ^ (x << 2) ^ (x << 18) ^ ((x & 0x3f) << 26) ^ ((x & 0xc0) << 10);
                let z = t.rotate_left(shamt);
                self.set_x(rd, sext32(u64::from(z ^ (rs1 as u32))));
            }
            Op::Sm4ks => {
                let shamt = ((insn.imm as u32) & 3) * 8;
                let x = u32::from(SM4_SBOX[((rs2 >> shamt) & 0xff) as usize]);
                let t = x
                    ^ ((x & 0x07) << 29)
                    ^ ((x & 0xfe) << 7)
                    ^ ((x & 0x01) << 23)
                    ^ ((x & 0xf8) << 13);
                let z = t.rotate_left(shamt);
                self.set_x(rd, sext32(u64::from(z ^ (rs1 as u32))));
            }
            Op::Sm3p0 => {
                let x = rs1 as u32;
                let v = x ^ x.rotate_left(9) ^ x.rotate_left(17);
                self.set_x(rd, sext32(u64::from(v)));
            }
            Op::Sm3p1 => {
                let x = rs1 as u32;
                let v = x ^ x.rotate_left(15) ^ x.rotate_left(23);
                self.set_x(rd, sext32(u64::from(v)));
            }

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

            // A / Zacas / Zabha / Zalasr extensions.
            Op::Lr | Op::Sc | Op::Amo | Op::AmoCas | Op::LoadAq | Op::StoreRl => {
                self.exec_atomic(bus, insn, rs1, rs2)?;
            }

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
                let mpelp = s & status::MPELP != 0;
                let to_virt = self.h_ext && s & status::MPV != 0 && mpp != Priv::Machine;
                let mut n =
                    s & !(status::MIE | status::MPP | status::MPELP | status::MDT | status::MPV);
                if s & status::MPIE != 0 {
                    n |= status::MIE;
                }
                n |= status::MPIE;
                if mpp != Priv::Machine {
                    n &= !status::MPRV;
                    if mpp == Priv::User {
                        n &= !status::SDT;
                    }
                }
                self.csr.mstatus = n;
                if to_virt {
                    self.set_virt(true);
                }
                self.prv = mpp;
                self.elp = mpelp && self.csr.landing_pad_enabled(mpp);
                self.reservation = None;
                self.fetch_vpn = 0;
                self.check_irq = true;
                new_pc = self.csr.mepc;
            }
            Op::Sret => {
                let s = self.csr.mstatus;
                if self.virt
                    && (self.prv < Priv::Supervisor || self.csr.h.hstatus & hstatus::VTSR != 0)
                {
                    return Err(Exception::new(cause::VIRTUAL_INSN, u64::from(insn.raw)));
                }
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
                let spelp = s & status::SPELP != 0;
                let mut n =
                    s & !(status::SIE | status::SPP | status::MPRV | status::SPELP | status::SDT);
                if s & status::SPIE != 0 {
                    n |= status::SIE;
                }
                n |= status::SPIE;
                self.csr.mstatus = n;
                // The return address is the current mode's sepc: read it
                // before a switch to V = 1 swaps in the VS copy.
                new_pc = self.csr.sepc;
                // From HS-mode, sret returns to hstatus.SPV's mode and clears
                // SPV (as QEMU does); from VS-mode it stays in V = 1.
                if self.h_ext && !self.virt {
                    let spv = self.csr.h.hstatus & hstatus::SPV != 0;
                    self.csr.h.hstatus &= !hstatus::SPV;
                    if spv {
                        self.set_virt(true);
                    }
                }
                self.prv = spp;
                self.elp = spelp && self.csr.landing_pad_enabled(spp);
                self.reservation = None;
                self.fetch_vpn = 0;
                self.check_irq = true;
            }
            Op::Wfi => {
                let s = self.csr.mstatus;
                if (self.prv == Priv::User && !self.virt)
                    || (self.prv == Priv::Supervisor && s & status::TW != 0)
                {
                    return Err(Self::illegal(insn));
                }
                if self.virt && (self.prv == Priv::User || self.csr.h.hstatus & hstatus::VTW != 0) {
                    return Err(Exception::new(cause::VIRTUAL_INSN, u64::from(insn.raw)));
                }
                if self.mip() & self.csr.mie == 0 {
                    self.wfi = true;
                } else {
                    self.check_irq = true;
                    self.yield_slice = true;
                }
            }
            Op::WrsNto => {
                // Zawrs: with V = 1, hstatus.VTW (and TW = 0) makes it a
                // virtual instruction (QEMU raises it at once).
                if self.virt
                    && self.csr.h.hstatus & hstatus::VTW != 0
                    && self.csr.mstatus & status::TW == 0
                {
                    return Err(Exception::new(cause::VIRTUAL_INSN, u64::from(insn.raw)));
                }
                if self.prv != Priv::Machine && self.csr.mstatus & status::TW != 0 {
                    return Err(Self::illegal(insn));
                }
                self.reservation = None;
                self.yield_slice = true;
            }
            Op::WrsSto => {
                self.reservation = None;
                self.yield_slice = true;
            }
            Op::SfenceVma | Op::SinvalVma => {
                // mstatus.TVM governs HS-mode, hstatus.VTVM VS-mode; from
                // VU-mode it is a virtual instruction.
                if self.virt {
                    if self.prv == Priv::User || self.csr.h.hstatus & hstatus::VTVM != 0 {
                        return Err(Exception::new(cause::VIRTUAL_INSN, u64::from(insn.raw)));
                    }
                } else if self.prv == Priv::User
                    || (self.prv == Priv::Supervisor && self.csr.mstatus & status::TVM != 0)
                {
                    return Err(Self::illegal(insn));
                }
                if self.virt {
                    // VS-stage entries are not tagged by VMID or VS-ASID.
                    self.flush_tlb();
                } else {
                    self.sfence_vma(insn.rs1, insn.rs2, rs1, rs2);
                }
            }
            Op::SfenceWInval | Op::SfenceInvalIr => {
                if self.prv == Priv::User {
                    return Err(if self.virt {
                        Exception::new(cause::VIRTUAL_INSN, u64::from(insn.raw))
                    } else {
                        Self::illegal(insn)
                    });
                }
                self.fetch_vpn = 0;
            }
            Op::HfenceVvma | Op::HfenceGvma => {
                self.h_insn_check(insn.raw, true)?;
                if insn.op == Op::HfenceGvma
                    && self.prv == Priv::Supervisor
                    && self.csr.mstatus & status::TVM != 0
                {
                    return Err(Self::illegal(insn));
                }
                self.flush_tlb();
            }
            Op::Hlv => {
                self.h_insn_check(insn.raw, false)?;
                let v = self.hyp_load(bus, rs1, insn.funct)?;
                self.set_x(rd, v);
            }
            Op::Hsv => {
                self.h_insn_check(insn.raw, false)?;
                self.hyp_store(bus, rs1, insn.funct, rs2)?;
            }

            // Zicbom / Zicboz (64-byte cache-block operations).
            Op::CboInval => {
                self.cbo_enabled(envcfg::CBIE_MASK, insn)?;
                let va = self.mask_data_addr(rs1);
                let base_va = va & !63;
                let pa = self
                    .translate(bus, base_va, Access::CacheBom)
                    .map_err(|e| Exception { tval: va, ..e })?;
                self.check_pmp_access(va, pa, 64, Access::CacheBom)?;
                Self::cbo_pma_check(bus, pa, va, false)?;
                self.invalidate_reservation(pa);
                bus.invalidate_cache_line(pa, AccessSize::Double);
            }
            Op::CboClean | Op::CboFlush => {
                self.cbo_enabled(envcfg::CBCFE, insn)?;
                let va = self.mask_data_addr(rs1);
                let base_va = va & !63;
                let pa = self
                    .translate(bus, base_va, Access::CacheBom)
                    .map_err(|e| Exception { tval: va, ..e })?;
                self.check_pmp_access(va, pa, 64, Access::CacheBom)?;
                Self::cbo_pma_check(bus, pa, va, false)?;
                self.invalidate_reservation(pa);
                bus.invalidate_cache_line(pa, AccessSize::Double);
            }
            Op::CboZero => {
                self.cbo_enabled(envcfg::CBZE, insn)?;
                let va = self.mask_data_addr(rs1);
                let base_va = va & !63;
                let pa = self
                    .translate(bus, base_va, Access::Store)
                    .map_err(|e| Exception { tval: va, ..e })?;
                self.check_pmp_access(va, pa, 64, Access::Store)?;
                Self::cbo_pma_check(bus, pa, va, true)?;
                self.invalidate_reservation(pa);
                bus.invalidate_cache_line(pa, AccessSize::Double);
                for off in (0..64u64).step_by(8) {
                    bus.write(pa + off, AccessSize::Double, 0)
                        .map_err(|_| Exception::new(cause::STORE_ACCESS, va))?;
                }
            }

            // Zimop / Zcmop / Zicfiss extensions.
            Op::Mop => self.set_x(rd, 0),
            Op::Ssrdp => {
                let v = if self.csr.shadow_stack_enabled(self.prv) {
                    self.csr.ssp
                } else {
                    0
                };
                self.set_x(rd, v);
            }
            Op::Sspush => {
                if self.csr.shadow_stack_enabled(self.prv) {
                    let addr = self.csr.ssp.wrapping_sub(8);
                    let va = self.mask_data_addr(addr);
                    if va & 7 != 0 {
                        return Err(Exception::new(cause::STORE_ACCESS, va));
                    }
                    let pa = self.translate(bus, va, Access::ShadowStackStore)?;
                    self.check_pmp_access(va, pa, 8, Access::ShadowStackStore)?;
                    self.invalidate_reservation(pa);
                    bus.write(pa, AccessSize::Double, rs2)
                        .map_err(|_| Exception::new(cause::STORE_ACCESS, va))?;
                    self.csr.ssp = addr;
                }
            }
            Op::Sspopchk => {
                if self.csr.shadow_stack_enabled(self.prv) {
                    let addr = self.csr.ssp;
                    let va = self.mask_data_addr(addr);
                    if va & 7 != 0 {
                        return Err(Exception::new(cause::STORE_ACCESS, va));
                    }
                    let pa = self.translate(bus, va, Access::ShadowStackLoad)?;
                    self.check_pmp_access(va, pa, 8, Access::ShadowStackLoad)?;
                    let val = bus
                        .read(pa, AccessSize::Double)
                        .map_err(|_| Exception::new(cause::STORE_ACCESS, va))?;
                    if val != rs1 {
                        return Err(Exception::new(cause::SW_CHECK, 3));
                    }
                    self.csr.ssp = addr.wrapping_add(8);
                }
            }
            Op::Ssamoswap => {
                if !self.csr.shadow_stack_enabled(self.prv) {
                    return Err(Self::illegal(insn));
                }
                let va = self.mask_data_addr(rs1);
                let size = if insn.imm == 2 {
                    AccessSize::Word
                } else {
                    AccessSize::Double
                };
                let n = size.bytes();
                if va & (n - 1) != 0 {
                    return Err(Exception::new(cause::STORE_ACCESS, va));
                }
                let pa = self.translate(bus, va, Access::ShadowStackStore)?;
                self.check_pmp_access(va, pa, n, Access::ShadowStackStore)?;
                let b_u = if size == AccessSize::Word {
                    u64::from(rs2 as u32)
                } else {
                    rs2
                };
                let old = bus
                    .atomic_rmw(pa, size, |_| b_u)
                    .map_err(|_| Exception::new(cause::STORE_ACCESS, va))?;
                self.invalidate_reservation(pa);
                let res = if size == AccessSize::Word {
                    sext32(old)
                } else {
                    old
                };
                self.set_x(rd, res);
            }

            // V extension.
            Op::Vload | Op::Vstore | Op::Vop => self.exec_vector(bus, insn)?,

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
        let addr = self.mask_data_addr(addr);
        if insn.op == Op::AmoCas && insn.imm == 4 {
            self.check_data_triggers(addr, 16, true, true)?;
            if addr & 15 != 0 {
                return Err(Exception::new(cause::STORE_MISALIGNED, addr));
            }
            let pa = self.translate(bus, addr, Access::Store)?;
            self.check_pmp_access(addr, pa, 16, Access::Store)?;
            let cmp = if insn.rd == 0 {
                (0, 0)
            } else {
                (
                    self.x[usize::from(insn.rd)],
                    self.x[usize::from(insn.rd + 1)],
                )
            };
            let new = if insn.rs2 == 0 {
                (0, 0)
            } else {
                (
                    self.x[usize::from(insn.rs2)],
                    self.x[usize::from(insn.rs2 + 1)],
                )
            };
            let (old_lo, old_hi) = bus
                .atomic_cas128(pa, cmp, new)
                .map_err(|_| Exception::new(cause::STORE_ACCESS, addr))?;
            self.invalidate_reservation(pa);
            self.invalidate_reservation(pa.wrapping_add(8));
            if insn.rd != 0 {
                self.set_x(insn.rd, old_lo);
                self.set_x(insn.rd + 1, old_hi);
            }
            return Ok(());
        }

        let size = match insn.imm {
            0 => AccessSize::Byte,
            1 => AccessSize::Half,
            2 => AccessSize::Word,
            _ => AccessSize::Double,
        };
        let sext = |v: u64| match size {
            AccessSize::Byte => v as i8 as i64 as u64,
            AccessSize::Half => v as i16 as i64 as u64,
            AccessSize::Word => sext32(v),
            AccessSize::Double => v,
        };
        let zext = |v: u64| match size {
            AccessSize::Byte => u64::from(v as u8),
            AccessSize::Half => u64::from(v as u16),
            AccessSize::Word => u64::from(v as u32),
            AccessSize::Double => v,
        };
        let (is_load, is_store) = match insn.op {
            Op::Lr | Op::LoadAq => (true, false),
            Op::Sc | Op::StoreRl => (false, true),
            _ => (true, true),
        };
        self.check_data_triggers(addr, size.bytes(), is_load, is_store)?;
        // Zama16b: a misaligned AMO (Zaamo, Zabha, Zacas up to doubleword)
        // that stays inside a naturally aligned 16-byte granule executes
        // atomically. Other misaligned AMOs, and misaligned LR/SC and
        // Zalasr accesses, raise address-misaligned.
        let misaligned = addr & (size.bytes() - 1) != 0;
        if misaligned
            && (matches!(insn.op, Op::Lr | Op::Sc | Op::LoadAq | Op::StoreRl)
                || (addr & 15) + size.bytes() > 16)
        {
            let c = if matches!(insn.op, Op::Lr | Op::LoadAq) {
                cause::LOAD_MISALIGNED
            } else {
                cause::STORE_MISALIGNED
            };
            return Err(Exception::new(c, addr));
        }
        let amo_rmw = |pa: u64, f: &mut dyn FnMut(u64) -> u64| {
            if misaligned {
                bus.atomic_rmw_granule(pa, size, f)
            } else {
                bus.atomic_rmw(pa, size, f)
            }
        };
        match insn.op {
            Op::Lr => {
                let pa = self.translate(bus, addr, Access::Load)?;
                self.check_pmp_access(addr, pa, size.bytes(), Access::Load)?;
                let seq = bus.reserve_cache_line(pa);
                let v = bus
                    .read(pa, size)
                    .map_err(|_| Exception::new(cause::LOAD_ACCESS, addr))?;
                self.reservation = Some(Reservation {
                    pa,
                    size,
                    val: v,
                    seq,
                });
                self.set_x(insn.rd, sext(v));
            }
            Op::Sc => {
                let pa = self.translate(bus, addr, Access::Store)?;
                self.check_pmp_access(addr, pa, size.bytes(), Access::Store)?;
                let reservation = match self.reservation.take() {
                    Some(r) if r.pa == pa && r.size == size => Some((r.seq, r.val)),
                    _ => None,
                };
                let ok = bus
                    .store_conditional(pa, size, reservation, src)
                    .map_err(|_| Exception::new(cause::STORE_ACCESS, addr))?;
                self.set_x(insn.rd, u64::from(!ok));
            }
            Op::LoadAq => {
                let pa = self.translate(bus, addr, Access::Load)?;
                self.check_pmp_access(addr, pa, size.bytes(), Access::Load)?;
                let v = bus
                    .read(pa, size)
                    .map_err(|_| Exception::new(cause::LOAD_ACCESS, addr))?;
                std::sync::atomic::fence(Ordering::SeqCst);
                self.set_x(insn.rd, sext(v));
            }
            Op::StoreRl => {
                let pa = self.translate(bus, addr, Access::Store)?;
                self.check_pmp_access(addr, pa, size.bytes(), Access::Store)?;
                std::sync::atomic::fence(Ordering::SeqCst);
                bus.write(pa, size, src)
                    .map_err(|_| Exception::new(cause::STORE_ACCESS, addr))?;
                self.invalidate_reservation(pa);
            }
            Op::AmoCas => {
                let pa = self.translate(bus, addr, Access::Store)?;
                self.check_pmp_access(addr, pa, size.bytes(), Access::Store)?;
                let cmp = zext(self.x[usize::from(insn.rd)]);
                let new_val = zext(src);
                let old = amo_rmw(pa, &mut |old_raw| {
                    if zext(old_raw) == cmp {
                        new_val
                    } else {
                        old_raw
                    }
                })
                .map_err(|_| Exception::new(cause::STORE_ACCESS, addr))?;
                self.invalidate_reservation(pa);
                self.set_x(insn.rd, sext(old));
            }
            _ => {
                let pa = self.translate(bus, addr, Access::Store)?;
                self.check_pmp_access(addr, pa, size.bytes(), Access::Store)?;
                let b_s = sext(src) as i64;
                let b_u = zext(src);
                let funct = insn.funct;
                let old = amo_rmw(pa, &mut |old_raw| {
                    let a_s = sext(old_raw) as i64;
                    let a_u = zext(old_raw);
                    match funct {
                        amo::SWAP => b_u,
                        amo::ADD => a_u.wrapping_add(b_u),
                        amo::XOR => a_u ^ b_u,
                        amo::AND => a_u & b_u,
                        amo::OR => a_u | b_u,
                        amo::MIN => a_s.min(b_s) as u64,
                        amo::MAX => a_s.max(b_s) as u64,
                        amo::MINU => a_u.min(b_u),
                        _ => a_u.max(b_u),
                    }
                })
                .map_err(|_| Exception::new(cause::STORE_ACCESS, addr))?;
                self.invalidate_reservation(pa);
                self.set_x(insn.rd, sext(old));
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
        if self.h_ext {
            self.h_csr_check(num, writes, insn.raw)?;
        } else {
            if (self.prv as u16) < (num >> 8) & 3 {
                return Err(illegal());
            }
            if writes && num >> 10 == 3 {
                return Err(illegal());
            }
        }
        if num == csr::SEED && !writes {
            return Err(illegal());
        }
        let reads = !matches!(insn.op, Op::Csrrw | Op::Csrrwi) || insn.rd != 0;
        let old = if reads || writes {
            self.csr_read(num).ok_or_else(illegal)?
        } else {
            0
        };
        if reads
            && matches!(
                num,
                csr::SEED
                    | csr::TINFO
                    | csr::MIP
                    | csr::SIP
                    | 0xb00..=0xb1f
                    | 0xc00..=0xc1f
                    | 0xda0
            )
        {
            self.nondeterministic_read = true;
        }
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

    /// Floating-point state is on: `mstatus.FS`, and with V = 1 also the
    /// HS-level `sstatus.FS` (kept in the bank while V = 1).
    pub(crate) fn fs_enabled(&self) -> bool {
        self.csr.mstatus & status::FS != 0
            && (!self.virt || self.csr.h.bank.status & status::FS != 0)
    }

    /// Vector state is on (as [`Self::fs_enabled`], for `VS`).
    pub(crate) fn vs_enabled(&self) -> bool {
        self.csr.mstatus & status::VS != 0
            && (!self.virt || self.csr.h.bank.status & status::VS != 0)
    }

    /// Marks the floating-point state dirty, in both `vsstatus` and the
    /// HS-level `sstatus` while V = 1.
    #[inline]
    pub(crate) fn mark_fs_dirty(&mut self) {
        self.csr.mstatus |= status::FS;
        if self.virt {
            self.csr.h.bank.status |= status::FS;
        }
    }

    /// Marks the vector state dirty (as [`Self::mark_fs_dirty`]).
    #[inline]
    pub(crate) fn mark_vs_dirty(&mut self) {
        self.csr.mstatus |= status::VS;
        if self.virt {
            self.csr.h.bank.status |= status::VS;
        }
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
        if self.h_ext {
            if let Some(v) = self.h_csr_read(num) {
                return v;
            }
        }
        let c = &self.csr;
        Some(match num {
            csr::FFLAGS | csr::FRM | csr::FCSR if !self.fs_enabled() => return None,
            csr::FFLAGS => c.fflags,
            csr::FRM => c.frm,
            csr::FCSR => (c.frm << 5) | c.fflags,
            csr::VSTART
            | csr::VXSAT
            | csr::VXRM
            | csr::VCSR
            | csr::VL
            | csr::VTYPE
            | csr::VLENB
                if !self.vs_enabled() =>
            {
                return None;
            }
            csr::VSTART => c.vstart,
            csr::VXSAT => c.vxsat & 1,
            csr::VXRM => c.vxrm & 3,
            csr::VCSR => ((c.vxrm & 3) << 1) | (c.vxsat & 1),
            csr::VL => c.vl,
            csr::VTYPE => c.vtype,
            csr::VLENB => csr::VLENB_VAL,
            csr::SSP => {
                if self.prv != Priv::Machine && !c.shadow_stack_enabled(self.prv) {
                    return None;
                }
                c.ssp
            }
            csr::SEED => {
                let ok = match self.prv {
                    Priv::Machine => true,
                    Priv::Supervisor => c.mseccfg & csr::mseccfg::SSEED != 0,
                    Priv::User => c.mseccfg & csr::mseccfg::USEED != 0,
                };
                if !ok {
                    return None;
                }
                let entropy = (c.minstret.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 48) & 0xffff;
                0x8000_0000 | entropy
            }
            csr::CYCLE..=csr::HPMCOUNTER31 => {
                let idx = num - csr::CYCLE;
                if !self.counter_enabled(idx) {
                    return None;
                }
                match num {
                    csr::CYCLE => c.mcycle,
                    csr::INSTRET => c.minstret,
                    csr::TIME if self.virt => self.timebase.now().wrapping_add(c.h.htimedelta),
                    csr::TIME => self.timebase.now(),
                    _ => c.mhpmcounter[(num - csr::HPMCOUNTER3) as usize],
                }
            }
            csr::SCOUNTOVF => {
                let mut ovf = 0u64;
                // In VS-mode hcounteren masks the bits too (as QEMU).
                let en = if self.prv == Priv::Machine {
                    u64::MAX
                } else if self.virt {
                    c.mcounteren & c.h.hcounteren
                } else {
                    c.mcounteren
                };
                for i in 0..csr::HPM_COUNTERS {
                    let bit = 1u64 << (i + 3);
                    if en & bit != 0 && c.mhpmevent[i] & csr::hpmevent::OF != 0 {
                        ovf |= bit;
                    }
                }
                ovf
            }
            csr::SSTATUS => self.read_mstatus() & status::S_READABLE,
            csr::SIE => c.mie & c.mideleg,
            csr::STVEC => c.stvec,
            csr::SCOUNTEREN => c.scounteren,
            csr::SENVCFG => {
                if self.prv != Priv::Machine && (c.mstateen[0] & csr::stateen::ENVCFG) == 0 {
                    return None;
                }
                let mut v = c.senvcfg;
                if c.menvcfg & envcfg::SSE == 0 {
                    v &= !envcfg::SSE;
                }
                v
            }
            csr::SSTATEEN0..=csr::SSTATEEN3 => {
                let i = (num - csr::SSTATEEN0) as usize;
                if self.prv != Priv::Machine && (c.mstateen[i] & csr::stateen::SE) == 0 {
                    return None;
                }
                c.sstateen[i] & c.mstateen[i]
            }
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
                // mstatus.TVM traps HS-mode only (VS-mode has hstatus.VTVM).
                if self.prv == Priv::Supervisor && !self.virt && c.mstatus & status::TVM != 0 {
                    return None;
                }
                c.satp
            }
            csr::SRMCFG => {
                if self.prv != Priv::Machine && (c.mstateen[0] & csr::stateen::SRMCFG) == 0 {
                    return None;
                }
                c.srmcfg
            }
            csr::SCONTEXT => {
                if self.prv != Priv::Machine && (c.mstateen[0] & csr::stateen::CONTEXT) == 0 {
                    return None;
                }
                c.scontext
            }
            csr::MVENDORID | csr::MARCHID | csr::MIMPID | csr::MCONFIGPTR => 0,
            csr::MHARTID => self.id,
            csr::MSTATUS => self.read_mstatus(),
            csr::MISA => csr::MISA_VALUE | if self.h_ext { 1 << 7 } else { 0 },
            csr::MEDELEG => c.medeleg,
            csr::MIDELEG => c.mideleg,
            csr::MIE => c.mie,
            csr::MTVEC => c.mtvec,
            csr::MCOUNTEREN => c.mcounteren,
            csr::MENVCFG => c.menvcfg,
            csr::MSTATEEN0..=csr::MSTATEEN3 => c.mstateen[(num - csr::MSTATEEN0) as usize],
            csr::MCOUNTINHIBIT => c.mcountinhibit,
            csr::MCYCLECFG => c.mcyclecfg,
            csr::MINSTRETCFG => c.minstretcfg,
            csr::MHPMEVENT3..=csr::MHPMEVENT31 => c.mhpmevent[(num - csr::MHPMEVENT3) as usize],
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
            csr::MSECCFG => c.mseccfg,
            csr::TSELECT => c.tselect,
            csr::TDATA1 => c.tdata1[c.tselect as usize],
            csr::TDATA2 => c.tdata2[c.tselect as usize],
            csr::TDATA3 => c.tdata3[c.tselect as usize],
            csr::TINFO => csr::trig::TINFO_VAL,
            csr::MCONTEXT => c.mcontext,
            csr::MCYCLE => c.mcycle,
            csr::MINSTRET => c.minstret,
            csr::MHPMCOUNTER3..=csr::MHPMCOUNTER31 => {
                c.mhpmcounter[(num - csr::MHPMCOUNTER3) as usize]
            }
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
        if s & status::FS == status::FS || s & status::VS == status::VS {
            s | status::SD
        } else {
            s & !status::SD
        }
    }

    /// Writes a CSR; `None` if it does not exist or is inaccessible.
    pub fn csr_write(&mut self, num: u16, v: u64) -> Option<()> {
        if self.h_ext {
            if let Some(r) = self.h_csr_write(num, v) {
                return r;
            }
        }
        match num {
            csr::FFLAGS | csr::FRM | csr::FCSR if !self.fs_enabled() => return None,
            csr::FFLAGS => {
                self.csr.fflags = v & 0x1f;
                self.mark_fs_dirty();
            }
            csr::FRM => {
                self.csr.frm = v & 7;
                self.mark_fs_dirty();
            }
            csr::FCSR => {
                self.csr.fflags = v & 0x1f;
                self.csr.frm = (v >> 5) & 7;
                self.mark_fs_dirty();
            }
            csr::VSTART | csr::VXSAT | csr::VXRM | csr::VCSR if !self.vs_enabled() => return None,
            csr::VSTART => {
                self.csr.vstart = v & ((csr::VLEN as u64) - 1);
                self.mark_vs_dirty();
            }
            csr::VXSAT => {
                self.csr.vxsat = v & 1;
                self.mark_vs_dirty();
            }
            csr::VXRM => {
                self.csr.vxrm = v & 3;
                self.mark_vs_dirty();
            }
            csr::VCSR => {
                self.csr.vxsat = v & 1;
                self.csr.vxrm = (v >> 1) & 3;
                self.mark_vs_dirty();
            }
            csr::SSP => {
                if self.prv != Priv::Machine && !self.csr.shadow_stack_enabled(self.prv) {
                    return None;
                }
                self.csr.ssp = v & !3;
            }
            csr::SEED => {
                let ok = match self.prv {
                    Priv::Machine => true,
                    Priv::Supervisor => self.csr.mseccfg & csr::mseccfg::SSEED != 0,
                    Priv::User => self.csr.mseccfg & csr::mseccfg::USEED != 0,
                };
                if !ok {
                    return None;
                }
            }
            csr::SSTATUS => {
                let mut m = status::S_WRITABLE;
                // VS-mode's sstatus is vsstatus: its SDT needs henvcfg.DTE.
                let dte = if self.virt {
                    self.csr.henvcfg_eff()
                } else {
                    self.csr.menvcfg
                };
                if dte & envcfg::DTE == 0 {
                    m &= !status::SDT;
                }
                let mut n = (self.csr.mstatus & !m) | (v & m);
                if v & m & status::SDT != 0 {
                    n &= !status::SIE;
                }
                self.csr.mstatus = n;
            }
            csr::SIE => {
                let m = self.csr.mideleg & irq::S_MASK;
                self.csr.mie = (self.csr.mie & !m) | (v & m);
            }
            csr::STVEC => {
                if v & 2 == 0 {
                    self.csr.stvec = v;
                }
            }
            csr::SCOUNTEREN => self.csr.scounteren = v & 0xffff_ffff,
            csr::SENVCFG => {
                if self.prv != Priv::Machine && (self.csr.mstateen[0] & csr::stateen::ENVCFG) == 0 {
                    return None;
                }
                let mut mask = envcfg::S_MASK;
                // senvcfg.SSE needs menvcfg.SSE, and from VS-mode also
                // henvcfg.SSE (as QEMU).
                if self.csr.menvcfg & envcfg::SSE == 0
                    || (self.virt && self.csr.h.henvcfg & envcfg::SSE == 0)
                {
                    mask &= !envcfg::SSE;
                }
                self.csr.senvcfg = envcfg::sanitize(v, self.csr.senvcfg, mask);
                self.flush_tlb();
            }
            csr::SSTATEEN0..=csr::SSTATEEN3 => {
                let i = (num - csr::SSTATEEN0) as usize;
                if self.prv != Priv::Machine && (self.csr.mstateen[i] & csr::stateen::SE) == 0 {
                    return None;
                }
                self.csr.sstateen[i] = 0;
            }
            csr::SSCRATCH => self.csr.sscratch = v,
            csr::SEPC => self.csr.sepc = v & !1,
            csr::SCAUSE => self.csr.scause = v,
            csr::STVAL => self.csr.stval = v,
            csr::SIP => {
                let m = self.csr.mideleg & (irq::SSIP | irq::LCOFIP);
                self.csr.mip = (self.csr.mip & !m) | (v & m);
            }
            csr::STIMECMP => {
                if !self.stimecmp_accessible() {
                    return None;
                }
                self.csr.stimecmp = v;
            }
            csr::SATP => {
                if self.prv == Priv::Supervisor && !self.virt && self.csr.mstatus & status::TVM != 0
                {
                    return None;
                }
                let mode = v >> 60;
                if matches!(mode, 0 | 8 | 9 | 10) {
                    let old_mode = self.csr.satp >> 60;
                    // MODE, ASID (ASIDLEN = 16) and PPN cover all 64 bits.
                    self.csr.satp = v;
                    if old_mode != mode && old_mode != 0 && mode != 0 {
                        self.flush_tlb();
                    } else {
                        self.fetch_vpn = 0;
                        // The main TLB is ASID-tagged; the fast path is not.
                        self.tlb.fast.invalidate();
                    }
                }
            }
            csr::SRMCFG => {
                if self.prv != Priv::Machine && (self.csr.mstateen[0] & csr::stateen::SRMCFG) == 0 {
                    return None;
                }
                self.csr.srmcfg = v & csr::SRMCFG_MASK;
            }
            csr::SCONTEXT => {
                if self.prv != Priv::Machine && (self.csr.mstateen[0] & csr::stateen::CONTEXT) == 0
                {
                    return None;
                }
                self.csr.scontext = v & 0xffff_ffff;
            }
            csr::MVENDORID | csr::MARCHID | csr::MIMPID | csr::MHARTID | csr::MCONFIGPTR => {
                return None;
            }
            csr::MSTATUS => {
                let mut m = status::M_WRITABLE;
                if self.h_ext {
                    m |= status::MPV | status::GVA;
                }
                if self.csr.menvcfg & envcfg::DTE == 0 {
                    m &= !status::SDT;
                }
                if !self.smdbltrp {
                    m &= !status::MDT;
                }
                let mut n = (self.csr.mstatus & !m) | (v & m);
                if v & m & status::SDT != 0 {
                    n &= !status::SIE;
                }
                if v & m & status::MDT != 0 {
                    n &= !status::MIE;
                }
                if (n >> status::MPP_SHIFT) & 3 == 2 {
                    // Reserved MPP value: keep the old one (WARL).
                    n = (n & !status::MPP) | (self.csr.mstatus & status::MPP);
                }
                self.csr.mstatus = n;
            }
            csr::MISA => {}
            csr::MEDELEG => {
                let mask = if self.h_ext {
                    csr::MEDELEG_MASK_H
                } else {
                    csr::MEDELEG_MASK
                };
                self.csr.medeleg = v & mask;
            }
            csr::MIDELEG => self.csr.mideleg = v & irq::S_MASK,
            csr::MIE => {
                let mask = if self.h_ext {
                    irq::ALL | irq::HS_FORCED
                } else {
                    irq::ALL
                };
                self.csr.mie = v & mask;
            }
            csr::MTVEC => {
                if v & 2 == 0 {
                    self.csr.mtvec = v;
                }
            }
            csr::MCOUNTEREN => self.csr.mcounteren = v & 0xffff_ffff,
            csr::MENVCFG => {
                self.csr.menvcfg = envcfg::sanitize(v, self.csr.menvcfg, envcfg::M_MASK);
                if self.csr.menvcfg & envcfg::SSE == 0 {
                    self.csr.senvcfg &= !envcfg::SSE;
                }
                if self.csr.menvcfg & envcfg::DTE == 0 {
                    self.csr.mstatus &= !status::SDT;
                }
                self.flush_tlb();
            }
            csr::MSTATEEN0..=csr::MSTATEEN3 => {
                let i = (num - csr::MSTATEEN0) as usize;
                let mask = if i == 0 {
                    csr::stateen::M0_MASK
                } else {
                    csr::stateen::M1_3_MASK
                };
                self.csr.mstateen[i] = v & mask;
                if i == 0 {
                    self.csr.sstateen[0] &= self.csr.mstateen[0];
                }
            }
            csr::MCOUNTINHIBIT => {
                self.csr.mcountinhibit = v & 0xffff_fffd;
                self.csr.update_active_hpm_mask();
            }
            csr::MCYCLECFG => self.csr.mcyclecfg = v & self.hpm_filter_mask(),
            csr::MINSTRETCFG => self.csr.minstretcfg = v & self.hpm_filter_mask(),
            csr::MHPMEVENT3..=csr::MHPMEVENT31 => {
                let i = (num - csr::MHPMEVENT3) as usize;
                self.csr.mhpmevent[i] =
                    v & (csr::hpmevent::MHPMEVENT_MASK | self.hpm_filter_mask());
                self.csr.update_active_hpm_mask();
            }
            csr::MSCRATCH => self.csr.mscratch = v,
            csr::MEPC => self.csr.mepc = v & !1,
            csr::MCAUSE => self.csr.mcause = v,
            csr::MTVAL => self.csr.mtval = v,
            csr::MIP => {
                let mut m = irq::SSIP | irq::SEIP | irq::LCOFIP;
                if self.csr.menvcfg & envcfg::STCE == 0 {
                    m |= irq::STIP;
                }
                if self.h_ext {
                    m |= irq::VSSIP | irq::VSEIP;
                    if self.csr.menvcfg & self.csr.h.henvcfg & envcfg::STCE == 0 {
                        m |= irq::VSTIP;
                    }
                }
                self.csr.mip = (self.csr.mip & !m) | (v & m);
            }
            csr::PMPCFG0..=csr::PMPCFG15 => {
                let n = (num - csr::PMPCFG0) as usize;
                if n % 2 == 1 {
                    return None;
                }
                let rlb = (self.csr.mseccfg & csr::mseccfg::RLB) != 0;
                let mml = (self.csr.mseccfg & csr::mseccfg::MML) != 0;
                for i in 0..8 {
                    let e = n * 4 + i;
                    if e >= csr::PMP_ENTRIES || (!rlb && self.csr.pmpcfg[e] & 0x80 != 0) {
                        continue;
                    }
                    let b = (v >> (8 * i)) as u8 & 0x9f;
                    if mml && !rlb {
                        // Under Smepmp with MML=1 and RLB=0, adding a locked rule
                        // that grants M-mode execute or has R=0, W=1, X=0 is ignored.
                        let idx =
                            (((b >> 4) & 8) | ((b & 1) << 2) | (b & 2) | ((b >> 2) & 1)) as u32;
                        if (0xd1ffu32 >> idx) & 1 == 0 {
                            continue;
                        }
                    } else if !mml && !rlb && (b & 3) == 2 {
                        // R=0, W=1 is reserved when MML=0 and RLB=0: ignore the write.
                        continue;
                    }
                    self.csr.pmpcfg[e] = b;
                }
                self.update_pmp_state();
                self.flush_tlb();
            }
            csr::PMPADDR0..=csr::PMPADDR63 => {
                let n = (num - csr::PMPADDR0) as usize;
                let rlb = (self.csr.mseccfg & csr::mseccfg::RLB) != 0;
                let tor_locked =
                    !rlb && n + 1 < csr::PMP_ENTRIES && (self.csr.pmpcfg[n + 1] & 0x98) == 0x88;
                if n < csr::PMP_ENTRIES && (rlb || self.csr.pmpcfg[n] & 0x80 == 0) && !tor_locked {
                    self.csr.pmpaddr[n] = v & ((1 << 54) - 1);
                    self.update_pmp_state();
                    self.flush_tlb();
                }
            }
            csr::MSECCFG => {
                let old = self.csr.mseccfg;
                let sticky = csr::mseccfg::MML | csr::mseccfg::MMWP;
                let rw = csr::mseccfg::USEED | csr::mseccfg::SSEED | csr::mseccfg::MLPE;
                let mut next = (old & sticky) | (v & (sticky | rw));
                if v & csr::mseccfg::RLB != 0 {
                    let any_locked = self.csr.pmpcfg.iter().any(|&cfg| cfg & 0x80 != 0);
                    if (old & csr::mseccfg::RLB) != 0 || !any_locked {
                        next |= csr::mseccfg::RLB;
                    }
                }
                let pmm = v & csr::mseccfg::PMM_MASK;
                if pmm == (1 << csr::mseccfg::PMM_SHIFT) {
                    next |= old & csr::mseccfg::PMM_MASK;
                } else {
                    next |= pmm;
                }
                self.csr.mseccfg = next;
                self.update_pmp_state();
                self.flush_tlb();
            }
            csr::TSELECT => {
                if (v as usize) < csr::TRIGGER_COUNT {
                    self.csr.tselect = v;
                }
            }
            csr::TDATA1 => {
                let idx = self.csr.tselect as usize;
                let ty = v >> csr::trig::TYPE_SHIFT;
                match ty {
                    0 => {
                        let prev_ty = self.csr.tdata1[idx] >> csr::trig::TYPE_SHIFT;
                        self.csr.tdata1[idx] = prev_ty << csr::trig::TYPE_SHIFT;
                    }
                    csr::trig::TYPE_MCONTROL | csr::trig::TYPE_MCONTROL6 => {
                        let mut rw_mask = csr::trig::LOAD
                            | csr::trig::STORE
                            | csr::trig::EXECUTE
                            | csr::trig::U
                            | csr::trig::S
                            | csr::trig::M
                            | csr::trig::MATCH_MASK
                            | csr::trig::CHAIN
                            | csr::trig::SIZE_MASK;
                        if self.h_ext && ty == csr::trig::TYPE_MCONTROL6 {
                            rw_mask |= csr::trig::VS6 | csr::trig::VU6;
                        }
                        self.csr.tdata1[idx] = (ty << csr::trig::TYPE_SHIFT) | (v & rw_mask);
                    }
                    csr::trig::TYPE_ICOUNT => {
                        let mut rw_mask = csr::trig::ICOUNT_U
                            | csr::trig::ICOUNT_S
                            | csr::trig::ICOUNT_PENDING
                            | csr::trig::ICOUNT_M
                            | csr::trig::ICOUNT_COUNT_MASK
                            | csr::trig::ICOUNT_HIT;
                        if self.h_ext {
                            rw_mask |= csr::trig::ICOUNT_VS | csr::trig::ICOUNT_VU;
                        }
                        self.csr.tdata1[idx] = (ty << csr::trig::TYPE_SHIFT) | (v & rw_mask);
                        self.icount_written = true;
                    }
                    csr::trig::TYPE_DISABLED => {
                        self.csr.tdata1[idx] = csr::trig::TYPE_DISABLED << csr::trig::TYPE_SHIFT;
                    }
                    _ => {}
                }
                self.csr.update_active_trigger_mask();
            }
            csr::TDATA2 => {
                self.csr.tdata2[self.csr.tselect as usize] = v;
            }
            csr::TDATA3 => {
                self.csr.tdata3[self.csr.tselect as usize] = v;
            }
            csr::TINFO => {}
            csr::MCONTEXT => {
                self.csr.mcontext = v & 0x1fff;
            }
            csr::MCYCLE => {
                self.csr.mcycle = v;
                self.cycle_written = true;
            }
            csr::MINSTRET => {
                self.csr.minstret = v;
                self.instret_written = true;
            }
            csr::MHPMCOUNTER3..=csr::MHPMCOUNTER31 => {
                let i = (num - csr::MHPMCOUNTER3) as usize;
                self.csr.mhpmcounter[i] = v;
                self.hpm_written_mask |= 1u32 << (i + 3);
            }
            _ => return None,
        }
        Some(())
    }

    /// Handles hart-local SBI calls (`BASE`, `TIME`, and `FWFT`) inline without
    /// leaving the inner execution loop. Returns `false` if the call requires
    /// machine-wide servicing (`IPI`, `RFENCE`, `HSM`, `SRST`, `DBCN`).
    fn try_local_sbi(&mut self) -> bool {
        let eid = self.x[17];
        let fid = self.x[16];
        let a0 = self.x[10];
        let a1 = self.x[11];
        let a2 = self.x[12];
        let (err, val): (i64, u64) = match (eid, fid) {
            // Base extension (0x10).
            (0x10, 0) => (0, 3 << 24), // SBI spec v3.0
            (0x10, 1) => (0, 4),       // Implementation ID (RustSBI / rivemu-sbi)
            (0x10, 2) => (0, 0x0001_0000),
            (0x10, 3) => {
                let supported = matches!(
                    a0,
                    0x10 | 0x5449_4d45
                        | 0x0073_5049
                        | 0x5246_4e43
                        | 0x0048_534d
                        | 0x5352_5354
                        | 0x4442_434e
                        | 0x4657_4654
                );
                (0, u64::from(supported))
            }
            (0x10, 4..=6) => (0, 0), // mvendorid / marchid / mimpid
            (0x10, _) => (-2, 0),    // SBI_ERR_NOT_SUPPORTED
            // Timer extension ("TIME" = 0x54494d45, or legacy 0x00).
            (0x5449_4d45, 0) | (0x00, _) => {
                self.csr.stimecmp = a0;
                self.csr.mip &= !irq::STIP;
                self.check_irq = true;
                (0, 0)
            }
            (0x5449_4d45, _) => (-2, 0),
            // Firmware Features extension ("FWFT" = 0x46574654).
            (0x4657_4654, 0) => match a0 {
                // SBI_FWFT_LANDING_PAD (1) / SBI_FWFT_SHADOW_STACK (2).
                1 | 2 => {
                    let (bit, lock) = if a0 == 1 {
                        (envcfg::LPE, 1)
                    } else {
                        (envcfg::SSE, 2)
                    };
                    if a1 > 1 || a2 & !1 != 0 {
                        (-3, 0) // SBI_ERR_INVALID_PARAM
                    } else if self.fwft_locks & lock != 0
                        && (self.csr.menvcfg & bit != 0) != (a1 != 0)
                    {
                        (-14, 0) // SBI_ERR_DENIED_LOCKED
                    } else {
                        if a1 == 1 {
                            self.csr.menvcfg |= bit;
                        } else {
                            self.csr.menvcfg &= !bit;
                            if a0 == 2 {
                                self.csr.senvcfg &= !envcfg::SSE;
                            }
                        }
                        if a2 & 1 != 0 {
                            self.fwft_locks |= lock;
                        }
                        self.flush_tlb();
                        (0, 0)
                    }
                }
                0 | 3..=5 => (-2, 0), // SBI_ERR_NOT_SUPPORTED
                _ => (-4, 0),         // SBI_ERR_DENIED for reserved features
            },
            (0x4657_4654, 1) => match a0 {
                1 => (0, u64::from(self.csr.menvcfg & envcfg::LPE != 0)),
                2 => (0, u64::from(self.csr.menvcfg & envcfg::SSE != 0)),
                0 | 3..=5 => (-2, 0),
                _ => (-4, 0),
            },
            (0x4657_4654, _) => (-2, 0),
            _ => return false,
        };
        self.x[10] = err as u64;
        self.x[11] = val;
        if self.log_flags.has(LogFlags::SBI) {
            self.log_sbi(eid, fid, a0, a1, err as u64, val);
        }
        true
    }

    /// Checks whether the `ebreak` at `pc` is bracketed by the RISC-V
    /// Semihosting magic sequence (`slli x0, x0, 0x1f` before and
    /// `srai x0, x0, 7` after).
    fn is_semihost_sequence(&self, bus: &AddressSpace, pc: u64) -> bool {
        if pc & 3 != 0 {
            return false;
        }
        let Some(pa_prev) = self.debug_translate(bus, pc.wrapping_sub(4)) else {
            return false;
        };
        let Some(pa_next) = self.debug_translate(bus, pc.wrapping_add(4)) else {
            return false;
        };
        let Ok(prev) = bus.read(pa_prev, AccessSize::Word) else {
            return false;
        };
        let Ok(next) = bus.read(pa_next, AccessSize::Word) else {
            return false;
        };
        prev == 0x01f0_1013 && next == 0x4070_5013
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

const AES_SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

const AES_INV_SBOX: [u8; 256] = [
    0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38, 0xbf, 0x40, 0xa3, 0x9e, 0x81, 0xf3, 0xd7, 0xfb,
    0x7c, 0xe3, 0x39, 0x82, 0x9b, 0x2f, 0xff, 0x87, 0x34, 0x8e, 0x43, 0x44, 0xc4, 0xde, 0xe9, 0xcb,
    0x54, 0x7b, 0x94, 0x32, 0xa6, 0xc2, 0x23, 0x3d, 0xee, 0x4c, 0x95, 0x0b, 0x42, 0xfa, 0xc3, 0x4e,
    0x08, 0x2e, 0xa1, 0x66, 0x28, 0xd9, 0x24, 0xb2, 0x76, 0x5b, 0xa2, 0x49, 0x6d, 0x8b, 0xd1, 0x25,
    0x72, 0xf8, 0xf6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xd4, 0xa4, 0x5c, 0xcc, 0x5d, 0x65, 0xb6, 0x92,
    0x6c, 0x70, 0x48, 0x50, 0xfd, 0xed, 0xb9, 0xda, 0x5e, 0x15, 0x46, 0x57, 0xa7, 0x8d, 0x9d, 0x84,
    0x90, 0xd8, 0xab, 0x00, 0x8c, 0xbc, 0xd3, 0x0a, 0xf7, 0xe4, 0x58, 0x05, 0xb8, 0xb3, 0x45, 0x06,
    0xd0, 0x2c, 0x1e, 0x8f, 0xca, 0x3f, 0x0f, 0x02, 0xc1, 0xaf, 0xbd, 0x03, 0x01, 0x13, 0x8a, 0x6b,
    0x3a, 0x91, 0x11, 0x41, 0x4f, 0x67, 0xdc, 0xea, 0x97, 0xf2, 0xcf, 0xce, 0xf0, 0xb4, 0xe6, 0x73,
    0x96, 0xac, 0x74, 0x22, 0xe7, 0xad, 0x35, 0x85, 0xe2, 0xf9, 0x37, 0xe8, 0x1c, 0x75, 0xdf, 0x6e,
    0x47, 0xf1, 0x1a, 0x71, 0x1d, 0x29, 0xc5, 0x89, 0x6f, 0xb7, 0x62, 0x0e, 0xaa, 0x18, 0xbe, 0x1b,
    0xfc, 0x56, 0x3e, 0x4b, 0xc6, 0xd2, 0x79, 0x20, 0x9a, 0xdb, 0xc0, 0xfe, 0x78, 0xcd, 0x5a, 0xf4,
    0x1f, 0xdd, 0xa8, 0x33, 0x88, 0x07, 0xc7, 0x31, 0xb1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xec, 0x5f,
    0x60, 0x51, 0x7f, 0xa9, 0x19, 0xb5, 0x4a, 0x0d, 0x2d, 0xe5, 0x7a, 0x9f, 0x93, 0xc9, 0x9c, 0xef,
    0xa0, 0xe0, 0x3b, 0x4d, 0xae, 0x2a, 0xf5, 0xb0, 0xc8, 0xeb, 0xbb, 0x3c, 0x83, 0x53, 0x99, 0x61,
    0x17, 0x2b, 0x04, 0x7e, 0xba, 0x77, 0xd6, 0x26, 0xe1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0c, 0x7d,
];

fn aes64_shift_sub(rs1: u64, rs2: u64, inv: bool) -> u64 {
    const FWD_SHIFT: [usize; 8] = [0, 5, 10, 15, 4, 9, 14, 3];
    const INV_SHIFT: [usize; 8] = [0, 13, 10, 7, 4, 1, 14, 11];
    let mut st = [0u8; 16];
    st[..8].copy_from_slice(&rs1.to_le_bytes());
    st[8..].copy_from_slice(&rs2.to_le_bytes());
    let (idx, sbox) = if inv {
        (&INV_SHIFT, &AES_INV_SBOX)
    } else {
        (&FWD_SHIFT, &AES_SBOX)
    };
    let mut out = [0u8; 8];
    for i in 0..8 {
        out[i] = sbox[st[idx[i]] as usize];
    }
    u64::from_le_bytes(out)
}

#[inline]
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

fn aes_mix_col(w: u32) -> u32 {
    let [s0, s1, s2, s3] = w.to_le_bytes();
    let r0 = gf_mul(s0, 2) ^ gf_mul(s1, 3) ^ s2 ^ s3;
    let r1 = s0 ^ gf_mul(s1, 2) ^ gf_mul(s2, 3) ^ s3;
    let r2 = s0 ^ s1 ^ gf_mul(s2, 2) ^ gf_mul(s3, 3);
    let r3 = gf_mul(s0, 3) ^ s1 ^ s2 ^ gf_mul(s3, 2);
    u32::from_le_bytes([r0, r1, r2, r3])
}

fn aes_inv_mix_col(w: u32) -> u32 {
    let [s0, s1, s2, s3] = w.to_le_bytes();
    let r0 = gf_mul(s0, 0x0e) ^ gf_mul(s1, 0x0b) ^ gf_mul(s2, 0x0d) ^ gf_mul(s3, 0x09);
    let r1 = gf_mul(s0, 0x09) ^ gf_mul(s1, 0x0e) ^ gf_mul(s2, 0x0b) ^ gf_mul(s3, 0x0d);
    let r2 = gf_mul(s0, 0x0d) ^ gf_mul(s1, 0x09) ^ gf_mul(s2, 0x0e) ^ gf_mul(s3, 0x0b);
    let r3 = gf_mul(s0, 0x0b) ^ gf_mul(s1, 0x0d) ^ gf_mul(s2, 0x09) ^ gf_mul(s3, 0x0e);
    u32::from_le_bytes([r0, r1, r2, r3])
}

fn aes64_ks1i(rs1: u64, rnum: u32) -> u64 {
    const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];
    let mut w = (rs1 >> 32) as u32;
    if rnum != 10 {
        w = w.rotate_right(8);
    }
    let mut b = w.to_le_bytes();
    for x in &mut b {
        *x = AES_SBOX[*x as usize];
    }
    if (rnum as usize) < 10 {
        b[0] ^= RCON[rnum as usize];
    }
    let s = u32::from_le_bytes(b);
    u64::from(s) | (u64::from(s) << 32)
}

const SM4_SBOX: [u8; 256] = [
    0xd6, 0x90, 0xe9, 0xfe, 0xcc, 0xe1, 0x3d, 0xb7, 0x16, 0xb6, 0x14, 0xc2, 0x28, 0xfb, 0x2c, 0x05,
    0x2b, 0x67, 0x9a, 0x76, 0x2a, 0xbe, 0x04, 0xc3, 0xaa, 0x44, 0x13, 0x26, 0x49, 0x86, 0x06, 0x99,
    0x9c, 0x42, 0x50, 0xf4, 0x91, 0xef, 0x98, 0x7a, 0x33, 0x54, 0x0b, 0x43, 0xed, 0xcf, 0xac, 0x62,
    0xe4, 0xb3, 0x1c, 0xa9, 0xc9, 0x08, 0xe8, 0x95, 0x80, 0xdf, 0x94, 0xfa, 0x75, 0x8f, 0x3f, 0xa6,
    0x47, 0x07, 0xa7, 0xfc, 0xf3, 0x73, 0x17, 0xba, 0x83, 0x59, 0x3c, 0x19, 0xe6, 0x85, 0x4f, 0xa8,
    0x68, 0x6b, 0x81, 0xb2, 0x71, 0x64, 0xda, 0x8b, 0xf8, 0xeb, 0x0f, 0x4b, 0x70, 0x56, 0x9d, 0x35,
    0x1e, 0x24, 0x0e, 0x5e, 0x63, 0x58, 0xd1, 0xa2, 0x25, 0x22, 0x7c, 0x3b, 0x01, 0x21, 0x78, 0x87,
    0xd4, 0x00, 0x46, 0x57, 0x9f, 0xd3, 0x27, 0x52, 0x4c, 0x36, 0x02, 0xe7, 0xa0, 0xc4, 0xc8, 0x9e,
    0xea, 0xbf, 0x8a, 0xd2, 0x40, 0xc7, 0x38, 0xb5, 0xa3, 0xf7, 0xf2, 0xce, 0xf9, 0x61, 0x15, 0xa1,
    0xe0, 0xae, 0x5d, 0xa4, 0x9b, 0x34, 0x1a, 0x55, 0xad, 0x93, 0x32, 0x30, 0xf5, 0x8c, 0xb1, 0xe3,
    0x1d, 0xf6, 0xe2, 0x2e, 0x82, 0x66, 0xca, 0x60, 0xc0, 0x29, 0x23, 0xab, 0x0d, 0x53, 0x4e, 0x6f,
    0xd5, 0xdb, 0x37, 0x45, 0xde, 0xfd, 0x8e, 0x2f, 0x03, 0xff, 0x6a, 0x72, 0x6d, 0x6c, 0x5b, 0x51,
    0x8d, 0x1b, 0xaf, 0x92, 0xbb, 0xdd, 0xbc, 0x7f, 0x11, 0xd9, 0x5c, 0x41, 0x1f, 0x10, 0x5a, 0xd8,
    0x0a, 0xc1, 0x31, 0x88, 0xa5, 0xcd, 0x7b, 0xbd, 0x2d, 0x74, 0xd0, 0x12, 0xb8, 0xe5, 0xb4, 0xb0,
    0x89, 0x69, 0x97, 0x4a, 0x0c, 0x96, 0x77, 0x7e, 0x65, 0xb9, 0xf1, 0x09, 0xc5, 0x6e, 0xc6, 0x84,
    0x18, 0xf0, 0x7d, 0xec, 0x3a, 0xdc, 0x4d, 0x20, 0x79, 0xee, 0x5f, 0x3e, 0xd7, 0xcb, 0x39, 0x48,
];

#[cfg(test)]
mod tests {
    use super::*;
    use rivemu_core::FrozenTimebase;
    use rivemu_core::size::MIB;
    use rivemu_mem::Ram;

    /// Runs `addi a0, a0, imm; j .+4` blocks from `at` until the `wfi` after
    /// them and returns `a0`.
    fn run_blocks(h: &mut Hart, bus: &AddressSpace, at: u64) -> u64 {
        h.wfi = false;
        h.pc = at;
        h.x[10] = 0;
        let mut attempted = 0;
        while !h.wfi && attempted < 1000 {
            attempted += h.run(bus, 100);
        }
        assert!(h.wfi);
        h.x[10]
    }

    /// Clearing the full block arena must also drop every index entry that
    /// points into it; otherwise a block decoded before the clear runs the
    /// instructions of whatever block reused its arena slots.
    #[test]
    fn clearing_the_block_arena_drops_index_entries() {
        // A and B use different index slots, so only a stale A entry can
        // point at B's instructions.
        const A: u64 = 0x8000_0000;
        const B: u64 = 0x8000_0100;
        let mut bus = AddressSpace::new();
        bus.add_ram("ram", A, Arc::new(Ram::new(MIB).unwrap()))
            .unwrap();
        for (base, imm) in [(A, 1u64), (B, 100)] {
            for i in 0..4 {
                let at = base + 8 * i;
                bus.write(at, AccessSize::Word, (imm << 20) | 0x0005_0513)
                    .unwrap(); // addi a0, a0, imm
                bus.write(at + 4, AccessSize::Word, 0x0040_006f).unwrap(); // j .+4
            }
            bus.write(base + 32, AccessSize::Word, 0x1050_0073).unwrap(); // wfi
        }
        let mut h = Hart::new(0, Arc::new(FrozenTimebase(0)));
        h.reset(A);
        assert_eq!(run_blocks(&mut h, &bus, A), 4);
        for i in 0..4 {
            assert_ne!(
                decode_cache_index(A + 8 * i, BBCACHE_SIZE),
                decode_cache_index(B + 8 * i, BBCACHE_SIZE)
            );
        }
        // Fill the arena up to the point where B's first block clears it, so
        // B's blocks land in the slots that A's blocks used.
        let filler = BB_ARENA_SIZE - BB_MAX_INSNS + 1 - h.bb_arena.len();
        h.bb_arena
            .extend(std::iter::repeat_n(BlockInsn::new(Insn::ILLEGAL), filler));
        assert_eq!(run_blocks(&mut h, &bus, B), 400);
        assert!(
            h.bb_arena.len() < BB_ARENA_SIZE / 2,
            "the arena was cleared"
        );
        assert_eq!(run_blocks(&mut h, &bus, A), 4);
    }
}
