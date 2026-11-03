//! Sv39/Sv48/Sv57 and Svnapot address translation with a small software TLB.

use rivemu_core::AccessSize;
use rivemu_mem::AddressSpace;

use crate::csr::{PMP_ENTRIES, envcfg, mseccfg, status};
use crate::hart::{Exception, Hart, Priv, cause};
use crate::log::LogFlags;

/// Kind of memory access being translated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Fetch,
    Load,
    /// Stores and AMOs.
    Store,
    /// `Zicbom` cache-block management operations (`cbo.inval`, `cbo.clean`, `cbo.flush`).
    CacheBom,
    /// `Zicfiss` shadow-stack pop (`sspopchk`, `c.sspopchk`).
    ShadowStackLoad,
    /// `Zicfiss` shadow-stack push/swap (`sspush`, `c.sspush`, `ssamoswap`).
    ShadowStackStore,
}

impl Access {
    fn page_fault(self, va: u64) -> Exception {
        let c = match self {
            Access::Fetch => cause::INSN_PAGE_FAULT,
            Access::Load => cause::LOAD_PAGE_FAULT,
            Access::Store
            | Access::CacheBom
            | Access::ShadowStackLoad
            | Access::ShadowStackStore => cause::STORE_PAGE_FAULT,
        };
        Exception::new(c, va)
    }

    /// Guest-page fault (H) for this access type at guest physical `gpa`,
    /// reported with guest virtual address `gva`.
    fn guest_page_fault(self, gva: u64, gpa: u64) -> Exception {
        let c = match self {
            Access::Fetch => cause::INSN_GUEST_PAGE_FAULT,
            Access::Load | Access::ShadowStackLoad => cause::LOAD_GUEST_PAGE_FAULT,
            Access::Store | Access::CacheBom | Access::ShadowStackStore => {
                cause::STORE_GUEST_PAGE_FAULT
            }
        };
        Exception {
            cause: c,
            tval: gva,
            tval2: gpa >> 2,
            gva: true,
        }
    }

    fn access_fault(self, va: u64) -> Exception {
        let c = match self {
            Access::Fetch => cause::INSN_ACCESS,
            Access::Load => cause::LOAD_ACCESS,
            Access::Store
            | Access::CacheBom
            | Access::ShadowStackLoad
            | Access::ShadowStackStore => cause::STORE_ACCESS,
        };
        Exception::new(c, va)
    }
}

mod pte {
    pub(super) const V: u64 = 1 << 0;
    pub(super) const R: u64 = 1 << 1;
    pub(super) const W: u64 = 1 << 2;
    pub(super) const X: u64 = 1 << 3;
    pub(super) const U: u64 = 1 << 4;
    pub(super) const G: u64 = 1 << 5;
    pub(super) const A: u64 = 1 << 6;
    pub(super) const D: u64 = 1 << 7;
    pub(super) const PPN_MASK: u64 = (1 << 44) - 1;
    pub(super) const RSW60T59B_MASK: u64 = 3 << 59;
    pub(super) const PBMT_SHIFT: u32 = 61;
    pub(super) const PBMT_MASK: u64 = 3 << PBMT_SHIFT;
    pub(super) const N: u64 = 1 << 63;
    pub(super) const RESERVED_HIGH_MASK: u64 =
        !((1u64 << 54) - 1) & !(RSW60T59B_MASK | PBMT_MASK | N);
}

const TLB_SIZE: usize = 1024;

#[derive(Clone, Copy, Debug)]
struct TlbEntry {
    /// `(va >> 12) + 1` (0 means invalid).
    tag: u64,
    /// Physical address of the 4 KiB page.
    pa_page: u64,
    /// Pre-validated byte offset of `pa_page` within `AddressSpace::fast_ram`
    /// when `PBMT == 0` (`u64::MAX` otherwise).
    ram_page_off: u64,
    /// Flush epoch matching [`Tlb::epoch`].
    epoch: u32,
    /// Address-space identifier (`satp.ASID`) when the entry was installed.
    asid: u16,
    /// Leaf PTE flags (low 8 bits, with non-leaf `G` propagated into bit 5).
    flags: u8,
    /// VPN shift for range invalidation (`0` = 4 KiB, `4` = 64 KiB `Svnapot`, `9 * level` = superpages).
    vpn_shift: u8,
}

impl Default for TlbEntry {
    fn default() -> Self {
        Self {
            tag: 0,
            pa_page: 0,
            ram_page_off: u64::MAX,
            epoch: 0,
            asid: 0,
            flags: 0,
            vpn_shift: 0,
        }
    }
}

/// Number of entries in [`PmpCache`] (direct-mapped by physical page).
const PMP_CACHE_SIZE: usize = 256;

/// One page's cached PMP verdicts.
#[derive(Clone, Copy, Default)]
struct PmpCacheEntry {
    /// `(pa >> 12) + 1` (0 means invalid).
    tag: u64,
    /// [`PmpCache::epoch`] when the entry was filled.
    epoch: u32,
    /// Bit `access + 8 * is_machine` is set once that check has run.
    checked: u16,
    /// The same bit is set when that check allowed the access.
    allowed: u16,
}

/// PMP verdicts per 4 KiB physical page.
///
/// While every active PMP entry starts and ends on a page boundary
/// (`!pmp_subpage`), the same entries match every byte of a page, so one
/// [`Hart::pmp_check`] per page, access type and privilege class (M or S/U)
/// decides all later accesses. [`Hart::update_pmp_state`] invalidates the
/// cache whenever the PMP configuration changes.
pub(crate) struct PmpCache {
    entries: Box<[PmpCacheEntry; PMP_CACHE_SIZE]>,
    epoch: u32,
}

impl Default for PmpCache {
    fn default() -> Self {
        Self {
            entries: Box::new([PmpCacheEntry::default(); PMP_CACHE_SIZE]),
            epoch: 1,
        }
    }
}

impl PmpCache {
    fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.entries.fill(PmpCacheEntry::default());
            self.epoch = 1;
        }
    }
}

/// Entries per access type in a [`FastTlb`] slot (direct-mapped by virtual
/// page).
const FAST_TLB_SIZE: usize = 256;
/// Translation contexts the [`FastTlb`] keeps at once.
pub(crate) const FAST_TLB_SLOTS: usize = 4;
/// Bits above the virtual page number (`va >> 12` has 52) that hold a slot's
/// epoch in the tags.
const FAST_EPOCH_SHIFT: u32 = 52;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FastTlbEntry {
    /// `(va >> 12) | epoch << 52` with the slot's epoch when the entry was
    /// filled (epochs start at 1, so 0 is invalid). One compare checks both.
    pub(crate) tag: u64,
    /// Physical address of the page.
    pub(crate) pa_page: u64,
    /// Byte offset of the page within `AddressSpace::fast_ram`.
    pub(crate) ram_page_off: u64,
}

/// The fast-TLB tables of one translation context.
#[derive(Clone, Debug)]
pub(crate) struct FastTlbSlot {
    /// [`Hart::fast_tlb_ctx`] of this slot (`u64::MAX`: unused; a context
    /// value never has every bit set).
    ctx: u64,
    /// The slot's epoch, shifted into tag position.
    pub(crate) epoch_hi: u64,
    pub(crate) load: Box<[FastTlbEntry; FAST_TLB_SIZE]>,
    pub(crate) store: Box<[FastTlbEntry; FAST_TLB_SIZE]>,
}

impl Default for FastTlbSlot {
    fn default() -> Self {
        Self {
            ctx: u64::MAX,
            epoch_hi: 1 << FAST_EPOCH_SHIFT,
            load: Box::new([FastTlbEntry::default(); FAST_TLB_SIZE]),
            store: Box::new([FastTlbEntry::default(); FAST_TLB_SIZE]),
        }
    }
}

