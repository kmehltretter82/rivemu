//! CSR numbers, bit definitions and the CSR register file.

#![allow(missing_docs)]

// Unprivileged.
pub const FFLAGS: u16 = 0x001;
pub const FRM: u16 = 0x002;
pub const FCSR: u16 = 0x003;
pub const VSTART: u16 = 0x008;
pub const VXSAT: u16 = 0x009;
pub const VXRM: u16 = 0x00a;
pub const VCSR: u16 = 0x00f;
pub const SSP: u16 = 0x011;
pub const SEED: u16 = 0x015;
pub const CYCLE: u16 = 0xc00;
pub const TIME: u16 = 0xc01;
pub const INSTRET: u16 = 0xc02;
pub const HPMCOUNTER3: u16 = 0xc03;
pub const HPMCOUNTER31: u16 = 0xc1f;
pub const VL: u16 = 0xc20;
pub const VTYPE: u16 = 0xc21;
pub const VLENB: u16 = 0xc22;

/// Vector register width in bits (`VLEN = 256`).
pub const VLEN: usize = 256;
/// Vector register width in bytes (`vlenb = VLEN / 8 = 32`).
pub const VLENB_VAL: u64 = (VLEN / 8) as u64;

// Supervisor.
pub const SSTATUS: u16 = 0x100;
pub const SIE: u16 = 0x104;
pub const STVEC: u16 = 0x105;
pub const SCOUNTEREN: u16 = 0x106;
pub const SENVCFG: u16 = 0x10a;
pub const SSTATEEN0: u16 = 0x10c;
pub const SSTATEEN3: u16 = 0x10f;
pub const SSCRATCH: u16 = 0x140;
pub const SEPC: u16 = 0x141;
pub const SCAUSE: u16 = 0x142;
pub const STVAL: u16 = 0x143;
pub const SIP: u16 = 0x144;
pub const STIMECMP: u16 = 0x14d;
pub const SATP: u16 = 0x180;
pub const SRMCFG: u16 = 0x181;
pub const SCONTEXT: u16 = 0x5a8;
pub const SCOUNTOVF: u16 = 0xda0;

/// Mask of writable bits in `srmcfg` (`Ssqosid`: 12-bit `RCID` in `[11:0]`, 12-bit `MCID` in `[27:16]`).
pub const SRMCFG_MASK: u64 = 0x0fff_0fff;

// Machine.
pub const MVENDORID: u16 = 0xf11;
pub const MARCHID: u16 = 0xf12;
pub const MIMPID: u16 = 0xf13;
pub const MHARTID: u16 = 0xf14;
pub const MCONFIGPTR: u16 = 0xf15;
pub const MSTATUS: u16 = 0x300;
pub const MISA: u16 = 0x301;
pub const MEDELEG: u16 = 0x302;
pub const MIDELEG: u16 = 0x303;
pub const MIE: u16 = 0x304;
pub const MTVEC: u16 = 0x305;
pub const MCOUNTEREN: u16 = 0x306;
pub const MENVCFG: u16 = 0x30a;
pub const MSTATEEN0: u16 = 0x30c;
pub const MSTATEEN3: u16 = 0x30f;
pub const MCOUNTINHIBIT: u16 = 0x320;
pub const MCYCLECFG: u16 = 0x321;
pub const MINSTRETCFG: u16 = 0x322;
pub const MHPMEVENT3: u16 = 0x323;
pub const MHPMEVENT31: u16 = 0x33f;
pub const MSCRATCH: u16 = 0x340;
pub const MEPC: u16 = 0x341;
pub const MCAUSE: u16 = 0x342;
pub const MTVAL: u16 = 0x343;
pub const MIP: u16 = 0x344;
pub const PMPCFG0: u16 = 0x3a0;
pub const PMPCFG15: u16 = 0x3af;
pub const PMPADDR0: u16 = 0x3b0;
pub const PMPADDR63: u16 = 0x3ef;
pub const MSECCFG: u16 = 0x747;
pub const TSELECT: u16 = 0x7a0;
pub const TDATA1: u16 = 0x7a1;
pub const TDATA2: u16 = 0x7a2;
pub const TDATA3: u16 = 0x7a3;
pub const TINFO: u16 = 0x7a4;
pub const TCONTROL: u16 = 0x7a5;
pub const MCONTEXT: u16 = 0x7a8;
pub const MCYCLE: u16 = 0xb00;
pub const MINSTRET: u16 = 0xb02;
pub const MHPMCOUNTER3: u16 = 0xb03;
pub const MHPMCOUNTER31: u16 = 0xb1f;

