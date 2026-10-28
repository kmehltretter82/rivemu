//! Sv39/Sv48 address translation with a small software TLB.

use rivemu_core::AccessSize;
use rivemu_mem::AddressSpace;

use crate::csr::status;
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
            return Ok(e.pa_page | (va & 0xfff));
        }
        self.walk(bus, va, acc, prv, mode)
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
            if pte & pte::V == 0 || (pte & pte::R == 0 && pte & pte::W != 0) || pte >> 54 != 0 {
                return None;
            }
            let ppn = (pte >> 10) & pte::PPN_MASK;
            if pte & (pte::R | pte::X) == 0 {
                if level == 0 || pte & (pte::D | pte::A | pte::U) != 0 {
                    return None;
                }
                table = ppn << 12;
                continue;
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
            let pte = bus
                .read(pte_addr, AccessSize::Double)
                .map_err(|_| acc.access_fault(va))?;
            if pte & pte::V == 0 || (pte & pte::R == 0 && pte & pte::W != 0) || pte >> 54 != 0 {
                return Err(acc.page_fault(va));
            }
            let ppn = (pte >> 10) & pte::PPN_MASK;
            if pte & (pte::R | pte::X) == 0 {
                // Pointer to the next level.
                if level == 0 || pte & (pte::D | pte::A | pte::U) != 0 {
                    return Err(acc.page_fault(va));
                }
                table = ppn << 12;
                continue;
            }
            // Leaf.
            if !self.pte_permits(pte, prv, acc) {
                return Err(acc.page_fault(va));
            }
            let low_mask = (1u64 << (9 * level)) - 1;
            if ppn & low_mask != 0 {
                return Err(acc.page_fault(va)); // misaligned superpage
            }
            // Hardware A/D update.
            let mut new = pte | pte::A;
            if acc == Access::Store {
                new |= pte::D;
            }
            if new != pte {
                bus.write(pte_addr, AccessSize::Double, new)
                    .map_err(|_| acc.access_fault(va))?;
            }
            let pa_page = (ppn | ((va >> 12) & low_mask)) << 12;
            self.tlb.entries[((va >> 12) as usize) & (TLB_SIZE - 1)] = TlbEntry {
                tag: (va >> 12) + 1,
                pa_page,
                epoch: self.tlb.epoch,
                flags: new as u8,
            };
            return Ok(pa_page | (va & 0xfff));
        }
        unreachable!("loop always returns at level 0")
    }
}