impl FastTlbSlot {
    /// Invalidates every entry: a new epoch (clearing the tables when the
    /// 12-bit epoch wraps).
    fn invalidate(&mut self) {
        let next = (self.epoch_hi >> FAST_EPOCH_SHIFT) + 1;
        if next >= 1 << (64 - FAST_EPOCH_SHIFT) {
            self.load.fill(FastTlbEntry::default());
            self.store.fill(FastTlbEntry::default());
            self.epoch_hi = 1 << FAST_EPOCH_SHIFT;
        } else {
            self.epoch_hi = next << FAST_EPOCH_SHIFT;
        }
    }

    /// The entry for `va` in `table` if it is valid for this slot's epoch.
    #[inline(always)]
    pub(crate) fn hit(
        table: &[FastTlbEntry; FAST_TLB_SIZE],
        epoch_hi: u64,
        va: u64,
    ) -> Option<&FastTlbEntry> {
        let e = &table[((va >> 12) as usize) & (FAST_TLB_SIZE - 1)];
        (e.tag == (va >> 12) | epoch_hi).then_some(e)
    }
}

/// Fast path for scalar loads and stores to RAM: per translation context,
/// one direct-mapped table per access type, from virtual page to RAM offset.
///
/// An entry is filled only after the full translation (privilege, PTE
/// permissions, A/D, Svukte, PMP) allowed that access type on that page, the
/// page is plain RAM (`PBMT == 0`), and no PMP boundary lies inside a page; a
/// store entry only after a store to the page set its dirty bit. A slot
/// holds the entries of one translation context ([`Hart::fast_tlb_ctx`]:
/// privilege, `MPRV`, `MPP`, `MPV`, `SUM`, `MXR`, `senvcfg.UKTE`), so a
/// switch between, say, kernel and user mode keeps both. Everything else
/// the full path depends on invalidates every slot:
/// - `sfence.vma`, any TLB flush, and remote fences;
/// - writes to `satp` (the table is not ASID-tagged) and `menvcfg`;
/// - PMP changes.
#[derive(Clone, Debug, Default)]
pub(crate) struct FastTlb {
    pub(crate) slots: [FastTlbSlot; FAST_TLB_SLOTS],
    /// The slot to reassign next.
    victim: usize,
}

impl FastTlb {
    #[inline]
    pub(crate) fn invalidate(&mut self) {
        for s in &mut self.slots {
            s.invalidate();
        }
    }

    /// The slot holding `ctx`, if any.
    #[inline]
    pub(crate) fn find(&self, ctx: u64) -> Option<usize> {
        self.slots.iter().position(|s| s.ctx == ctx)
    }

    /// The slot for `ctx`, reassigning (and invalidating) one if none holds
    /// it.
    #[inline]
    pub(crate) fn slot_for(&mut self, ctx: u64) -> usize {
        if let Some(i) = self.find(ctx) {
            return i;
        }
        let i = self.victim;
        self.victim = (i + 1) % FAST_TLB_SLOTS;
        let s = &mut self.slots[i];
        s.ctx = ctx;
        s.invalidate();
        i
    }
}

/// Direct-mapped TLB caching leaf translations. Permissions are re-checked
/// on every hit, so privilege/MPRV/SUM/MXR changes need no flush.
#[derive(Clone, Debug)]
pub(crate) struct Tlb {
    entries: Box<[TlbEntry; TLB_SIZE]>,
    epoch: u32,
    has_superpages: bool,
    /// Load/store fast path; invalidated with every flush and `sfence.vma`.
    pub(crate) fast: FastTlb,
}

impl Default for Tlb {
    fn default() -> Self {
        Self {
            entries: Box::new([TlbEntry::default(); TLB_SIZE]),
            epoch: 1,
            has_superpages: false,
            fast: FastTlb::default(),
        }
    }
}

impl Tlb {
    #[inline]
    pub(crate) fn flush(&mut self) {
        self.fast.invalidate();
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.entries.fill(TlbEntry::default());
            self.epoch = 1;
        }
        self.has_superpages = false;
    }

    /// Selective TLB invalidation for `sfence.vma` / `sinval.vma` per RISC-V
    /// Privileged Spec Section 12.2.1.
    pub(crate) fn sfence_vma(&mut self, rs1_idx: u8, rs2_idx: u8, va: u64, asid: u64) {
        self.fast.invalidate();
        let target_asid = (asid & 0xffff) as u16;
        let vpn = va >> 12;
        match (rs1_idx != 0, rs2_idx != 0) {
            (false, false) => self.flush(),
            (true, false) => {
                if !self.has_superpages {
                    let e = &mut self.entries[(vpn as usize) & (TLB_SIZE - 1)];
                    if e.epoch == self.epoch && e.tag == vpn.wrapping_add(1) {
                        e.tag = 0;
                    }
                } else {
                    for e in self.entries.iter_mut() {
                        if e.epoch == self.epoch
                            && e.tag != 0
                            && ((e.tag - 1) ^ vpn) >> u32::from(e.vpn_shift) == 0
                        {
                            e.tag = 0;
                        }
                    }
                }
            }
            (false, true) => {
                for e in self.entries.iter_mut() {
                    if e.epoch == self.epoch
                        && e.tag != 0
                        && e.flags & (pte::G as u8) == 0
                        && e.asid == target_asid
                    {
                        e.tag = 0;
                    }
                }
            }
            (true, true) => {
                if !self.has_superpages {
                    let e = &mut self.entries[(vpn as usize) & (TLB_SIZE - 1)];
                    if e.epoch == self.epoch
                        && e.tag == vpn.wrapping_add(1)
                        && e.flags & (pte::G as u8) == 0
                        && e.asid == target_asid
                    {
                        e.tag = 0;
                    }
                } else {
                    for e in self.entries.iter_mut() {
                        if e.epoch == self.epoch
                            && e.tag != 0
                            && e.flags & (pte::G as u8) == 0
                            && e.asid == target_asid
                            && ((e.tag - 1) ^ vpn) >> u32::from(e.vpn_shift) == 0
                        {
                            e.tag = 0;
                        }
                    }
                }
            }
        }
    }
}

/// Leaf PTE permission check with explicit `SUM`, `MXR` and shadow-stack
/// enable (the H extension's VS-stage uses `vsstatus` and `henvcfg`).
#[inline]
fn pte_permits_with(flags: u64, prv: Priv, acc: Access, sum: bool, mxr: bool, sse: bool) -> bool {
    let is_ss_pte = (flags & (pte::R | pte::W | pte::X)) == pte::W && sse;
    let ok = match acc {
        Access::Fetch => !is_ss_pte && (flags & pte::X != 0),
        Access::Load => is_ss_pte || (flags & pte::R != 0) || (mxr && flags & pte::X != 0),
        Access::Store => !is_ss_pte && (flags & pte::W != 0),
        Access::CacheBom => {
            !is_ss_pte && ((flags & (pte::R | pte::W) != 0) || (mxr && flags & pte::X != 0))
        }
        Access::ShadowStackLoad | Access::ShadowStackStore => is_ss_pte,
    };
    ok && match prv {
        Priv::User => flags & pte::U != 0,
        Priv::Supervisor => {
            flags & pte::U == 0
                || (!matches!(
                    acc,
                    Access::Fetch | Access::ShadowStackLoad | Access::ShadowStackStore
                ) && sum)
        }
        Priv::Machine => true,
    }
}