/// `mstatus` / `sstatus` bits.
pub mod status {
    pub const SIE: u64 = 1 << 1;
    pub const MIE: u64 = 1 << 3;
    pub const SPIE: u64 = 1 << 5;
    pub const MPIE: u64 = 1 << 7;
    pub const SPP: u64 = 1 << 8;
    pub const VS_SHIFT: u32 = 9;
    pub const VS: u64 = 3 << VS_SHIFT;
    pub const MPP_SHIFT: u32 = 11;
    pub const MPP: u64 = 3 << MPP_SHIFT;
    pub const FS_SHIFT: u32 = 13;
    pub const FS: u64 = 3 << FS_SHIFT;
    pub const MPRV: u64 = 1 << 17;
    pub const SUM: u64 = 1 << 18;
    pub const MXR: u64 = 1 << 19;
    pub const TVM: u64 = 1 << 20;
    pub const TW: u64 = 1 << 21;
    pub const TSR: u64 = 1 << 22;
    pub const SPELP: u64 = 1 << 23;
    pub const SDT: u64 = 1 << 24;
    pub const UXL: u64 = 3 << 32;
    pub const SXL: u64 = 3 << 34;
    pub const MPELP: u64 = 1 << 41;
    pub const MDT: u64 = 1 << 42;
    pub const SD: u64 = 1 << 63;

    /// Bits writable through `mstatus`.
    pub const M_WRITABLE: u64 = SIE
        | MIE
        | SPIE
        | MPIE
        | SPP
        | VS
        | MPP
        | FS
        | MPRV
        | SUM
        | MXR
        | TVM
        | TW
        | TSR
        | SPELP
        | SDT
        | MPELP
        | MDT;
    /// Bits visible through `sstatus`.
    pub const S_READABLE: u64 = SIE | SPIE | SPP | VS | FS | SUM | MXR | SPELP | SDT | UXL | SD;
    /// Bits writable through `sstatus`.
    pub const S_WRITABLE: u64 = SIE | SPIE | SPP | VS | FS | SUM | MXR | SPELP | SDT;
}

/// Interrupt bits in `mip` / `mie` (and interrupt cause numbers).
pub mod irq {
    pub const SSI: u64 = 1;
    pub const MSI: u64 = 3;
    pub const STI: u64 = 5;
    pub const MTI: u64 = 7;
    pub const SEI: u64 = 9;
    pub const MEI: u64 = 11;
    pub const LCOFI: u64 = 13;

    pub const SSIP: u64 = 1 << SSI;
    pub const MSIP: u64 = 1 << MSI;
    pub const STIP: u64 = 1 << STI;
    pub const MTIP: u64 = 1 << MTI;
    pub const SEIP: u64 = 1 << SEI;
    pub const MEIP: u64 = 1 << MEI;
    pub const LCOFIP: u64 = 1 << LCOFI;

    /// Interrupts that can be delegated to S-mode.
    pub const S_MASK: u64 = SSIP | STIP | SEIP | LCOFIP;
    /// All implemented interrupts.
    pub const ALL: u64 = S_MASK | MSIP | MTIP | MEIP;
    /// Priority order for taking interrupts (highest first).
    pub const PRIORITY: [u64; 7] = [MEI, MSI, MTI, SEI, SSI, STI, LCOFI];
}

/// `mhpmevent3..31`, `mcyclecfg`, and `minstretcfg` bits (`Sscofpmf` / `Smcntrpmf`).
pub mod hpmevent {
    pub const OF: u64 = 1 << 63;
    pub const MINH: u64 = 1 << 62;
    pub const SINH: u64 = 1 << 61;
    pub const UINH: u64 = 1 << 60;
    pub const FILTER_MASK: u64 = MINH | SINH | UINH;
    pub const EVENT_MASK: u64 = 0x000f_ffff;
    pub const EVENT_CYCLES: u64 = 1;
    pub const EVENT_INSTRUCTIONS: u64 = 2;
    pub const MHPMEVENT_MASK: u64 = OF | FILTER_MASK | EVENT_MASK;
}

