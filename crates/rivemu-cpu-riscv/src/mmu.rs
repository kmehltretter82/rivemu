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
    pub(super) const PBMT_SHIFT: u32 = 61;
    pub(super) const PBMT_MASK: u64 = 3 << PBMT_SHIFT;
    pub(super) const RESERVED_HIGH_MASK: u64 = !((1u64 << 54) - 1) & !PBMT_MASK;
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
    /// Page-table level (`0` = 4 KiB, `1` = 2 MiB, `2` = 1 GiB, `3` = 512 GiB).
    level: u8,
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
            level: 0,
        }
    }
}

/// Direct-mapped TLB caching leaf translations. Permissions are re-checked
/// on every hit, so privilege/MPRV/SUM/MXR changes need no flush.
#[derive(Clone, Debug)]
pub(crate) struct Tlb {
    entries: Box<[TlbEntry; TLB_SIZE]>,
    epoch: u32,
    has_superpages: bool,
}

impl Default for Tlb {
    fn default() -> Self {
        Self {
            entries: Box::new([TlbEntry::default(); TLB_SIZE]),
            epoch: 1,
            has_superpages: false,
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
        self.has_superpages = false;
    }

    /// Selective TLB invalidation for `sfence.vma` / `sinval.vma` per RISC-V
    /// Privileged Spec Section 12.2.1.
    pub(crate) fn sfence_vma(&mut self, rs1_idx: u8, rs2_idx: u8, va: u64, asid: u64) {
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
                            && ((e.tag - 1) ^ vpn) >> (9 * u32::from(e.level)) == 0
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
                            && ((e.tag - 1) ^ vpn) >> (9 * u32::from(e.level)) == 0
                        {
                            e.tag = 0;
                        }
                    }
                }
            }
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

    /// Applies `Ssnpm`/`Smnpm` pointer masking (`PMLEN = 7` or `16`) to a data
    /// virtual address when enabled for the effective data privilege level.
    #[inline]
    pub(crate) fn mask_data_addr(&self, va: u64) -> u64 {
        let prv = self.effective_priv(Access::Load);
        let pmlen = self.csr.effective_pmlen(prv);
        if pmlen == 0 {
            return va;
        }
        if prv != Priv::Machine && (self.csr.satp >> 60) != 0 {
            ((va << pmlen) as i64 >> pmlen) as u64
        } else {
            (va << pmlen) >> pmlen
        }
    }

    #[inline]
    fn pte_permits(&self, flags: u64, prv: Priv, acc: Access) -> bool {
        let s = self.csr.mstatus;
        let is_ss_pte =
            (flags & (pte::R | pte::W | pte::X)) == pte::W && (self.csr.menvcfg & envcfg::SSE != 0);
        let ok = match acc {
            Access::Fetch => !is_ss_pte && (flags & pte::X != 0),
            Access::Load => {
                is_ss_pte
                    || (flags & pte::R != 0)
                    || (s & status::MXR != 0 && flags & pte::X != 0)
            }
            Access::Store => !is_ss_pte && (flags & pte::W != 0),
            Access::CacheBom => {
                !is_ss_pte
                    && ((flags & (pte::R | pte::W) != 0)
                        || (s & status::MXR != 0 && flags & pte::X != 0))
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
                    ) && s & status::SUM != 0)
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
        if prv == Priv::Machine && !self.pmp_any_locked && (!self.pmp_subpage || size <= 1) {
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
                    Access::Load => cfg & 0x01 != 0,
                    Access::Store => cfg & 0x02 != 0,
                    Access::CacheBom => cfg & 0x03 != 0,
                    Access::ShadowStackLoad | Access::ShadowStackStore => (cfg & 0x03) == 0x03,
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

    /// Translates a virtual address and returns `(pa, fast_ram_offset)` where
    /// `fast_ram_offset != u64::MAX` when the translated 4 KiB page has
    /// `PBMT == 0`, `!pmp_enforced`, and lies inside `AddressSpace::fast_ram`.
    #[inline]
    pub(crate) fn translate_with_ram_off(
        &mut self,
        bus: &AddressSpace,
        va: u64,
        acc: Access,
    ) -> Result<(u64, u64), Exception> {
        let prv = self.effective_priv(acc);
        let mode = self.csr.satp >> 60;
        if prv == Priv::Machine || mode == 0 {
            if matches!(acc, Access::ShadowStackLoad | Access::ShadowStackStore) {
                return Err(acc.access_fault(va));
            }
            if self.pmp_enforced
                && (prv != Priv::Machine || self.pmp_any_locked)
                && !self.pmp_check(va, 1, prv, acc)
            {
                return Err(acc.access_fault(va));
            }
            return Ok((va, u64::MAX));
        }
        if mode == 8
            && prv == Priv::User
            && (self.csr.senvcfg & envcfg::UKTE) != 0
            && (va >> 63) != 0
        {
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
                if !self.pmp_check(pa, 1, prv, acc) {
                    return Err(acc.access_fault(va));
                }
                return Ok((pa, u64::MAX));
            }
            let ram_off = if e.ram_page_off != u64::MAX {
                e.ram_page_off | off
            } else {
                u64::MAX
            };
            return Ok((pa, ram_off));
        }
        let pa = self.walk(bus, va, acc, prv, mode)?;
        if self.pmp_enforced && !self.pmp_check(pa, 1, prv, acc) {
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
        let mode = self.csr.satp >> 60;
        if self.prv == Priv::Machine || mode == 0 {
            return Some(va);
        }
        if mode == 8
            && self.prv == Priv::User
            && (self.csr.senvcfg & envcfg::UKTE) != 0
            && (va >> 63) != 0
        {
            return None;
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
        if mode == 8
            && prv == Priv::User
            && (self.csr.senvcfg & envcfg::UKTE) != 0
            && (va >> 63) != 0
        {
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
            let low_mask = (1u64 << (9 * level)) - 1;
            if ppn & low_mask != 0 {
                return Err(acc.page_fault(va)); // misaligned superpage
            }
            // Svade vs Svadu: when Svade is active (`self.svade` and `menvcfg.ADUE == 0`),
            // missing A (or D on a store / shadow-stack store) raises a page fault instead of updating the PTE.
            let needs_dirty = matches!(acc, Access::Store | Access::ShadowStackStore);
            if self.svade
                && self.csr.menvcfg & envcfg::ADUE == 0
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
            let pa_page = (ppn | ((va >> 12) & low_mask)) << 12;
            let ram_page_off = if pbmt == 0 {
                bus.fast_ram_page_offset(pa_page).unwrap_or(u64::MAX)
            } else {
                u64::MAX
            };
            if level > 0 {
                self.tlb.has_superpages = true;
            }
            self.tlb.entries[((va >> 12) as usize) & (TLB_SIZE - 1)] = TlbEntry {
                tag: (va >> 12) + 1,
                pa_page,
                ram_page_off,
                epoch: self.tlb.epoch,
                asid,
                flags: (new | global) as u8,
                level: level as u8,
            };
            return Ok(pa_page | (va & 0xfff));
        }
        unreachable!("loop always returns at level 0")
    }
}
