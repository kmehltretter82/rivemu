//! Sv39/Sv48 address translation with a small software TLB.

use rivemu_core::AccessSize;
use rivemu_mem::AddressSpace;

use crate::csr::{PMP_ENTRIES, envcfg, status};
use crate::hart::{Exception, Hart, Priv, cause};
use crate::log::LogFlags;

/// Kind of memory access being translated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    Fetch,
    Load,
    /// Stores and AMOs.
    Store,
}

impl Access {
    fn page_fault(self, va: u64) -> Exception {
        let c = match self {
            Access::Fetch => cause::INSN_PAGE_FAULT,
            Access::Load => cause::LOAD_PAGE_FAULT,
            Access::Store => cause::STORE_PAGE_FAULT,
        };
        Exception::new(c, va)
    }

    fn access_fault(self, va: u64) -> Exception {
        let c = match self {
            Access::Fetch => cause::INSN_ACCESS,
            Access::Load => cause::LOAD_ACCESS,
            Access::Store => cause::STORE_ACCESS,
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
    pub(super) const A: u64 = 1 << 6;
    pub(super) const D: u64 = 1 << 7;
    pub(super) const PPN_MASK: u64 = (1 << 44) - 1;
    pub(super) const PBMT_SHIFT: u32 = 61;
    pub(super) const PBMT_MASK: u64 = 3 << PBMT_SHIFT;
    pub(super) const RESERVED_HIGH_MASK: u64 = !((1u64 << 54) - 1) & !PBMT_MASK;
}

const TLB_SIZE: usize = 1024;

#[derive(Clone, Copy, Debug, Default)]
struct TlbEntry {
    /// `(va >> 12) + 1` (0 means invalid).
    tag: u64,
    /// Physical address of the 4 KiB page.
    pa_page: u64,
    /// Flush epoch matching [`Tlb::epoch`].
    epoch: u32,
    /// Leaf PTE flags (low 8 bits).
    flags: u8,
}

/// Direct-mapped TLB caching leaf translations. Permissions are re-checked
/// on every hit, so privilege/MPRV/SUM/MXR changes need no flush.
#[derive(Clone, Debug)]
pub(crate) struct Tlb {
    entries: Box<[TlbEntry; TLB_SIZE]>,
    epoch: u32,
}

impl Default for Tlb {
    fn default() -> Self {
        Self {
            entries: Box::new([TlbEntry::default(); TLB_SIZE]),
            epoch: 1,
        }
    }
}

impl Tlb {
    #[inline]
    pub(crate) fn flush(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.entries.fill(TlbEntry::default());
            self.epoch = 1;
        }
    }
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

    #[inline]
    fn pte_permits(&self, flags: u64, prv: Priv, acc: Access) -> bool {
        let s = self.csr.mstatus;
        let ok = match acc {
            Access::Fetch => flags & pte::X != 0,
            Access::Load => flags & pte::R != 0 || (s & status::MXR != 0 && flags & pte::X != 0),
            Access::Store => flags & pte::W != 0,
        };
        ok && match prv {
            Priv::User => flags & pte::U != 0,
            Priv::Supervisor => {
                flags & pte::U == 0 || (acc != Access::Fetch && s & status::SUM != 0)
            }
            Priv::Machine => true,
        }
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
        self.pmp_count = count as u8;
        self.pmp_any_locked = any_locked;
        // Entry 0 as an unlocked NAPOT catch-all (`R|W|X`) covering >= 56 bits
        // grants unconditional access to all privileges.
        let full_open_entry0 =
            (self.csr.pmpcfg[0] & 0x9f) == 0x1f && (!self.csr.pmpaddr[0]).trailing_zeros() >= 53;
        self.pmp_enforced = !full_open_entry0 && (any_active || self.strict_pmp);
        self.pmp_subpage = self.pmp_enforced && subpage;
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
    /// Privileged Spec Section 3.7.
    #[cold]
    pub(crate) fn pmp_check(&self, pa: u64, size: u64, prv: Priv, acc: Access) -> bool {
        if prv == Priv::Machine && !self.pmp_any_locked {
            return true;
        }
        let last = pa.saturating_add(size.saturating_sub(1));
        for &(lo, hi, cfg) in &self.pmp_ranges[..self.pmp_count as usize] {
            if pa <= hi && last >= lo {
                // Accesses straddling a PMP boundary must fault.
                if pa < lo || last > hi {
                    return false;
                }
                // Unlocked M-mode accesses always succeed.
                if prv == Priv::Machine && cfg & 0x80 == 0 {
                    return true;
                }
                return match acc {
                    Access::Fetch => cfg & 0x04 != 0,
                    Access::Load => {
                        cfg & 0x01 != 0 || (self.csr.mstatus & status::MXR != 0 && cfg & 0x04 != 0)
                    }
                    Access::Store => cfg & 0x02 != 0,
                };
            }
        }
        if prv == Priv::Machine {
            true
        } else {
            !self.strict_pmp && self.pmp_count == 0
        }
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

    /// Translates a virtual address.
    #[inline]
    pub(crate) fn translate(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        acc: Access,
    ) -> Result<u64, Exception> {
        let prv = self.effective_priv(acc);
        let mode = self.csr.satp >> 60;
        if prv == Priv::Machine || mode == 0 {
            if self.pmp_enforced
                && (prv != Priv::Machine || self.pmp_any_locked)
                && !self.pmp_check(va, 1, prv, acc)
            {
                return Err(acc.access_fault(va));
            }
            return Ok(va);
        }
        let tag = (va >> 12) + 1;
        let e = self.tlb.entries[((va >> 12) as usize) & (TLB_SIZE - 1)];
        let flags = u64::from(e.flags);
        if e.tag == tag
            && e.epoch == self.tlb.epoch
            && self.pte_permits(flags, prv, acc)
            && (acc != Access::Store || flags & pte::D != 0)
        {
            let pa = e.pa_page | (va & 0xfff);
            if self.pmp_subpage && !self.pmp_check(pa, 1, prv, acc) {
                return Err(acc.access_fault(va));
            }
            return Ok(pa);
        }
        let pa = self.walk(bus, va, acc, prv, mode)?;
        if self.pmp_enforced && !self.pmp_check(pa, 1, prv, acc) {
            return Err(acc.access_fault(va));
        }
        Ok(pa)
    }

    /// Side-effect-free virtual-to-physical translation for debuggers (GDB).
    pub fn debug_translate(&self, bus: &AddressSpace, va: u64) -> Option<u64> {
        let mode = self.csr.satp >> 60;
        if self.prv == Priv::Machine || mode == 0 {
            return Some(va);
        }
        let levels: u32 = match mode {
            8 => 3,
            9 => 4,
            _ => return None,
        };
        let va_bits = 12 + 9 * levels;
        let shift = 64 - va_bits;
        if ((va << shift) as i64 >> shift) as u64 != va {
            return None;
        }
        let mut table = (self.csr.satp & pte::PPN_MASK) << 12;
        for level in (0..levels).rev() {
            let idx = (va >> (12 + 9 * level)) & 0x1ff;
            let pte = bus.read(table + idx * 8, AccessSize::Double).ok()?;
            if pte & pte::V == 0
                || (pte & pte::R == 0 && pte & pte::W != 0)
                || pte & pte::RESERVED_HIGH_MASK != 0
            {
                return None;
            }
            let pbmt = (pte & pte::PBMT_MASK) >> pte::PBMT_SHIFT;
            let ppn = (pte >> 10) & pte::PPN_MASK;
            if pte & (pte::R | pte::X) == 0 {
                if level == 0 || pte & (pte::D | pte::A | pte::U) != 0 || pbmt != 0 {
                    return None;
                }
                table = ppn << 12;
                continue;
            }
            if pbmt != 0 && (self.csr.menvcfg & envcfg::PBMTE == 0 || pbmt == 3) {
                return None;
            }
            let low_mask = (1u64 << (9 * level)) - 1;
            if ppn & low_mask != 0 {
                return None;
            }
            let pa_page = (ppn | ((va >> 12) & low_mask)) << 12;
            return Some(pa_page | (va & 0xfff));
        }
        None
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
        let levels: u32 = if mode == 8 { 3 } else { 4 };
        let va_bits = 12 + 9 * levels;
        let shift = 64 - va_bits;
        if ((va << shift) as i64 >> shift) as u64 != va {
            return Err(acc.page_fault(va));
        }
        let mut table = (self.csr.satp & pte::PPN_MASK) << 12;
        for level in (0..levels).rev() {
            let idx = (va >> (12 + 9 * level)) & 0x1ff;
            let pte_addr = table + idx * 8;
            if self.pmp_enforced && !self.pmp_check(pte_addr, 8, Priv::Supervisor, Access::Load) {
                return Err(acc.access_fault(va));
            }
            let pte = bus
                .read(pte_addr, AccessSize::Double)
                .map_err(|_| acc.access_fault(va))?;
            if pte & pte::V == 0
                || (pte & pte::R == 0 && pte & pte::W != 0)
                || pte & pte::RESERVED_HIGH_MASK != 0
            {
                return Err(acc.page_fault(va));
            }
            let pbmt = (pte & pte::PBMT_MASK) >> pte::PBMT_SHIFT;
            let ppn = (pte >> 10) & pte::PPN_MASK;
            if pte & (pte::R | pte::X) == 0 {
                // Pointer to the next level.
                if level == 0 || pte & (pte::D | pte::A | pte::U) != 0 || pbmt != 0 {
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
                return Err(acc.page_fault(va));
            }
            let low_mask = (1u64 << (9 * level)) - 1;
            if ppn & low_mask != 0 {
                return Err(acc.page_fault(va)); // misaligned superpage
            }
            // Svade vs Svadu: when Svade is active (`self.svade` and `menvcfg.ADUE == 0`),
            // missing A (or D on a store) raises a page fault instead of updating the PTE.
            if self.svade
                && self.csr.menvcfg & envcfg::ADUE == 0
                && (pte & pte::A == 0 || (acc == Access::Store && pte & pte::D == 0))
            {
                return Err(acc.page_fault(va));
            }
            // Hardware A/D update.
            let mut new = pte | pte::A;
            if acc == Access::Store {
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
            let pa_page = (ppn | ((va >> 12) & low_mask)) << 12;
            let mut tlb_flags = new as u8;
            if self.pmp_enforced && !self.pmp_subpage {
                if !self.pmp_check(pa_page, 4096, Priv::Supervisor, Access::Load) {
                    tlb_flags &= !(pte::R as u8);
                }
                if !self.pmp_check(pa_page, 4096, Priv::Supervisor, Access::Store) {
                    tlb_flags &= !(pte::W as u8);
                }
                if !self.pmp_check(pa_page, 4096, Priv::Supervisor, Access::Fetch) {
                    tlb_flags &= !(pte::X as u8);
                }
            }
            self.tlb.entries[((va >> 12) as usize) & (TLB_SIZE - 1)] = TlbEntry {
                tag: (va >> 12) + 1,
                pa_page,
                epoch: self.tlb.epoch,
                flags: tlb_flags,
            };
            return Ok(pa_page | (va & 0xfff));
        }
        unreachable!("loop always returns at level 0")
    }
}