/// `menvcfg` / `senvcfg` bits.
pub mod envcfg {
    pub const FIOM: u64 = 1;
    pub const LPE: u64 = 1 << 2;
    pub const SSE: u64 = 1 << 3;
    pub const CBIE_SHIFT: u32 = 4;
    pub const CBIE_MASK: u64 = 3 << CBIE_SHIFT;
    pub const CBCFE: u64 = 1 << 6;
    pub const CBZE: u64 = 1 << 7;
    pub const UKTE: u64 = 1 << 8;
    pub const PMM_SHIFT: u32 = 32;
    pub const PMM_MASK: u64 = 3 << PMM_SHIFT;
    pub const PMM_PMLEN_7: u64 = 2 << PMM_SHIFT;
    pub const PMM_PMLEN_16: u64 = 3 << PMM_SHIFT;
    pub const DTE: u64 = 1 << 59;
    pub const ADUE: u64 = 1 << 61;
    pub const PBMTE: u64 = 1 << 62;
    pub const STCE: u64 = 1 << 63;

    pub const S_MASK: u64 = FIOM | LPE | SSE | CBIE_MASK | CBCFE | CBZE | UKTE | PMM_MASK;
    pub const M_MASK: u64 = (S_MASK & !UKTE) | DTE | ADUE | PBMTE | STCE;

    /// Masks `v` to `mask`, clears the reserved `CBIE = 0b10` encoding (WARL),
    /// and preserves the previous `PMM` field if `v` specifies reserved `PMM = 0b01`.
    #[inline]
    pub const fn sanitize(v: u64, old: u64, mask: u64) -> u64 {
        let mut out = v & mask;
        if (out & CBIE_MASK) == (2 << CBIE_SHIFT) {
            out &= !CBIE_MASK;
        }
        if (out & PMM_MASK) == (1 << PMM_SHIFT) {
            out = (out & !PMM_MASK) | (old & PMM_MASK);
        }
        out
    }
}

/// `mseccfg` bits (`Smepmp`, `Zkr`, `Zicfilp`, `Smnpm`).
pub mod mseccfg {
    pub const MML: u64 = 1 << 0;
    pub const MMWP: u64 = 1 << 1;
    pub const RLB: u64 = 1 << 2;
    pub const USEED: u64 = 1 << 8;
    pub const SSEED: u64 = 1 << 9;
    pub const MLPE: u64 = 1 << 10;
    pub const PMM_SHIFT: u32 = 32;
    pub const PMM_MASK: u64 = 3 << PMM_SHIFT;
}

/// `mstateen0..3` / `sstateen0..3` bits (`Smstateen` / `Ssstateen`).
pub mod stateen {
    pub const SRMCFG: u64 = 1 << 55;
    pub const CONTEXT: u64 = 1 << 57;
    pub const ENVCFG: u64 = 1 << 62;
    pub const SE: u64 = 1 << 63;
    pub const M0_MASK: u64 = SE | ENVCFG | CONTEXT | SRMCFG;
    pub const M1_3_MASK: u64 = SE;
}

/// `misa` value: RV64 IMAFDCV + S + U.
pub const MISA_VALUE: u64 = (2 << 62)
    | (1 << 0)  // A
    | (1 << 2)  // C
    | (1 << 3)  // D
    | (1 << 5)  // F
    | (1 << 8)  // I
    | (1 << 12) // M
    | (1 << 18) // S
    | (1 << 20) // U
    | (1 << 21); // V

/// Exceptions that may be delegated to S-mode (all but ecall-from-M and reserved, plus `SW_CHECK` = 18).
pub const MEDELEG_MASK: u64 = 0x4b3ff;

/// Number of implemented PMP entries.
pub const PMP_ENTRIES: usize = 16;

/// Number of implemented `Sdtrig` hardware debug triggers (`tselect = 0..3`).
pub const TRIGGER_COUNT: usize = 4;

/// `Sdtrig` Debug Trigger Module (`tdata1`, `tdata3`, `tinfo`) bit definitions.
pub mod trig {
    pub const TYPE_SHIFT: u32 = 60;
    pub const TYPE_MCONTROL: u64 = 2;
    pub const TYPE_ICOUNT: u64 = 3;
    pub const TYPE_MCONTROL6: u64 = 6;
    pub const TYPE_DISABLED: u64 = 15;

