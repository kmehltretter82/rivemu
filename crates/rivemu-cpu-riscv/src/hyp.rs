//! H extension (hypervisor): virtualization mode, the hypervisor and VS
//! CSRs, CSR access rules, and interrupt selection with VS-level
//! interrupts. Two-stage address translation is in `mmu.rs`.
//!
//! The S-level CSRs are kept in two copies, swapped whenever V changes (as
//! QEMU does): while V = 1 the active `mstatus` S fields, `stvec`,
//! `sscratch`, `sepc`, `scause`, `stval` and `satp` hold the VS copies, so
//! the existing S-mode trap, `sret`, CSR and translation paths serve VS-mode
//! unchanged; [`csr::SBank`] holds the other copy.

use crate::csr::{self, envcfg, hstatus, irq, status};
use crate::hart::{Exception, Hart, Priv, cause};

/// `hgatp.PPN` (bits 43:0).
const fn pte_ppn_mask() -> u64 {
    (1 << 44) - 1
}

/// Words in [`Hart::h_snapshot`].
pub const H_SNAPSHOT_WORDS: usize = 24;

impl Hart {
    /// The H-extension state for snapshots: V, the hypervisor CSRs and the
    /// inactive S-level bank (the active one is in the S-level CSRs).
    pub fn h_snapshot(&self) -> [u64; H_SNAPSHOT_WORDS] {
        let h = &self.csr.h;
        let b = &h.bank;
        [
            u64::from(self.virt),
            h.hstatus,
            h.hedeleg,
            h.hideleg,
            h.hcounteren,
            h.htimedelta,
            h.htval,
            h.htinst,
            h.hgatp,
            h.henvcfg,
            h.hstateen[0],
            h.hstateen[1],
            h.hstateen[2],
            h.hstateen[3],
            h.vstimecmp,
            h.mtval2,
            h.mtinst,
            b.status,
            b.stvec,
            b.sscratch,
            b.sepc,
            b.scause,
            b.stval,
            b.satp,
        ]
    }