/// A page walk without side effects (no A/D updates, no permission or PMP
/// checks) for debuggers and co-simulation. `atp` is `satp`, `vsatp` or
/// `hgatp` (MODE 8/9/10 and the root PPN); `root_bits` is 9, or 11 for a
/// G-stage root; `pte_pa` maps each page-table address (the G-stage for
/// VS-stage tables). With an 11-bit root the address must fit in
/// `12 + 9 * levels + 2` bits, otherwise it must be canonical.
fn debug_walk(
    bus: &AddressSpace,
    atp: u64,
    va: u64,
    root_bits: u32,
    pbmte: bool,
    pte_pa: &dyn Fn(u64) -> Option<u64>,
) -> Option<u64> {
    let levels: u32 = match atp >> 60 {
        8 => 3,
        9 => 4,
        10 => 5,
        _ => return None,
    };
    let va_bits = 12 + 9 * levels;
    if root_bits == 11 {
        if va >> (va_bits + 2) != 0 {
            return None;
        }
    } else {
        let shift = 64 - va_bits;
        if ((va << shift) as i64 >> shift) as u64 != va {
            return None;
        }
    }
    let mut table = (atp & pte::PPN_MASK) << 12;
    for level in (0..levels).rev() {
        let idx_mask = if level == levels - 1 {
            (1u64 << root_bits) - 1
        } else {
            0x1ff
        };
        let idx = (va >> (12 + 9 * level)) & idx_mask;
        let pte = bus
            .read(pte_pa(table + idx * 8)?, AccessSize::Double)
            .ok()?;
        if pte & pte::V == 0
            || (pte & pte::R == 0 && pte & pte::W != 0)
            || pte & pte::RESERVED_HIGH_MASK != 0
        {
            return None;
        }
        let pbmt = (pte & pte::PBMT_MASK) >> pte::PBMT_SHIFT;
        let ppn = (pte >> 10) & pte::PPN_MASK;
        if pte & (pte::R | pte::X) == 0 {
            if level == 0 || pte & (pte::D | pte::A | pte::U | pte::N) != 0 || pbmt != 0 {
                return None;
            }
            table = ppn << 12;
            continue;
        }
        if pbmt != 0 && (!pbmte || pbmt == 3) {
            return None;
        }
        let pa_page = if pte & pte::N != 0 {
            if level != 0 || (ppn & 0xf) != 0x8 {
                return None;
            }
            ((ppn & !0xf) | ((va >> 12) & 0xf)) << 12
        } else {
            let low_mask = (1u64 << (9 * level)) - 1;
            if ppn & low_mask != 0 {
                return None;
            }
            (ppn | ((va >> 12) & low_mask)) << 12
        };
        return Some(pa_page | (va & 0xfff));
    }
    None
}

/// What a VS-stage translation (H extension) depends on: the guest's
/// privilege, `vsatp`, `vsstatus.SUM`/`MXR`, HS-level `MXR` (for the
/// G-stage), whether the access is an `hlvx` (execute permission read),
/// whether VS-stage shadow-stack pages exist (`henvcfg.SSE`) and whether
/// Svukte applies to a guest user access.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VsCtx {
    pub(crate) prv: Priv,
    pub(crate) vsatp: u64,
    pub(crate) sum: bool,
    pub(crate) mxr: bool,
    pub(crate) hs_mxr: bool,
    pub(crate) hlvx: bool,
    pub(crate) sse: bool,
    pub(crate) ukte: bool,
}

impl Hart {
    /// Effective privilege for data accesses (honours `mstatus.MPRV`).
    #[inline]
    fn effective_priv(&self, acc: Access) -> Priv {
        if acc != Access::Fetch && self.csr.mstatus & status::MPRV != 0 {
            match (self.csr.mstatus >> status::MPP_SHIFT) & 3 {
                0 => Priv::User,
                1 => Priv::Supervisor,
                _ => Priv::Machine,
            }
        } else {
            self.prv
        }
    }

    /// Applies `Ssnpm`/`Smnpm` pointer masking (`PMLEN = 7` or `16`) to a data
    /// virtual address when enabled for the effective data privilege level.
    #[inline]
    pub(crate) fn mask_data_addr(&self, va: u64) -> u64 {
        // Pointer masking is off unless some PMM field is set; all three sit
        // at bits [33:32]. Skips the effective-privilege lookup on every
        // access in the common case.
        if (self.csr.menvcfg | self.csr.senvcfg | self.csr.mseccfg | self.csr.h.henvcfg)
            & envcfg::PMM_MASK
            == 0
        {
            return va;
        }
        let prv = self.effective_priv(Access::Load);
        // M-mode with MPRV and MPV accesses as VS/VU-mode.
        let mprv_virt = self.prv == Priv::Machine
            && prv != Priv::Machine
            && self.csr.mstatus & (status::MPRV | status::MPV) == status::MPRV | status::MPV;
        let virt = self.virt || mprv_virt;
        let pmlen = self.csr.effective_pmlen(prv, virt);
        if pmlen == 0 {
            return va;
        }
        let satp = if mprv_virt {
            self.csr.h.bank.satp
        } else {
            self.csr.satp
        };
        if prv != Priv::Machine && (satp >> 60) != 0 {
            ((va << pmlen) as i64 >> pmlen) as u64
        } else {
            (va << pmlen) >> pmlen
        }
    }

    #[inline]
    fn pte_permits(&self, flags: u64, prv: Priv, acc: Access) -> bool {
        let s = self.csr.mstatus;
        let sse = self.csr.menvcfg & envcfg::SSE != 0;
        pte_permits_with(
            flags,
            prv,
            acc,
            s & status::SUM != 0,
            s & status::MXR != 0,
            sse,
        )
    }

    /// Recomputes predecoded PMP ranges and fast-path flags.
    pub fn update_pmp_state(&mut self) {
        let mut count = 0usize;
        let mut subpage = false;
        let mut any_locked = false;
        let mut any_active = false;
        for i in 0..PMP_ENTRIES {
            let cfg = self.csr.pmpcfg[i];
            if cfg & 0x18 != 0 {
                any_active = true;
            }
            if let Some((lo, hi)) = self.pmp_entry_range(i) {
                if (lo & 0xfff) != 0 || (hi & 0xfff) != 0xfff {
                    subpage = true;
                }
                if cfg & 0x80 != 0 {
                    any_locked = true;
                }
                self.pmp_ranges[count] = (lo, hi, cfg);
                count += 1;
            }
        }
        let mseccfg_active = (self.csr.mseccfg & (mseccfg::MML | mseccfg::MMWP)) != 0;
        self.pmp_count = count as u8;
        self.pmp_any_locked = any_locked || mseccfg_active;
        // Entry 0 as an unlocked NAPOT catch-all (`R|W|X`) covering >= 56 bits
        // grants unconditional access to all privileges unless Smepmp (`MML`/`MMWP`) is active.
        let full_open_entry0 = !mseccfg_active
            && (self.csr.pmpcfg[0] & 0x9f) == 0x1f
            && (!self.csr.pmpaddr[0]).trailing_zeros() >= 53;
        self.pmp_enforced = !full_open_entry0 && (any_active || self.strict_pmp || mseccfg_active);
        self.pmp_subpage = self.pmp_enforced && subpage;
        self.pmp_cache.invalidate();
        self.tlb.fast.invalidate();
    }

    /// Returns the inclusive physical address range `(lo, hi_inclusive)` matched
    /// by PMP entry `i`, or `None` if the entry is `OFF` or empty (`TOR` with `lo >= hi`).
    fn pmp_entry_range(&self, i: usize) -> Option<(u64, u64)> {
        let cfg = self.csr.pmpcfg[i];
        match (cfg >> 3) & 3 {
            0 => None,
            1 => {
                let lo = if i == 0 {
                    0
                } else {
                    self.csr.pmpaddr[i - 1] << 2
                };
                let hi = self.csr.pmpaddr[i] << 2;
                if lo >= hi { None } else { Some((lo, hi - 1)) }
            }
            2 => {
                let lo = self.csr.pmpaddr[i] << 2;
                Some((lo, lo.saturating_add(3)))
            }
            _ => {
                let addr = self.csr.pmpaddr[i];
                let ones = (!addr).trailing_zeros();
                if ones >= 61 {
                    Some((0, u64::MAX))
                } else {
                    let mask = (1u64 << (ones + 3)) - 1;
                    let lo = (addr << 2) & !mask;
                    Some((lo, lo | mask))
                }
            }
        }
    }