    pub const LOAD: u64 = 1 << 0;
    pub const STORE: u64 = 1 << 1;
    pub const EXECUTE: u64 = 1 << 2;
    pub const U: u64 = 1 << 3;
    pub const S: u64 = 1 << 4;
    pub const M: u64 = 1 << 6;
    pub const MATCH_SHIFT: u32 = 7;
    pub const MATCH_MASK: u64 = 0xf << 7;
    pub const CHAIN: u64 = 1 << 11;
    pub const ACTION_SHIFT: u32 = 12;
    pub const ACTION_MASK: u64 = 0xf << 12;
    pub const SIZE_SHIFT: u32 = 16;
    pub const SIZE_MASK: u64 = 0xf << 16;
    pub const HIT0: u64 = 1 << 22;
    pub const DMODE: u64 = 1 << 59;

    pub const ICOUNT_ACTION_MASK: u64 = 0x3f;
    pub const ICOUNT_U: u64 = 1 << 6;
    pub const ICOUNT_S: u64 = 1 << 7;
    pub const ICOUNT_PENDING: u64 = 1 << 8;
    pub const ICOUNT_M: u64 = 1 << 9;
    pub const ICOUNT_COUNT_SHIFT: u32 = 10;
    pub const ICOUNT_COUNT_MASK: u64 = 0x3fff << 10;
    pub const ICOUNT_HIT: u64 = 1 << 24;

    /// `tinfo`: version = 1 (`[31:24]`), supported types = 2 (`mcontrol`), 3 (`icount`), 6 (`mcontrol6`).
    pub const TINFO_VAL: u64 = 0x0100_004c;
}

/// Number of programmable HPM counters (`mhpmcounter3..31`).
pub const HPM_COUNTERS: usize = 29;

/// The CSR state of a hart that is not derived from other state.
#[derive(Clone, Debug)]
pub struct Csrs {
    pub mstatus: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    pub mie: u64,
    /// Software-writable pending bits (SSIP, STIP without Sstc, SEIP, LCOFIP).
    pub mip: u64,
    pub mtvec: u64,
    pub mcounteren: u64,
    pub menvcfg: u64,
    pub mcountinhibit: u64,
    pub mcyclecfg: u64,
    pub minstretcfg: u64,
    pub mhpmevent: [u64; HPM_COUNTERS],
    pub mhpmcounter: [u64; HPM_COUNTERS],
    pub active_hpm_mask: u32,
    pub mscratch: u64,
    pub mepc: u64,
    pub mcause: u64,
    pub mtval: u64,
    pub stvec: u64,
    pub scounteren: u64,
    pub senvcfg: u64,
    pub sscratch: u64,
    pub sepc: u64,
    pub scause: u64,
    pub stval: u64,
    pub stimecmp: u64,
    pub satp: u64,
    pub srmcfg: u64,
    pub fflags: u64,
    pub frm: u64,
    pub vstart: u64,
    pub vxsat: u64,
    pub vxrm: u64,
    pub vl: u64,
    pub vtype: u64,
    pub ssp: u64,
    pub mseccfg: u64,
    pub mstateen: [u64; 4],
    pub sstateen: [u64; 4],
    pub pmpcfg: [u8; PMP_ENTRIES],
    pub pmpaddr: [u64; PMP_ENTRIES],
    pub tselect: u64,
    pub tdata1: [u64; TRIGGER_COUNT],
    pub tdata2: [u64; TRIGGER_COUNT],
    pub tdata3: [u64; TRIGGER_COUNT],
    pub scontext: u64,
    pub mcontext: u64,
    pub active_trigger_mask: u8,
    pub minstret: u64,
}

impl Csrs {
    /// Returns whether `Zicfiss` shadow-stack operations are enabled for `prv`.
    #[inline]
    pub fn shadow_stack_enabled(&self, prv: crate::hart::Priv) -> bool {
        match prv {
            crate::hart::Priv::Machine => false,
            crate::hart::Priv::Supervisor => (self.menvcfg & envcfg::SSE) != 0,
            crate::hart::Priv::User => (self.menvcfg & self.senvcfg & envcfg::SSE) != 0,
        }
    }

    /// Returns whether `Zicfilp` forward-edge landing-pad enforcement is enabled for `prv`.
    #[inline]
    pub fn landing_pad_enabled(&self, prv: crate::hart::Priv) -> bool {
        match prv {
            crate::hart::Priv::Machine => (self.mseccfg & mseccfg::MLPE) != 0,
            crate::hart::Priv::Supervisor => (self.menvcfg & envcfg::LPE) != 0,
            crate::hart::Priv::User => (self.senvcfg & envcfg::LPE) != 0,
        }
    }