    /// Restores [`Self::h_snapshot`]. Fails when it has V = 1 but the
    /// hart does not implement H.
    pub fn restore_h_snapshot(&mut self, s: &[u64; H_SNAPSHOT_WORDS]) -> Result<(), &'static str> {
        let virt = s[0] != 0;
        if virt && !self.h_ext {
            return Err("snapshot runs a guest (V = 1) but the hart has no H extension");
        }
        let h = &mut self.csr.h;
        [
            h.hstatus,
            h.hedeleg,
            h.hideleg,
            h.hcounteren,
            h.htimedelta,
            h.htval,
            h.htinst,
            h.hgatp,
            h.henvcfg,
        ] = [s[1], s[2], s[3], s[4], s[5], s[6], s[7], s[8], s[9]];
        h.hstateen = [s[10], s[11], s[12], s[13]];
        [h.vstimecmp, h.mtval2, h.mtinst] = [s[14], s[15], s[16]];
        let b = &mut h.bank;
        [
            b.status, b.stvec, b.sscratch, b.sepc, b.scause, b.stval, b.satp,
        ] = [s[17], s[18], s[19], s[20], s[21], s[22], s[23]];
        // The S-level CSRs already hold the active bank: no swap.
        self.virt = virt;
        self.csr.h.virt = virt;
        Ok(())
    }

    /// Enables or disables the H extension (before the hart runs).
    pub fn set_hypervisor(&mut self, on: bool) {
        self.h_ext = on;
    }

    /// Switches the virtualization mode, swapping the S-level CSR bank.
    pub(crate) fn set_virt(&mut self, v: bool) {
        if v == self.virt {
            return;
        }
        let c = &mut self.csr;
        let b = &mut c.h.bank;
        let active = c.mstatus & status::VS_SWAPPED;
        c.mstatus = (c.mstatus & !status::VS_SWAPPED) | (b.status & status::VS_SWAPPED);
        b.status = active;
        std::mem::swap(&mut c.stvec, &mut b.stvec);
        std::mem::swap(&mut c.sscratch, &mut b.sscratch);
        std::mem::swap(&mut c.sepc, &mut b.sepc);
        std::mem::swap(&mut c.scause, &mut b.scause);
        std::mem::swap(&mut c.stval, &mut b.stval);
        std::mem::swap(&mut c.satp, &mut b.satp);
        self.virt = v;
        self.csr.h.virt = v;
        // Cached translations belong to one virtualization mode.
        self.flush_tlb();
    }

    /// `mideleg` as the hart uses it: with H, the VS-level and guest
    /// external interrupts are always delegated to HS-mode.
    #[inline]
    pub(crate) fn mideleg_eff(&self) -> u64 {
        if self.h_ext {
            self.csr.mideleg | irq::HS_FORCED
        } else {
            self.csr.mideleg
        }
    }

    /// The VS-level timer interrupt from `vstimecmp` (Sstc with H).
    #[inline]
    pub(crate) fn vstimer_pending(&self) -> bool {
        self.h_ext
            && self.csr.menvcfg & envcfg::STCE != 0
            && self.csr.h.henvcfg & envcfg::STCE != 0
            && self.timebase.now().wrapping_add(self.csr.h.htimedelta) >= self.csr.h.vstimecmp
    }

    /// Picks the interrupt to take with H: M-level, then HS-level, then
    /// VS-level, each enabled as the privileged spec (and QEMU) define.
    pub(crate) fn h_select_interrupt(&self, pending: u64) -> Option<u64> {
        let mstatus = self.csr.mstatus;
        let (m_en, hs_en, vs_en) = if self.virt {
            let vs = self.prv < Priv::Supervisor
                || (self.prv == Priv::Supervisor && mstatus & status::SIE != 0);
            (true, true, vs)
        } else {
            let m = self.prv < Priv::Machine || mstatus & status::MIE != 0;
            let hs = self.prv < Priv::Supervisor
                || (self.prv == Priv::Supervisor && mstatus & status::SIE != 0);
            (m, hs, false)
        };
        let mideleg = self.mideleg_eff();
        let hideleg = self.csr.h.hideleg;
        let pick = |bits: u64| irq::PRIORITY_H.into_iter().find(|&c| bits & (1 << c) != 0);
        let m = pending & !mideleg;
        if m_en && m != 0 {
            return pick(m);
        }
        let hs = pending & mideleg & !hideleg;
        if hs_en && hs != 0 {
            return pick(hs);
        }
        let vs = pending & mideleg & hideleg;
        if vs_en && vs != 0 {
            return pick(vs);
        }
        None
    }

    /// The exception for a CSR access that the H extension forbids, if any:
    /// QEMU's `riscv_csrrw_check` order (read-only write, then the CSR's own
    /// predicate, then the privilege level), with a virtual-instruction
    /// exception where an access would be allowed in HS-mode (or U-mode
    /// with V = 0) but is made with V = 1.
    pub(crate) fn h_csr_check(&self, num: u16, writes: bool, raw: u32) -> Result<(), Exception> {
        let illegal = Exception::new(cause::ILLEGAL_INSN, u64::from(raw));
        let virtual_insn = Exception::new(cause::VIRTUAL_INSN, u64::from(raw));
        if writes && num >> 10 == 3 {
            return Err(illegal);
        }
        let c = &self.csr;
        let prv = self.prv;
        match num {
            csr::CYCLE..=csr::HPMCOUNTER31 => {
                let bit = 1u64 << (num - csr::CYCLE);
                if prv < Priv::Machine && c.mcounteren & bit == 0 {
                    return Err(illegal);
                }
                if self.virt
                    && (c.h.hcounteren & bit == 0 || (prv == Priv::User && c.scounteren & bit == 0))
                {
                    return Err(virtual_insn);
                }
                if prv == Priv::User && c.scounteren & bit == 0 {
                    return Err(illegal);
                }
            }
            csr::SATP => {
                if prv == Priv::Supervisor && !self.virt && c.mstatus & status::TVM != 0 {
                    return Err(illegal);
                }
                if prv == Priv::Supervisor && self.virt && c.h.hstatus & hstatus::VTVM != 0 {
                    return Err(virtual_insn);
                }
            }
            csr::HGATP => {
                if prv == Priv::Supervisor && !self.virt && c.mstatus & status::TVM != 0 {
                    return Err(illegal);
                }
            }
            csr::STIMECMP | csr::VSTIMECMP => {
                if prv != Priv::Machine {
                    if c.mcounteren & 2 == 0 || c.menvcfg & envcfg::STCE == 0 {
                        return Err(illegal);
                    }
                    if self.virt && (c.h.hcounteren & 2 == 0 || c.h.henvcfg & envcfg::STCE == 0) {
                        return Err(virtual_insn);
                    }
                }
            }
            csr::SENVCFG => {
                if prv != Priv::Machine && c.mstateen[0] & csr::stateen::ENVCFG == 0 {
                    return Err(illegal);
                }
                if self.virt && c.h.hstateen[0] & csr::stateen::ENVCFG == 0 {
                    return Err(virtual_insn);
                }
            }
            csr::SSTATEEN0..=csr::SSTATEEN3 => {
                let i = usize::from(num - csr::SSTATEEN0);
                if prv != Priv::Machine && c.mstateen[i] & csr::stateen::SE == 0 {
                    return Err(illegal);
                }
                if self.virt && c.h.hstateen[i] & csr::stateen::SE == 0 {
                    return Err(virtual_insn);
                }
            }
            csr::HENVCFG => {
                if prv != Priv::Machine && c.mstateen[0] & csr::stateen::ENVCFG == 0 {
                    return Err(illegal);
                }
            }
            csr::HSTATEEN0..=csr::HSTATEEN3 => {
                let i = usize::from(num - csr::HSTATEEN0);
                if prv != Priv::Machine && c.mstateen[i] & csr::stateen::SE == 0 {
                    return Err(illegal);
                }
            }
            // Zkr: seed from VS/VU always traps, as a virtual instruction
            // only when mseccfg.SSEED would allow it in S-mode.
            csr::SEED if self.virt => {
                return Err(if c.mseccfg & csr::mseccfg::SSEED != 0 {
                    virtual_insn
                } else {
                    illegal
                });
            }
            _ => {}
        }
        // HS-mode may access the hypervisor CSRs (privilege field 2).
        let effective = prv as u16 + u16::from(prv == Priv::Supervisor && !self.virt);
        let csr_priv = (num >> 8) & 3;
        if effective < csr_priv {
            return Err(if csr_priv <= 2 && self.virt {
                virtual_insn
            } else {
                illegal
            });
        }
        Ok(())
    }

    /// Reads a CSR that the H extension adds or redirects. `None` when `num`
    /// is not one of them (the caller handles it); `Some(None)` when it does
    /// not exist.
    pub(crate) fn h_csr_read(&self, num: u16) -> Option<Option<u64>> {
        let c = &self.csr;
        let h = &c.h;
        let vs_view = |bits: u64| (bits & h.hideleg & irq::VS_MASK) >> 1;
        Some(Some(match num {
            // VS-mode's sip/sie/stimecmp are the VS copies.
            csr::SIP if self.virt => vs_view(self.mip()),
            csr::SIE if self.virt => vs_view(c.mie),
            csr::STIMECMP if self.virt => h.vstimecmp,
            csr::MIDELEG => self.mideleg_eff(),
            csr::MTVAL2 => h.mtval2,
            csr::MTINST => h.mtinst,
            csr::HSTATUS => h.hstatus | hstatus::VSXL_64,
            csr::HEDELEG => h.hedeleg,
            csr::HIDELEG => h.hideleg,
            csr::HIE => c.mie & irq::HS_FORCED,
            csr::HTIMEDELTA => h.htimedelta,
            csr::HCOUNTEREN => h.hcounteren,
            csr::HGEIE | csr::HGEIP => 0,
            csr::HENVCFG => c.henvcfg_eff(),
            csr::HSTATEEN0..=csr::HSTATEEN3 => {
                let i = usize::from(num - csr::HSTATEEN0);
                h.hstateen[i] & c.mstateen[i]
            }
            csr::HTVAL => h.htval,
            csr::HIP => self.mip() & irq::HS_FORCED,
            csr::HVIP => c.mip & irq::VS_MASK,
            csr::HTINST => h.htinst,
            csr::HGATP => h.hgatp,
            csr::VSSTATUS => {
                let s = if self.virt {
                    c.mstatus & status::VS_SWAPPED
                } else {
                    h.bank.status
                };
                let dirty = s & status::FS == status::FS || s & status::VS == status::VS;
                (s & status::S_READABLE) | if dirty { status::SD } else { 0 }
            }
            csr::VSIE => vs_view(c.mie),
            csr::VSTVEC | csr::VSSCRATCH | csr::VSEPC | csr::VSCAUSE | csr::VSTVAL | csr::VSATP => {
                *self.vs_copy(num)
            }
            csr::VSIP => vs_view(self.mip()),
            csr::VSTIMECMP => h.vstimecmp,
            _ => return None,
        }))
    }

    /// Writes a CSR that the H extension adds or redirects (see
    /// [`Self::h_csr_read`]).
    pub(crate) fn h_csr_write(&mut self, num: u16, v: u64) -> Option<Option<()>> {
        let hideleg = self.csr.h.hideleg;
        // VS-level bits as VS-mode sees them (shifted down by one).
        let vs_bits = |v: u64| (v << 1) & irq::VS_MASK & hideleg;
        match num {
            csr::SIP if self.virt => {
                let m = irq::VSSIP & hideleg;
                self.csr.mip = (self.csr.mip & !m) | (vs_bits(v) & m);
            }
            csr::SIE if self.virt => {
                let m = irq::VS_MASK & hideleg;
                self.csr.mie = (self.csr.mie & !m) | vs_bits(v);
            }
            csr::STIMECMP if self.virt => self.csr.h.vstimecmp = v,
            csr::MTVAL2 => self.csr.h.mtval2 = v,
            csr::MTINST => self.csr.h.mtinst = v,
            csr::HSTATUS => {
                let mut v = v;
                // HUPMM is WARL: the reserved value 1 keeps the old one.
                if (v & hstatus::HUPMM_MASK) >> hstatus::HUPMM_SHIFT == 1 {
                    v = (v & !hstatus::HUPMM_MASK) | (self.csr.h.hstatus & hstatus::HUPMM_MASK);
                }
                self.csr.h.hstatus = (v & hstatus::WRITABLE) | hstatus::VSXL_64;
            }
            csr::HEDELEG => self.csr.h.hedeleg = v & csr::HEDELEG_MASK,
            csr::HIDELEG => self.csr.h.hideleg = v & csr::HIDELEG_MASK,
            csr::HIE => {
                let m = irq::VS_MASK | irq::SGEIP;
                self.csr.mie = (self.csr.mie & !m) | (v & m);
            }
            csr::HTIMEDELTA => self.csr.h.htimedelta = v,
            csr::HCOUNTEREN => self.csr.h.hcounteren = v & 0xffff_ffff,
            csr::HGEIE => {}
            csr::HENVCFG => {
                let mask =
                    (envcfg::M_MASK & !envcfg::H_GATED) | (envcfg::H_GATED & self.csr.menvcfg);
                self.csr.h.henvcfg = envcfg::sanitize(v, self.csr.h.henvcfg, mask);
                // vsstatus.SDT is read-only zero without henvcfg.DTE.
                if self.csr.h.henvcfg & envcfg::DTE == 0 {
                    self.csr.h.bank.status &= !status::SDT;
                }
                self.flush_tlb();
            }
            csr::HSTATEEN0..=csr::HSTATEEN3 => {
                let i = usize::from(num - csr::HSTATEEN0);
                let mask = if i == 0 {
                    csr::stateen::M0_MASK
                } else {
                    csr::stateen::M1_3_MASK
                };
                self.csr.h.hstateen[i] = v & mask & self.csr.mstateen[i];
            }
            csr::HTVAL => self.csr.h.htval = v,
            csr::HIP => {
                self.csr.mip = (self.csr.mip & !irq::VSSIP) | (v & irq::VSSIP);
            }
            csr::HVIP => {
                self.csr.mip = (self.csr.mip & !irq::VS_MASK) | (v & irq::VS_MASK);
            }
            csr::HTINST => self.csr.h.htinst = v,
            csr::HGATP => {
                // Unlike satp, a write with an unsupported MODE is not
                // ignored: MODE keeps its value and the other fields are
                // written (as Sail; QEMU ignores the write). VMID is 14 bits
                // (57:44), and PPN[1:0] read as zero (the root table is
                // 16 KiB aligned), as the spec allows in every mode.
                let mode = if matches!(v >> 60, 0 | 8 | 9 | 10) {
                    v >> 60
                } else {
                    self.csr.h.hgatp >> 60
                };
                self.csr.h.hgatp = (mode << 60) | (v & ((0x3fff << 44) | (pte_ppn_mask() & !3)));
                self.flush_tlb();
            }
            csr::HGEIP => return Some(None),
            csr::VSSTATUS => {
                let mut m = status::S_WRITABLE & status::VS_SWAPPED;
                if self.csr.henvcfg_eff() & envcfg::DTE == 0 {
                    m &= !status::SDT;
                }
                let s = if self.virt {
                    &mut self.csr.mstatus
                } else {
                    &mut self.csr.h.bank.status
                };
                *s = (*s & !m) | (v & m);
            }
            csr::VSIE => {
                let m = irq::VS_MASK & hideleg;
                self.csr.mie = (self.csr.mie & !m) | vs_bits(v);
            }
            csr::VSTVEC => {
                if v & 2 == 0 {
                    *self.vs_copy_mut(num) = v;
                }
            }
            csr::VSSCRATCH | csr::VSCAUSE | csr::VSTVAL => *self.vs_copy_mut(num) = v,
            csr::VSEPC => *self.vs_copy_mut(num) = v & !1,
            csr::VSIP => {
                let m = irq::VSSIP & hideleg;
                self.csr.mip = (self.csr.mip & !m) | (vs_bits(v) & m);
            }
            csr::VSTIMECMP => self.csr.h.vstimecmp = v,
            csr::VSATP => {
                if matches!(v >> 60, 0 | 8 | 9 | 10) {
                    *self.vs_copy_mut(num) = v;
                    if self.virt {
                        // The running translation changed.
                        self.flush_tlb();
                    }
                }
            }
            _ => return None,
        }
        Some(Some(()))
    }

    /// The VS copy of S-level CSR `vs` (`vstvec`, `vsscratch`, `vsepc`,
    /// `vscause`, `vstval` or `vsatp`): the active register while V = 1
    /// (only a debugger reaches these CSRs then), otherwise the bank.
    fn vs_copy(&self, vs: u16) -> &u64 {
        let c = &self.csr;
        let b = &c.h.bank;
        match (vs, self.virt) {
            (csr::VSTVEC, true) => &c.stvec,
            (csr::VSTVEC, false) => &b.stvec,
            (csr::VSSCRATCH, true) => &c.sscratch,
            (csr::VSSCRATCH, false) => &b.sscratch,
            (csr::VSEPC, true) => &c.sepc,
            (csr::VSEPC, false) => &b.sepc,
            (csr::VSCAUSE, true) => &c.scause,
            (csr::VSCAUSE, false) => &b.scause,
            (csr::VSTVAL, true) => &c.stval,
            (csr::VSTVAL, false) => &b.stval,
            (_, true) => &c.satp,
            (_, false) => &b.satp,
        }
    }

    /// [`Self::vs_copy`], writable.
    fn vs_copy_mut(&mut self, vs: u16) -> &mut u64 {
        let virt = self.virt;
        let c = &mut self.csr;
        let b = &mut c.h.bank;
        match (vs, virt) {
            (csr::VSTVEC, true) => &mut c.stvec,
            (csr::VSTVEC, false) => &mut b.stvec,
            (csr::VSSCRATCH, true) => &mut c.sscratch,
            (csr::VSSCRATCH, false) => &mut b.sscratch,
            (csr::VSEPC, true) => &mut c.sepc,
            (csr::VSEPC, false) => &mut b.sepc,
            (csr::VSCAUSE, true) => &mut c.scause,
            (csr::VSCAUSE, false) => &mut b.scause,
            (csr::VSTVAL, true) => &mut c.stval,
            (csr::VSTVAL, false) => &mut b.stval,
            (_, true) => &mut c.satp,
            (_, false) => &mut b.satp,
        }
    }

    /// Checks a hypervisor load/store (`HLV`, `HLVX`, `HSV`) or fence
    /// (`HFENCE`, `HINVAL`) as QEMU does.
    pub(crate) fn h_insn_check(&self, raw: u32, fence: bool) -> Result<(), Exception> {
        let illegal = Exception::new(cause::ILLEGAL_INSN, u64::from(raw));
        if !self.h_ext {
            return Err(illegal);
        }
        if self.virt {
            return Err(Exception::new(cause::VIRTUAL_INSN, u64::from(raw)));
        }
        match self.prv {
            Priv::Machine | Priv::Supervisor => Ok(()),
            Priv::User if !fence && self.csr.h.hstatus & hstatus::HU != 0 => Ok(()),
            Priv::User => Err(illegal),
        }
    }
}