    /// Checks `[pa, pa + size - 1]` against the active PMP entries per RISC-V
    /// Privileged Spec Section 3.7 and `Smepmp`.
    #[cold]
    pub(crate) fn pmp_check(&self, pa: u64, size: u64, prv: Priv, acc: Access) -> bool {
        const MML_M_PERMS: [u8; 16] = [0, 0, 3, 3, 0, 0, 0, 0, 0, 4, 4, 5, 1, 5, 3, 1];
        const MML_SU_PERMS: [u8; 16] = [0, 4, 1, 3, 1, 5, 3, 7, 0, 0, 4, 4, 0, 0, 0, 1];

        let mml = (self.csr.mseccfg & mseccfg::MML) != 0;
        let mmwp = (self.csr.mseccfg & mseccfg::MMWP) != 0;
        if prv == Priv::Machine
            && !self.pmp_any_locked
            && !mml
            && !mmwp
            && (!self.pmp_subpage || size <= 1)
        {
            return true;
        }
        let last = pa.saturating_add(size.saturating_sub(1));
        for &(lo, hi, cfg) in &self.pmp_ranges[..self.pmp_count as usize] {
            if pa <= hi && last >= lo {
                // Accesses straddling a PMP boundary must fault.
                if pa < lo || last > hi {
                    return false;
                }
                let rwx = if mml {
                    let idx = (((cfg >> 4) & 8) | ((cfg & 1) << 2) | (cfg & 2) | ((cfg >> 2) & 1))
                        as usize;
                    if prv == Priv::Machine {
                        MML_M_PERMS[idx]
                    } else {
                        MML_SU_PERMS[idx]
                    }
                } else if prv == Priv::Machine && cfg & 0x80 == 0 {
                    0x07
                } else {
                    cfg & 0x07
                };
                return match acc {
                    Access::Fetch => rwx & 0x04 != 0,
                    Access::Load => rwx & 0x01 != 0,
                    Access::Store => rwx & 0x02 != 0,
                    Access::CacheBom => rwx & 0x03 != 0,
                    Access::ShadowStackLoad | Access::ShadowStackStore => (rwx & 0x03) == 0x03,
                };
            }
        }
        if prv == Priv::Machine {
            if mmwp {
                false
            } else if mml {
                acc != Access::Fetch
            } else {
                true
            }
        } else {
            !self.strict_pmp && self.pmp_count == 0 && !mmwp && !mml
        }
    }

    /// Everything outside the TLB contents that decides whether a cached
    /// translation still permits an access: privilege, `MPRV`/`MPP`,
    /// `SUM`, `MXR` and `senvcfg.UKTE` (bit 8, moved to bit 0, which is
    /// reserved in `mstatus`).
    #[inline]
    pub(crate) fn fast_tlb_ctx(&self) -> u64 {
        (self.csr.mstatus & (status::MPRV | status::MPP | status::MPV | status::SUM | status::MXR))
            | ((self.csr.senvcfg & envcfg::UKTE) >> 8)
            | ((self.prv as u64) << 62)
    }

    /// RAM offset for a load from `va` (within one page) on a fast-TLB hit.
    #[inline]
    pub(crate) fn fast_load_ram_off(&self, va: u64) -> Option<u64> {
        let f = &self.tlb.fast;
        let s = &f.slots[f.find(self.fast_tlb_ctx())?];
        FastTlbSlot::hit(&s.load, s.epoch_hi, va).map(|e| e.ram_page_off | (va & 0xfff))
    }

    /// `(pa, ram_off)` for a store to `va` (within one page) on a fast-TLB hit.
    #[inline]
    pub(crate) fn fast_store_ram_off(&self, va: u64) -> Option<(u64, u64)> {
        let f = &self.tlb.fast;
        let s = &f.slots[f.find(self.fast_tlb_ctx())?];
        FastTlbSlot::hit(&s.store, s.epoch_hi, va)
            .map(|e| (e.pa_page | (va & 0xfff), e.ram_page_off | (va & 0xfff)))
    }

    /// Records a translation that the full path allowed. `ram_off` is the
    /// value [`Self::translate_with_ram_off`] returned (`u64::MAX` for
    /// anything but plain RAM). A store entry is recorded after the store
    /// (which set the page's dirty bit).
    #[inline]
    pub(crate) fn fast_tlb_fill(&mut self, va: u64, pa: u64, ram_off: u64, store: bool) {
        if ram_off == u64::MAX || self.pmp_subpage {
            return;
        }
        let ctx = self.fast_tlb_ctx();
        let i = self.tlb.fast.slot_for(ctx);
        let slot = &mut self.tlb.fast.slots[i];
        let e = FastTlbEntry {
            tag: (va >> 12) | slot.epoch_hi,
            pa_page: pa & !0xfff,
            ram_page_off: ram_off & !0xfff,
        };
        let idx = ((va >> 12) as usize) & (FAST_TLB_SIZE - 1);
        if store {
            slot.store[idx] = e;
        } else {
            slot.load[idx] = e;
        }
    }

    /// [`Self::pmp_check`] for a 1-byte access, cached per physical page
    /// (see [`PmpCache`]) unless a PMP entry boundary lies inside a page.
    #[inline]
    fn pmp_allows(&mut self, pa: u64, prv: Priv, acc: Access) -> bool {
        if self.pmp_subpage {
            return self.pmp_check(pa, 1, prv, acc);
        }
        let bit = 1u16 << (acc as u16 + if prv == Priv::Machine { 8 } else { 0 });
        let page = pa >> 12;
        let idx = (page as usize) & (PMP_CACHE_SIZE - 1);
        let mut e = self.pmp_cache.entries[idx];
        if e.tag != page + 1 || e.epoch != self.pmp_cache.epoch {
            e = PmpCacheEntry {
                tag: page + 1,
                epoch: self.pmp_cache.epoch,
                checked: 0,
                allowed: 0,
            };
        }
        if e.checked & bit == 0 {
            e.checked |= bit;
            if self.pmp_check(pa, 1, prv, acc) {
                e.allowed |= bit;
            }
            self.pmp_cache.entries[idx] = e;
        }
        e.allowed & bit != 0
    }

    #[inline]
    pub(crate) fn check_pmp_access(
        &self,
        va: u64,
        pa: u64,
        size: u64,
        acc: Access,
    ) -> Result<(), Exception> {
        if self.pmp_subpage && !self.pmp_check(pa, size, self.effective_priv(acc), acc) {
            return Err(acc.access_fault(va));
        }
        Ok(())
    }