    /// Returns the effective `Ssnpm`/`Smnpm` pointer-masking width (`0`, `7`, or `16`) for `prv`.
    #[inline]
    pub fn effective_pmlen(&self, prv: crate::hart::Priv) -> u32 {
        if prv != crate::hart::Priv::Machine && (self.mstatus & status::MXR) != 0 {
            return 0;
        }
        let pmm = match prv {
            crate::hart::Priv::Machine => (self.mseccfg & mseccfg::PMM_MASK) >> mseccfg::PMM_SHIFT,
            crate::hart::Priv::Supervisor => (self.menvcfg & envcfg::PMM_MASK) >> envcfg::PMM_SHIFT,
            crate::hart::Priv::User => (self.senvcfg & envcfg::PMM_MASK) >> envcfg::PMM_SHIFT,
        };
        match pmm {
            2 => 7,
            3 => 16,
            _ => 0,
        }
    }

    /// Recomputes [`Self::active_hpm_mask`] from `mcountinhibit` and `mhpmevent`.
    pub fn update_active_hpm_mask(&mut self) {
        let mut mask = 0u32;
        for i in 0..HPM_COUNTERS {
            let bit = 1u32 << (i + 3);
            let ev = self.mhpmevent[i] & hpmevent::EVENT_MASK;
            if (self.mcountinhibit as u32) & bit == 0
                && matches!(ev, hpmevent::EVENT_CYCLES | hpmevent::EVENT_INSTRUCTIONS)
            {
                mask |= bit;
            }
        }
        self.active_hpm_mask = mask;
    }

    /// Recomputes [`Self::active_trigger_mask`] from `tdata1`.
    pub fn update_active_trigger_mask(&mut self) {
        let mut mask = 0u8;
        for i in 0..TRIGGER_COUNT {
            let t1 = self.tdata1[i];
            let ty = t1 >> trig::TYPE_SHIFT;
            let active = match ty {
                trig::TYPE_MCONTROL | trig::TYPE_MCONTROL6 => {
                    (t1 & (trig::M | trig::S | trig::U)) != 0
                        && (t1 & (trig::LOAD | trig::STORE | trig::EXECUTE)) != 0
                }
                trig::TYPE_ICOUNT => {
                    (t1 & (trig::ICOUNT_M | trig::ICOUNT_S | trig::ICOUNT_U)) != 0
                        && (t1 & (trig::ICOUNT_COUNT_MASK | trig::ICOUNT_PENDING)) != 0
                }
                _ => false,
            };
            if active {
                mask |= 1 << i;
            }
        }
        self.active_trigger_mask = mask;
    }
}

impl Default for Csrs {
    fn default() -> Self {
        Self {
            // UXL = SXL = 64-bit.
            mstatus: (2 << 32) | (2 << 34),
            medeleg: 0,
            mideleg: 0,
            mie: 0,
            mip: 0,
            mtvec: 0,
            mcounteren: 0,
            // ADUE = 1: hardware A/D updating (Svadu) is enabled at reset,
            // matching what the MMU does unless Svade is selected.
            menvcfg: envcfg::CBIE_MASK
                | envcfg::CBCFE
                | envcfg::CBZE
                | envcfg::PBMTE
                | envcfg::ADUE,
            mcountinhibit: 0,
            mcyclecfg: 0,
            minstretcfg: 0,
            mhpmevent: [0; HPM_COUNTERS],
            mhpmcounter: [0; HPM_COUNTERS],
            active_hpm_mask: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            stvec: 0,
            scounteren: 0,
            senvcfg: envcfg::CBIE_MASK | envcfg::CBCFE | envcfg::CBZE,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            stimecmp: u64::MAX,
            satp: 0,
            srmcfg: 0,
            fflags: 0,
            frm: 0,
            vstart: 0,
            vxsat: 0,
            vxrm: 0,
            vl: 0,
            vtype: 1 << 63,
            ssp: 0,
            mseccfg: 0,
            mstateen: [
                stateen::M0_MASK,
                stateen::M1_3_MASK,
                stateen::M1_3_MASK,
                stateen::M1_3_MASK,
            ],
            sstateen: [0; 4],
            pmpcfg: [0; PMP_ENTRIES],
            pmpaddr: [0; PMP_ENTRIES],
            tselect: 0,
            tdata1: [trig::TYPE_MCONTROL << trig::TYPE_SHIFT; TRIGGER_COUNT],
            tdata2: [0; TRIGGER_COUNT],
            tdata3: [0; TRIGGER_COUNT],
            scontext: 0,
            mcontext: 0,
            active_trigger_mask: 0,
            minstret: 0,
        }
    }
}
