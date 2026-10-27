//! CSR numbers, bit definitions and the CSR register file.

#![allow(missing_docs)]

// Unprivileged.
pub const FFLAGS: u16 = 0x001;
pub const FRM: u16 = 0x002;
pub const FCSR: u16 = 0x003;
pub const CYCLE: u16 = 0xc00;
pub const TIME: u16 = 0xc01;
pub const INSTRET: u16 = 0xc02;
pub const HPMCOUNTER3: u16 = 0xc03;
pub const HPMCOUNTER31: u16 = 0xc1f;

// Supervisor.
pub const SSTATUS: u16 = 0x100;
pub const SIE: u16 = 0x104;
pub const STVEC: u16 = 0x105;
pub const SCOUNTEREN: u16 = 0x106;
pub const SENVCFG: u16 = 0x10a;
pub const SSCRATCH: u16 = 0x140;
pub const SEPC: u16 = 0x141;
pub const SCAUSE: u16 = 0x142;
pub const STVAL: u16 = 0x143;
pub const SIP: u16 = 0x144;
pub const STIMECMP: u16 = 0x14d;
pub const SATP: u16 = 0x180;

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
pub const MCOUNTINHIBIT: u16 = 0x320;
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
pub const TDATA3: u16 = 0x7a3;
pub const TINFO: u16 = 0x7a4;
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
    pub const UXL: u64 = 3 << 32;
    pub const SXL: u64 = 3 << 34;
    pub const SD: u64 = 1 << 63;

    /// Bits writable through `mstatus`.
    pub const M_WRITABLE: u64 =
        SIE | MIE | SPIE | MPIE | SPP | MPP | FS | MPRV | SUM | MXR | TVM | TW | TSR;
    /// Bits visible through `sstatus`.
    pub const S_READABLE: u64 = SIE | SPIE | SPP | FS | SUM | MXR | UXL | SD;
    /// Bits writable through `sstatus`.
    pub const S_WRITABLE: u64 = SIE | SPIE | SPP | FS | SUM | MXR;
}

/// Interrupt bits in `mip` / `mie` (and interrupt cause numbers).
pub mod irq {
    pub const SSI: u64 = 1;
    pub const MSI: u64 = 3;
    pub const STI: u64 = 5;
    pub const MTI: u64 = 7;
    pub const SEI: u64 = 9;
    pub const MEI: u64 = 11;

    pub const SSIP: u64 = 1 << SSI;
    pub const MSIP: u64 = 1 << MSI;
    pub const STIP: u64 = 1 << STI;
    pub const MTIP: u64 = 1 << MTI;
    pub const SEIP: u64 = 1 << SEI;
    pub const MEIP: u64 = 1 << MEI;

    /// Interrupts that can be delegated to S-mode.
    pub const S_MASK: u64 = SSIP | STIP | SEIP;
    /// All implemented interrupts.
    pub const ALL: u64 = S_MASK | MSIP | MTIP | MEIP;
    /// Priority order for taking interrupts (highest first).
    pub const PRIORITY: [u64; 6] = [MEI, MSI, MTI, SEI, SSI, STI];
}

/// `menvcfg` bits.
pub mod envcfg {
    pub const FIOM: u64 = 1;
    pub const STCE: u64 = 1 << 63;
}

/// `misa` value: RV64 IMAFDC + S + U.
pub const MISA_VALUE: u64 = (2 << 62)
    | (1 << 0)  // A
    | (1 << 2)  // C
    | (1 << 3)  // D
    | (1 << 5)  // F
    | (1 << 8)  // I
    | (1 << 12) // M
    | (1 << 18) // S
    | (1 << 20); // U

/// Exceptions that may be delegated to S-mode (all but ecall-from-M and reserved).
pub const MEDELEG_MASK: u64 = 0xb3ff;

/// Number of implemented PMP entries.
pub const PMP_ENTRIES: usize = 16;

/// The CSR state of a hart that is not derived from other state.
#[derive(Clone, Debug)]
pub struct Csrs {
    pub mstatus: u64,
    pub medeleg: u64,
    pub mideleg: u64,
    pub mie: u64,
    /// Software-writable pending bits (SSIP, STIP without Sstc, SEIP).
    pub mip: u64,
    pub mtvec: u64,
    pub mcounteren: u64,
    pub menvcfg: u64,
    pub mcountinhibit: u64,
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
    pub fflags: u64,
    pub frm: u64,
    pub pmpcfg: [u8; PMP_ENTRIES],
    pub pmpaddr: [u64; PMP_ENTRIES],
    pub minstret: u64,
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
            menvcfg: 0,
            mcountinhibit: 0,
            mscratch: 0,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            stvec: 0,
            scounteren: 0,
            senvcfg: 0,
            sscratch: 0,
            sepc: 0,
            scause: 0,
            stval: 0,
            stimecmp: u64::MAX,
            satp: 0,
            fflags: 0,
            frm: 0,
            pmpcfg: [0; PMP_ENTRIES],
            pmpaddr: [0; PMP_ENTRIES],
            minstret: 0,
        }
    }
}