    /// Translates a virtual address and returns `(pa, fast_ram_offset)` where
    /// `fast_ram_offset != u64::MAX` when the translated 4 KiB page has
    /// `PBMT == 0`, lies inside `AddressSpace::fast_ram`, and no PMP entry
    /// boundary falls inside a page (`!pmp_subpage`).
    #[inline]
    pub(crate) fn translate_with_ram_off(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        acc: Access,
    ) -> Result<(u64, u64), Exception> {
        let prv = self.effective_priv(acc);
        if self.h_ext {
            if self.virt {
                let ctx = self.vs_ctx_active(prv);
                return self.translate_two_stage(bus, va, acc, ctx, true);
            }
            let s = self.csr.mstatus;
            if acc != Access::Fetch
                && self.prv == Priv::Machine
                && s & status::MPRV != 0
                && s & status::MPV != 0
                && prv != Priv::Machine
            {
                let ctx = self.vs_ctx_bank(prv, false);
                return self.translate_two_stage(bus, va, acc, ctx, false);
            }
        }
        let mode = self.csr.satp >> 60;
        if prv == Priv::Machine || mode == 0 {
            if matches!(acc, Access::ShadowStackLoad | Access::ShadowStackStore) {
                return Err(acc.access_fault(va));
            }
            if self.pmp_enforced
                && (prv != Priv::Machine || self.pmp_any_locked)
                && !self.pmp_allows(va, prv, acc)
            {
                return Err(acc.access_fault(va));
            }
            if self.pmp_subpage {
                return Ok((va, u64::MAX));
            }
            let ram_off = bus
                .fast_ram_page_offset(va & !0xfff)
                .map_or(u64::MAX, |o| o | (va & 0xfff));
            return Ok((va, ram_off));
        }
        if prv == Priv::User && (self.csr.senvcfg & envcfg::UKTE) != 0 && (va >> 63) != 0 {
            return Err(acc.page_fault(va));
        }
        let asid = ((self.csr.satp >> 44) & 0xffff) as u16;
        let tag = (va >> 12) + 1;
        let e = self.tlb.entries[((va >> 12) as usize) & (TLB_SIZE - 1)];
        let flags = u64::from(e.flags);
        if e.tag == tag
            && e.epoch == self.tlb.epoch
            && (flags & pte::G != 0 || e.asid == asid)
            && self.pte_permits(flags, prv, acc)
            && (!matches!(acc, Access::Store | Access::ShadowStackStore) || flags & pte::D != 0)
        {
            let off = va & 0xfff;
            let pa = e.pa_page | off;
            if self.pmp_enforced {
                if !self.pmp_allows(pa, prv, acc) {
                    return Err(acc.access_fault(va));
                }
                // Multi-byte accesses within a page need their own check only
                // when a PMP boundary can fall inside the page.
                if self.pmp_subpage {
                    return Ok((pa, u64::MAX));
                }
            }
            let ram_off = if e.ram_page_off != u64::MAX {
                e.ram_page_off | off
            } else {
                u64::MAX
            };
            return Ok((pa, ram_off));
        }
        let pa = self.walk(bus, va, acc, prv, mode)?;
        if self.pmp_enforced && !self.pmp_allows(pa, prv, acc) {
            return Err(acc.access_fault(va));
        }
        Ok((pa, u64::MAX))
    }

    /// Translates a virtual address.
    #[inline]
    pub(crate) fn translate(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        acc: Access,
    ) -> Result<u64, Exception> {
        self.translate_with_ram_off(bus, va, acc).map(|(pa, _)| pa)
    }

    /// Side-effect-free virtual-to-physical translation for debuggers (GDB).
    pub fn debug_translate(&self, bus: &AddressSpace, va: u64) -> Option<u64> {
        if self.virt {
            return self.debug_translate_two_stage(bus, va);
        }
        let mode = self.csr.satp >> 60;
        if self.prv == Priv::Machine || mode == 0 {
            return Some(va);
        }
        if self.prv == Priv::User && (self.csr.senvcfg & envcfg::UKTE) != 0 && (va >> 63) != 0 {
            return None;
        }
        let pbmte = self.csr.menvcfg & envcfg::PBMTE != 0;
        debug_walk(bus, self.csr.satp, va, 9, pbmte, &Some)
    }

    /// [`Self::debug_translate`] with V = 1: the VS-stage walk, whose
    /// page-table reads are themselves G-stage translated, then the G-stage.
    fn debug_translate_two_stage(&self, bus: &AddressSpace, va: u64) -> Option<u64> {
        let vsatp = self.csr.satp;
        let gpa = if vsatp >> 60 == 0 {
            va
        } else {
            if self.prv == Priv::User && (self.csr.senvcfg & envcfg::UKTE) != 0 && (va >> 63) != 0 {
                return None;
            }
            let pbmte = self.csr.henvcfg_eff() & envcfg::PBMTE != 0;
            debug_walk(bus, vsatp, va, 9, pbmte, &|gpa| {
                self.debug_g_stage(bus, gpa)
            })?
        };
        self.debug_g_stage(bus, gpa)
    }

    /// The G-stage (`hgatp`) of [`Self::debug_translate`].
    fn debug_g_stage(&self, bus: &AddressSpace, gpa: u64) -> Option<u64> {
        let hgatp = self.csr.h.hgatp;
        if hgatp >> 60 == 0 {
            return Some(gpa);
        }
        let pbmte = self.csr.menvcfg & envcfg::PBMTE != 0;
        // The x4 schemes: two more address bits, a 2048-entry root.
        debug_walk(bus, hgatp, gpa, 11, pbmte, &Some)
    }

    #[cold]
    fn walk(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        acc: Access,
        prv: Priv,
        mode: u64,
    ) -> Result<u64, Exception> {
        let res = self.walk_inner(bus, va, acc, prv, mode);
        if self.log_flags.has(LogFlags::MMU) {
            match &res {
                Ok(pa) => self.log_mmu(format_args!("walk va={va:#x} ({acc:?}) -> pa={pa:#x}")),
                Err(e) => self.log_mmu(format_args!(
                    "walk fault va={va:#x} ({acc:?}) cause={:#x}",
                    e.cause
                )),
            }
        }
        res
    }

    fn walk_inner(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        acc: Access,
        prv: Priv,
        mode: u64,
    ) -> Result<u64, Exception> {
        let levels: u32 = match mode {
            8 => 3,
            9 => 4,
            _ => 5,
        };
        if prv == Priv::User && (self.csr.senvcfg & envcfg::UKTE) != 0 && (va >> 63) != 0 {
            return Err(acc.page_fault(va));
        }
        let va_bits = 12 + 9 * levels;
        let shift = 64 - va_bits;
        if ((va << shift) as i64 >> shift) as u64 != va {
            return Err(acc.page_fault(va));
        }
        let mut table = (self.csr.satp & pte::PPN_MASK) << 12;
        let mut global = 0u64;
        for level in (0..levels).rev() {
            let idx = (va >> (12 + 9 * level)) & 0x1ff;
            let pte_addr = table + idx * 8;
            if self.pmp_enforced && !self.pmp_check(pte_addr, 8, Priv::Supervisor, Access::Load) {
                return Err(acc.access_fault(va));
            }
            let pte = bus
                .read(pte_addr, AccessSize::Double)
                .map_err(|_| acc.access_fault(va))?;
            let is_ss_pte = (pte & (pte::R | pte::W | pte::X)) == pte::W
                && (self.csr.menvcfg & envcfg::SSE != 0);
            if pte & pte::V == 0
                || (pte & pte::R == 0 && pte & pte::W != 0 && !is_ss_pte)
                || pte & pte::RESERVED_HIGH_MASK != 0
            {
                return Err(acc.page_fault(va));
            }
            global |= pte & pte::G;
            let pbmt = (pte & pte::PBMT_MASK) >> pte::PBMT_SHIFT;
            let ppn = (pte >> 10) & pte::PPN_MASK;
            if pte & (pte::R | pte::W | pte::X) == 0 {
                // Pointer to the next level.
                if level == 0 || pte & (pte::D | pte::A | pte::U | pte::N) != 0 || pbmt != 0 {
                    return Err(acc.page_fault(va));
                }
                table = ppn << 12;
                continue;
            }
            // Leaf: validate Svpbmt attributes (PBMTE must be enabled for PBMT != 0, and 3 is reserved).
            if pbmt != 0 && (self.csr.menvcfg & envcfg::PBMTE == 0 || pbmt == 3) {
                return Err(acc.page_fault(va));
            }
            if !self.pte_permits(pte, prv, acc) {
                let u_ok = match prv {
                    Priv::User => pte & pte::U != 0,
                    Priv::Supervisor => {
                        pte & pte::U == 0
                            || (!matches!(
                                acc,
                                Access::Fetch | Access::ShadowStackLoad | Access::ShadowStackStore
                            ) && self.csr.mstatus & status::SUM != 0)
                    }
                    Priv::Machine => true,
                };
                if (is_ss_pte
                    || (matches!(acc, Access::ShadowStackLoad | Access::ShadowStackStore)
                        && (pte & pte::W != 0)))
                    && u_ok
                {
                    return Err(acc.access_fault(va));
                }
                return Err(acc.page_fault(va));
            }
            let (pa_page, vpn_shift) = if pte & pte::N != 0 {
                // Svnapot: only 64 KiB (`level == 0`, `ppn[3:0] == 0b1000`) is defined.
                if level != 0 || (ppn & 0xf) != 0x8 {
                    return Err(acc.page_fault(va));
                }
                (((ppn & !0xf) | ((va >> 12) & 0xf)) << 12, 4u8)
            } else {
                let low_mask = (1u64 << (9 * level)) - 1;
                if ppn & low_mask != 0 {
                    return Err(acc.page_fault(va)); // misaligned superpage
                }
                ((ppn | ((va >> 12) & low_mask)) << 12, (9 * level) as u8)
            };
            // Svade vs Svadu: with `menvcfg.ADUE == 0`, a missing A bit (or D on
            // a store / shadow-stack store) raises a page fault instead of
            // updating the PTE ("the implementation behaves as though Svadu
            // were not implemented"; Sail `hw_updates_pte`).
            let needs_dirty = matches!(acc, Access::Store | Access::ShadowStackStore);
            if self.csr.menvcfg & envcfg::ADUE == 0
                && (pte & pte::A == 0 || (needs_dirty && pte & pte::D == 0))
            {
                return Err(acc.page_fault(va));
            }
            // Hardware A/D update.
            let mut new = pte | pte::A;
            if needs_dirty {
                new |= pte::D;
            }
            if new != pte {
                if self.pmp_enforced
                    && !self.pmp_check(pte_addr, 8, Priv::Supervisor, Access::Store)
                {
                    return Err(acc.access_fault(va));
                }
                bus.write(pte_addr, AccessSize::Double, new)
                    .map_err(|_| acc.access_fault(va))?;
            }
            let asid = ((self.csr.satp >> 44) & 0xffff) as u16;
            let ram_page_off = if pbmt == 0 {
                bus.fast_ram_page_offset(pa_page).unwrap_or(u64::MAX)
            } else {
                u64::MAX
            };
            if vpn_shift > 0 {
                self.tlb.has_superpages = true;
            }
            self.tlb.entries[((va >> 12) as usize) & (TLB_SIZE - 1)] = TlbEntry {
                tag: (va >> 12) + 1,
                pa_page,
                ram_page_off,
                epoch: self.tlb.epoch,
                asid,
                flags: (new | global) as u8,
                vpn_shift,
            };
            return Ok(pa_page | (va & 0xfff));
        }
        unreachable!("loop always returns at level 0")
    }

    // -------------------------------------------------------------------
    // H extension: two-stage (VS-stage + G-stage) translation
    // -------------------------------------------------------------------

    /// VS-stage context of the running guest (V = 1; the active S-level
    /// CSRs are the VS copies).
    pub(crate) fn vs_ctx_active(&self, prv: Priv) -> VsCtx {
        let s = self.csr.mstatus;
        VsCtx {
            prv,
            vsatp: self.csr.satp,
            sum: s & status::SUM != 0,
            mxr: s & status::MXR != 0,
            hs_mxr: self.csr.h.bank.status & status::MXR != 0,
            hlvx: false,
            sse: self.csr.henvcfg_eff() & envcfg::SSE != 0,
            ukte: self.csr.senvcfg & envcfg::UKTE != 0,
        }
    }

    /// VS-stage context for an access made with V = 0 on behalf of the
    /// guest (`mstatus.MPRV` with `MPV`, `hlv`/`hsv`): the VS copies are in
    /// the bank.
    pub(crate) fn vs_ctx_bank(&self, prv: Priv, hlvx: bool) -> VsCtx {
        let b = &self.csr.h.bank;
        // Svukte: HLV/HSV executed in U-mode use hstatus.HUKTE, everything
        // else senvcfg.UKTE (as QEMU).
        let ukte = if self.prv == Priv::User {
            self.csr.h.hstatus & crate::csr::hstatus::HUKTE != 0
        } else {
            self.csr.senvcfg & envcfg::UKTE != 0
        };
        VsCtx {
            prv,
            vsatp: b.satp,
            sum: b.status & status::SUM != 0,
            mxr: b.status & status::MXR != 0,
            hs_mxr: self.csr.mstatus & status::MXR != 0,
            hlvx,
            sse: self.csr.henvcfg_eff() & envcfg::SSE != 0,
            ukte,
        }
    }

    /// Translates guest virtual `gva` through the VS-stage (`vsatp`) and the
    /// G-stage (`hgatp`), then checks PMP. With `use_tlb` (V = 1), hits and
    /// fills use the TLB, which [`Hart::set_virt`] flushes on every switch.
    pub(crate) fn translate_two_stage(
        &mut self,
        bus: &AddressSpace,
        gva: u64,
        acc: Access,
        ctx: VsCtx,
        use_tlb: bool,
    ) -> Result<(u64, u64), Exception> {
        // Access faults in two-stage translation report a guest virtual
        // address (QEMU sets GVA for them).
        let access_fault = |acc: Access| Exception {
            gva: true,
            ..acc.access_fault(gva)
        };
        let vs_mode = ctx.vsatp >> 60;
        // Svukte: guest user accesses to the upper half fault.
        if ctx.prv == Priv::User && ctx.ukte && vs_mode != 0 && gva >> 63 != 0 {
            return Err(Exception {
                gva: true,
                ..acc.page_fault(gva)
            });
        }
        if use_tlb && vs_mode != 0 && !ctx.hlvx {
            let asid = ((ctx.vsatp >> 44) & 0xffff) as u16;
            let e = self.tlb.entries[((gva >> 12) as usize) & (TLB_SIZE - 1)];
            let flags = u64::from(e.flags);
            if e.tag == (gva >> 12) + 1
                && e.epoch == self.tlb.epoch
                && (flags & pte::G != 0 || e.asid == asid)
                && pte_permits_with(flags, ctx.prv, acc, ctx.sum, ctx.mxr, ctx.sse)
                && (!matches!(acc, Access::Store | Access::ShadowStackStore) || flags & pte::D != 0)
            {
                let pa = e.pa_page | (gva & 0xfff);
                if self.pmp_enforced && !self.pmp_allows(pa, ctx.prv, acc) {
                    return Err(access_fault(acc));
                }
                if self.pmp_subpage || e.ram_page_off == u64::MAX {
                    return Ok((pa, u64::MAX));
                }
                return Ok((pa, e.ram_page_off | (gva & 0xfff)));
            }
        }
        let (gpa, vs_flags, vs_shift, vs_pbmt) = if vs_mode == 0 {
            (gva, pte::R | pte::W | pte::X | pte::A | pte::D, 0u8, 0)
        } else {
            self.vs_walk(bus, gva, acc, ctx)?
        };
        let (spa, g_flags, g_pbmt) = self.g_stage(bus, gpa, gva, acc, ctx, None)?;
        if self.pmp_enforced && !self.pmp_allows(spa, ctx.prv, acc) {
            return Err(access_fault(acc));
        }
        let ram_off = if vs_pbmt == 0 && g_pbmt == 0 && !self.pmp_subpage {
            bus.fast_ram_page_offset(spa & !0xfff)
                .map_or(u64::MAX, |o| o | (spa & 0xfff))
        } else {
            u64::MAX
        };
        if use_tlb && vs_mode != 0 && !ctx.hlvx {
            // Cache the combined permissions, never more permissive than
            // either stage: a stricter entry only costs a walk.
            let g_load = g_flags & pte::R != 0 || (ctx.hs_mxr && g_flags & pte::X != 0);
            let mut flags = vs_flags & !(pte::R | pte::W | pte::X | pte::D);
            if vs_flags & pte::R != 0 && g_load {
                flags |= pte::R;
            }
            if vs_flags & pte::W != 0 && g_flags & pte::W != 0 {
                flags |= pte::W;
            }
            if vs_flags & pte::X != 0 && g_flags & pte::X != 0 && g_load {
                flags |= pte::X;
            }
            if vs_flags & pte::D != 0 && g_flags & pte::D != 0 {
                flags |= pte::D;
            }
            if vs_shift > 0 {
                self.tlb.has_superpages = true;
            }
            self.tlb.entries[((gva >> 12) as usize) & (TLB_SIZE - 1)] = TlbEntry {
                tag: (gva >> 12) + 1,
                pa_page: spa & !0xfff,
                ram_page_off: if ram_off == u64::MAX {
                    u64::MAX
                } else {
                    ram_off & !0xfff
                },
                epoch: self.tlb.epoch,
                asid: ((ctx.vsatp >> 44) & 0xffff) as u16,
                flags: flags as u8,
                vpn_shift: vs_shift,
            };
        }
        Ok((spa, ram_off))
    }

    /// VS-stage walk of `gva`: returns the guest physical address, the leaf
    /// flags (with non-leaf `G`), the VPN shift and the PBMT value. Page
    /// table accesses are themselves G-stage translated.
    fn vs_walk(
        &mut self,
        bus: &AddressSpace,
        gva: u64,
        acc: Access,
        ctx: VsCtx,
    ) -> Result<(u64, u64, u8, u64), Exception> {
        let page_fault = Exception {
            gva: true,
            ..acc.page_fault(gva)
        };
        let henvcfg = self.csr.henvcfg_eff();
        let levels: u32 = match ctx.vsatp >> 60 {
            8 => 3,
            9 => 4,
            _ => 5,
        };
        let va_bits = 12 + 9 * levels;
        let shift = 64 - va_bits;
        if ((gva << shift) as i64 >> shift) as u64 != gva {
            return Err(page_fault);
        }
        let mut table = (ctx.vsatp & pte::PPN_MASK) << 12;
        let mut global = 0u64;
        for level in (0..levels).rev() {
            let idx = (gva >> (12 + 9 * level)) & 0x1ff;
            let pte_gpa = table + idx * 8;
            let (pte_spa, _, _) = self.g_stage(bus, pte_gpa, gva, acc, ctx, Some(Access::Load))?;
            if self.pmp_enforced && !self.pmp_check(pte_spa, 8, Priv::Supervisor, Access::Load) {
                return Err(Exception {
                    gva: true,
                    ..acc.access_fault(gva)
                });
            }
            let pte = bus
                .read(pte_spa, AccessSize::Double)
                .map_err(|_| Exception {
                    gva: true,
                    ..acc.access_fault(gva)
                })?;
            // Zicfiss: R=0 W=1 X=0 is a shadow-stack page with henvcfg.SSE.
            let is_ss_pte = (pte & (pte::R | pte::W | pte::X)) == pte::W && ctx.sse;
            if pte & pte::V == 0
                || (pte & pte::R == 0 && pte & pte::W != 0 && !is_ss_pte)
                || pte & pte::RESERVED_HIGH_MASK != 0
            {
                return Err(page_fault);
            }
            global |= pte & pte::G;
            let pbmt = (pte & pte::PBMT_MASK) >> pte::PBMT_SHIFT;
            let ppn = (pte >> 10) & pte::PPN_MASK;
            if pte & (pte::R | pte::W | pte::X) == 0 {
                if level == 0 || pte & (pte::D | pte::A | pte::U | pte::N) != 0 || pbmt != 0 {
                    return Err(page_fault);
                }
                table = ppn << 12;
                continue;
            }
            if pbmt != 0 && (henvcfg & envcfg::PBMTE == 0 || pbmt == 3) {
                return Err(page_fault);
            }
            let perm_acc = if ctx.hlvx { Access::Fetch } else { acc };
            // hlvx reads with execute permission; SUM still applies.
            let prv_ok = match ctx.prv {
                Priv::User => pte & pte::U != 0,
                Priv::Supervisor => {
                    pte & pte::U == 0
                        || (!matches!(
                            acc,
                            Access::Fetch | Access::ShadowStackLoad | Access::ShadowStackStore
                        ) && ctx.sum)
                }
                Priv::Machine => true,
            };
            if !pte_permits_with(pte, Priv::Machine, perm_acc, ctx.sum, ctx.mxr, ctx.sse) || !prv_ok
            {
                // Zicfiss: a shadow-stack access to a writable non-shadow-stack
                // page, or another access to a shadow-stack page, is an
                // access fault when the privilege allows the page.
                if prv_ok
                    && (is_ss_pte
                        || (matches!(acc, Access::ShadowStackLoad | Access::ShadowStackStore)
                            && pte & pte::W != 0))
                {
                    return Err(Exception {
                        gva: true,
                        ..acc.access_fault(gva)
                    });
                }
                return Err(page_fault);
            }
            let (gpa_page, vpn_shift) = if pte & pte::N != 0 {
                if level != 0 || (ppn & 0xf) != 0x8 {
                    return Err(page_fault);
                }
                (((ppn & !0xf) | ((gva >> 12) & 0xf)) << 12, 4u8)
            } else {
                let low_mask = (1u64 << (9 * level)) - 1;
                if ppn & low_mask != 0 {
                    return Err(page_fault);
                }
                ((ppn | ((gva >> 12) & low_mask)) << 12, (9 * level) as u8)
            };
            let needs_dirty = matches!(acc, Access::Store | Access::ShadowStackStore);
            if henvcfg & envcfg::ADUE == 0
                && (pte & pte::A == 0 || (needs_dirty && pte & pte::D == 0))
            {
                return Err(page_fault);
            }
            let mut new = pte | pte::A;
            if needs_dirty {
                new |= pte::D;
            }
            if new != pte {
                bus.write(pte_spa, AccessSize::Double, new)
                    .map_err(|_| Exception {
                        gva: true,
                        ..acc.access_fault(gva)
                    })?;
            }
            return Ok((gpa_page | (gva & 0xfff), new | global, vpn_shift, pbmt));
        }
        unreachable!("loop always returns at level 0")
    }

    /// G-stage translation of guest physical `gpa` (for the access that
    /// started at guest virtual `gva`). `implicit` is the access type of a
    /// VS-stage page-table access; faults then still report `acc`'s
    /// guest-page-fault cause. Returns the supervisor physical address, the
    /// leaf flags and the PBMT value.
    fn g_stage(
        &mut self,
        bus: &AddressSpace,
        gpa: u64,
        gva: u64,
        acc: Access,
        ctx: VsCtx,
        implicit: Option<Access>,
    ) -> Result<(u64, u64, u64), Exception> {
        let hgatp = self.csr.h.hgatp;
        let fault = acc.guest_page_fault(gva, gpa);
        let levels: u32 = match hgatp >> 60 {
            0 => return Ok((gpa, pte::R | pte::W | pte::X | pte::U | pte::A | pte::D, 0)),
            8 => 3,
            9 => 4,
            _ => 5,
        };
        // Sv39x4/Sv48x4/Sv57x4: two more address bits, a 16 KiB root table.
        if gpa >> (12 + 9 * levels + 2) != 0 {
            return Err(fault);
        }
        let mut table = (hgatp & pte::PPN_MASK) << 12;
        for level in (0..levels).rev() {
            let idx_mask = if level == levels - 1 { 0x7ff } else { 0x1ff };
            let idx = (gpa >> (12 + 9 * level)) & idx_mask;
            let pte_addr = table + idx * 8;
            if self.pmp_enforced && !self.pmp_check(pte_addr, 8, Priv::Supervisor, Access::Load) {
                return Err(Exception {
                    gva: true,
                    ..acc.access_fault(gva)
                });
            }
            let pte = bus
                .read(pte_addr, AccessSize::Double)
                .map_err(|_| Exception {
                    gva: true,
                    ..acc.access_fault(gva)
                })?;
            if pte & pte::V == 0
                || (pte & pte::R == 0 && pte & pte::W != 0)
                || pte & pte::RESERVED_HIGH_MASK != 0
            {
                return Err(fault);
            }
            let pbmt = (pte & pte::PBMT_MASK) >> pte::PBMT_SHIFT;
            let ppn = (pte >> 10) & pte::PPN_MASK;
            if pte & (pte::R | pte::W | pte::X) == 0 {
                if level == 0 || pte & (pte::D | pte::A | pte::U | pte::N) != 0 || pbmt != 0 {
                    return Err(fault);
                }
                table = ppn << 12;
                continue;
            }
            if pbmt != 0 && (self.csr.menvcfg & envcfg::PBMTE == 0 || pbmt == 3) {
                return Err(fault);
            }
            // G-stage permissions treat every access as a U-mode access.
            let perm = implicit.unwrap_or(if ctx.hlvx { Access::Fetch } else { acc });
            let ok = pte & pte::U != 0
                && match perm {
                    Access::Fetch => pte & pte::X != 0,
                    Access::Load | Access::ShadowStackLoad => {
                        pte & pte::R != 0 || (ctx.hs_mxr && pte & pte::X != 0)
                    }
                    Access::Store | Access::ShadowStackStore => pte & pte::W != 0,
                    Access::CacheBom => {
                        pte & (pte::R | pte::W) != 0 || (ctx.hs_mxr && pte & pte::X != 0)
                    }
                };
            if !ok {
                return Err(fault);
            }
            let page = if pte & pte::N != 0 {
                if level != 0 || (ppn & 0xf) != 0x8 {
                    return Err(fault);
                }
                ((ppn & !0xf) | ((gpa >> 12) & 0xf)) << 12
            } else {
                let low_mask = (1u64 << (9 * level)) - 1;
                if ppn & low_mask != 0 {
                    return Err(fault);
                }
                (ppn | ((gpa >> 12) & low_mask)) << 12
            };
            let needs_dirty = matches!(perm, Access::Store | Access::ShadowStackStore);
            if self.csr.menvcfg & envcfg::ADUE == 0
                && (pte & pte::A == 0 || (needs_dirty && pte & pte::D == 0))
            {
                return Err(fault);
            }
            let mut new = pte | pte::A;
            if needs_dirty {
                new |= pte::D;
            }
            if new != pte {
                bus.write(pte_addr, AccessSize::Double, new)
                    .map_err(|_| Exception {
                        gva: true,
                        ..acc.access_fault(gva)
                    })?;
            }
            return Ok((page | (gpa & 0xfff), new, pbmt));
        }
        unreachable!("loop always returns at level 0")
    }

    /// `hlv`/`hlvx`: a load with two-stage translation at `hstatus.SPVP`'s
    /// privilege. `funct` is the decoded size code | kind << 2.
    pub(crate) fn hyp_load(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        funct: u8,
    ) -> Result<u64, Exception> {
        let size = funct & 3;
        let kind = funct >> 2;
        let n = 1u64 << size;
        // hlvx reads are not pointer-masked (as QEMU).
        let va = if kind == 3 {
            va
        } else {
            self.hyp_mask_addr(va)
        };
        let prv = if self.csr.h.hstatus & crate::csr::hstatus::SPVP != 0 {
            Priv::Supervisor
        } else {
            Priv::User
        };
        let ctx = self.vs_ctx_bank(prv, kind == 3);
        let mut v = 0u64;
        // Byte by byte across a page boundary.
        let chunks: &[(u64, u64)] = if (va & 0xfff) + n > 0x1000 {
            &[
                (0, 1),
                (1, 1),
                (2, 1),
                (3, 1),
                (4, 1),
                (5, 1),
                (6, 1),
                (7, 1),
            ][..n as usize]
        } else {
            &[(0, 0)]
        };
        for &(i, bytes) in chunks {
            let (len, at) = if bytes == 0 {
                (n, va)
            } else {
                (1, va.wrapping_add(i))
            };
            let (pa, _) = self.translate_two_stage(bus, at, Access::Load, ctx, false)?;
            if pa < 0x8000_0000 {
                self.nondeterministic_read = true;
            }
            let sz = access_size(len);
            let part = bus.read(pa, sz).map_err(|_| Exception {
                gva: true,
                ..Access::Load.access_fault(va)
            })?;
            v |= part << (8 * i);
        }
        Ok(match (size, kind) {
            (0, 0) => v as i8 as u64,
            (1, 0) => v as i16 as u64,
            (2, 0) => v as i32 as u64,
            _ => v,
        })
    }

    /// `hsv`: a store with two-stage translation (see [`Self::hyp_load`]).
    pub(crate) fn hyp_store(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        funct: u8,
        value: u64,
    ) -> Result<(), Exception> {
        let n = 1u64 << (funct & 3);
        let va = self.hyp_mask_addr(va);
        let prv = if self.csr.h.hstatus & crate::csr::hstatus::SPVP != 0 {
            Priv::Supervisor
        } else {
            Priv::User
        };
        let ctx = self.vs_ctx_bank(prv, false);
        let pieces: Vec<(u64, u64)> = if (va & 0xfff) + n > 0x1000 {
            (0..n).map(|i| (i, 1)).collect()
        } else {
            vec![(0, n)]
        };
        // Translate every piece before writing any.
        let mut pas = Vec::with_capacity(pieces.len());
        for &(i, len) in &pieces {
            let (pa, _) =
                self.translate_two_stage(bus, va.wrapping_add(i), Access::Store, ctx, false)?;
            pas.push((pa, i, len));
        }
        for (pa, i, len) in pas {
            bus.invalidate_cache_line(pa, access_size(len));
            bus.write(pa, access_size(len), value >> (8 * i))
                .map_err(|_| Exception {
                    gva: true,
                    ..Access::Store.access_fault(va)
                })?;
        }
        Ok(())
    }
}

impl Hart {
    /// Ssnpm pointer masking of an `hlv`/`hsv` address, as QEMU's
    /// `riscv_pm_get_vm_ldst_pmm`: `henvcfg.PMM` for VS-mode accesses
    /// (`hstatus.SPVP`), for VU-mode accesses `hstatus.HUPMM` when executed
    /// in U-mode and `senvcfg.PMM` otherwise; off when HS-mode's or
    /// VS-mode's `MXR` is set.
    fn hyp_mask_addr(&self, va: u64) -> u64 {
        use crate::csr::hstatus;
        let c = &self.csr;
        if (c.mstatus | c.h.bank.status) & status::MXR != 0 {
            return va;
        }
        let pmm = if c.h.hstatus & hstatus::SPVP != 0 {
            (c.h.henvcfg & envcfg::PMM_MASK) >> envcfg::PMM_SHIFT
        } else if self.prv == Priv::User {
            (c.h.hstatus & hstatus::HUPMM_MASK) >> hstatus::HUPMM_SHIFT
        } else {
            (c.senvcfg & envcfg::PMM_MASK) >> envcfg::PMM_SHIFT
        };
        let pmlen = match pmm {
            2 => 7,
            3 => 16,
            _ => return va,
        };
        if c.h.bank.satp >> 60 != 0 {
            ((va << pmlen) as i64 >> pmlen) as u64
        } else {
            (va << pmlen) >> pmlen
        }
    }
}

fn access_size(bytes: u64) -> AccessSize {
    match bytes {
        1 => AccessSize::Byte,
        2 => AccessSize::Half,
        4 => AccessSize::Word,
        _ => AccessSize::Double,
    }
}
