//! Instruction decoder: raw encoding → [`Insn`].
//!
//! Decoding is a pure function so decoded instructions can be cached per
//! guest page (decoded-block cache) without changing semantics.

use crate::rvc;

/// Operation, independent of operand registers.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Illegal,
    // RV64I
    Lui,
    Auipc,
    Jal,
    Jalr,
    Beq,
    Bne,
    Blt,
    Bge,
    Bltu,
    Bgeu,
    Lb,
    Lh,
    Lw,
    Ld,
    Lbu,
    Lhu,
    Lwu,
    Sb,
    Sh,
    Sw,
    Sd,
    Addi,
    Slti,
    Sltiu,
    Xori,
    Ori,
    Andi,
    Slli,
    Srli,
    Srai,
    Add,
    Sub,
    Sll,
    Slt,
    Sltu,
    Xor,
    Srl,
    Sra,
    Or,
    And,
    Addiw,
    Slliw,
    Srliw,
    Sraiw,
    Addw,
    Subw,
    Sllw,
    Srlw,
    Sraw,
    Fence,
    FenceI,
    Ecall,
    Ebreak,
    // M
    Mul,
    Mulh,
    Mulhsu,
    Mulhu,
    Div,
    Divu,
    Rem,
    Remu,
    Mulw,
    Divw,
    Divuw,
    Remw,
    Remuw,
    // A (`dbl` selects .D; `funct` holds the AMO operation)
    Lr,
    Sc,
    Amo,
    // Zicsr (`imm` holds the CSR number; immediate forms use `rs1` as zimm)
    Csrrw,
    Csrrs,
    Csrrc,
    Csrrwi,
    Csrrsi,
    Csrrci,
    // Privileged
    Mret,
    Sret,
    Wfi,
    SfenceVma,
    // F/D (`dbl` selects double precision, `funct` holds the rounding mode)
    Fload,
    Fstore,
    Fmadd,
    Fmsub,
    Fnmsub,
    Fnmadd,
    Fadd,
    Fsub,
    Fmul,
    Fdiv,
    Fsqrt,
    Fsgnj,
    Fsgnjn,
    Fsgnjx,
    Fmin,
    Fmax,
    /// Float → integer; `rs2` selects W/WU/L/LU.
    FcvtIntF,
    /// Integer → float; `rs2` selects W/WU/L/LU.
    FcvtFInt,
    /// fcvt.s.d (`dbl` = false) / fcvt.d.s (`dbl` = true).
    FcvtFF,
    FmvXF,
    FmvFX,
    Feq,
    Flt,
    Fle,
    Fclass,
    // Zba (address generation)
    AddUw,
    Sh1add,
    Sh2add,
    Sh3add,
    Sh1addUw,
    Sh2addUw,
    Sh3addUw,
    SlliUw,
    // Zbb (basic bit-manipulation)
    Andn,
    Orn,
    Xnor,
    Clz,
    Ctz,
    Cpop,
    Clzw,
    Ctzw,
    Cpopw,
    SextB,
    SextH,
    ZextH,
    Min,
    Minu,
    Max,
    Maxu,
    Rol,
    Ror,
    Rolw,
    Rorw,
    Rori,
    Roriw,
    OrcB,
    Rev8,
    // Zbc (carry-less multiply)
    Clmul,
    Clmulh,
    Clmulr,
    // Zbs (single-bit operations)
    Bclr,
    Bext,
    Binv,
    Bset,
    Bclri,
    Bexti,
    Binvi,
    Bseti,
}

/// AMO operation codes (instruction bits 31:27).
#[allow(missing_docs)]
pub mod amo {
    pub const ADD: u8 = 0x00;
    pub const SWAP: u8 = 0x01;
    pub const XOR: u8 = 0x04;
    pub const OR: u8 = 0x08;
    pub const AND: u8 = 0x0c;
    pub const MIN: u8 = 0x10;
    pub const MAX: u8 = 0x14;
    pub const MINU: u8 = 0x18;
    pub const MAXU: u8 = 0x1c;
}

/// A decoded instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    /// Operation.
    pub op: Op,
    /// Destination register.
    pub rd: u8,
    /// Source register 1 (or zimm for CSR immediate forms).
    pub rs1: u8,
    /// Source register 2 (or conversion selector).
    pub rs2: u8,
    /// Source register 3 (fused multiply-add).
    pub rs3: u8,
    /// Extra function field: rounding mode (FP) or AMO operation.
    pub funct: u8,
    /// Double-width variant (A: .D, F/D: double precision).
    pub dbl: bool,
    /// Encoded length in bytes (2 or 4).
    pub len: u8,
    /// Sign-extended immediate (or CSR number).
    pub imm: i32,
    /// Raw encoding (16-bit for compressed instructions), used for `mtval`.
    pub raw: u32,
}

impl Insn {
    pub(crate) const ILLEGAL: Self = Self {
        op: Op::Illegal,
        rd: 0,
        rs1: 0,
        rs2: 0,
        rs3: 0,
        funct: 0,
        dbl: false,
        len: 4,
        imm: 0,
        raw: 0,
    };
}

#[inline]
fn imm_i(raw: u32) -> i32 {
    raw as i32 >> 20
}

#[inline]
fn imm_s(raw: u32) -> i32 {
    ((raw as i32 >> 25) << 5) | ((raw >> 7) & 0x1f) as i32
}

#[inline]
fn imm_b(raw: u32) -> i32 {
    ((raw as i32 >> 31) << 12)
        | (((raw >> 7) & 1) << 11) as i32
        | (((raw >> 25) & 0x3f) << 5) as i32
        | (((raw >> 8) & 0xf) << 1) as i32
}

#[inline]
fn imm_u(raw: u32) -> i32 {
    (raw & 0xffff_f000) as i32
}

#[inline]
fn imm_j(raw: u32) -> i32 {
    ((raw as i32 >> 31) << 20)
        | (raw & 0x000f_f000) as i32
        | (((raw >> 20) & 1) << 11) as i32
        | (((raw >> 21) & 0x3ff) << 1) as i32
}

/// Decodes a 16-bit compressed instruction.
pub fn decode_compressed(raw: u16) -> Insn {
    let mut insn = match rvc::expand(raw) {
        Some(full) => decode(full),
        None => Insn::ILLEGAL,
    };
    insn.len = 2;
    insn.raw = u32::from(raw);
    insn
}

/// Decodes a 32-bit instruction.
pub fn decode(raw: u32) -> Insn {
    let rd = ((raw >> 7) & 0x1f) as u8;
    let rs1 = ((raw >> 15) & 0x1f) as u8;
    let rs2 = ((raw >> 20) & 0x1f) as u8;
    let f3 = (raw >> 12) & 7;
    let f7 = raw >> 25;
    let mut insn = Insn {
        rd,
        rs1,
        rs2,
        raw,
        ..Insn::ILLEGAL
    };

    let (op, imm) = match raw & 0x7f {
        0x37 => (Op::Lui, imm_u(raw)),
        0x17 => (Op::Auipc, imm_u(raw)),
        0x6f => (Op::Jal, imm_j(raw)),
        0x67 if f3 == 0 => (Op::Jalr, imm_i(raw)),
        0x63 => {
            let op = match f3 {
                0 => Op::Beq,
                1 => Op::Bne,
                4 => Op::Blt,
                5 => Op::Bge,
                6 => Op::Bltu,
                7 => Op::Bgeu,
                _ => Op::Illegal,
            };
            (op, imm_b(raw))
        }
        0x03 => {
            let op = match f3 {
                0 => Op::Lb,
                1 => Op::Lh,
                2 => Op::Lw,
                3 => Op::Ld,
                4 => Op::Lbu,
                5 => Op::Lhu,
                6 => Op::Lwu,
                _ => Op::Illegal,
            };
            (op, imm_i(raw))
        }
        0x23 => {
            let op = match f3 {
                0 => Op::Sb,
                1 => Op::Sh,
                2 => Op::Sw,
                3 => Op::Sd,
                _ => Op::Illegal,
            };
            (op, imm_s(raw))
        }
        0x13 => {
            let shamt = ((raw >> 20) & 0x3f) as i32;
            let top6 = raw >> 26;
            let imm12 = raw >> 20;
            match f3 {
                0 => (Op::Addi, imm_i(raw)),
                2 => (Op::Slti, imm_i(raw)),
                3 => (Op::Sltiu, imm_i(raw)),
                4 => (Op::Xori, imm_i(raw)),
                6 => (Op::Ori, imm_i(raw)),
                7 => (Op::Andi, imm_i(raw)),
                1 => match top6 {
                    0x00 => (Op::Slli, shamt),
                    0x0a => (Op::Bseti, shamt),
                    0x12 => (Op::Bclri, shamt),
                    0x1a => (Op::Binvi, shamt),
                    0x18 => match rs2 {
                        0 => (Op::Clz, 0),
                        1 => (Op::Ctz, 0),
                        2 => (Op::Cpop, 0),
                        4 => (Op::SextB, 0),
                        5 => (Op::SextH, 0),
                        _ => (Op::Illegal, 0),
                    },
                    _ => (Op::Illegal, 0),
                },
                5 => match top6 {
                    0x00 => (Op::Srli, shamt),
                    0x10 => (Op::Srai, shamt),
                    0x12 => (Op::Bexti, shamt),
                    0x18 => (Op::Rori, shamt),
                    _ if imm12 == 0x287 => (Op::OrcB, 0),
                    _ if imm12 == 0x6b8 => (Op::Rev8, 0),
                    _ => (Op::Illegal, 0),
                },
                _ => (Op::Illegal, 0),
            }
        }
        0x1b => {
            let shamt = ((raw >> 20) & 0x1f) as i32;
            let shamt6 = ((raw >> 20) & 0x3f) as i32;
            match (f3, f7) {
                (0, _) => (Op::Addiw, imm_i(raw)),
                (1, 0) => (Op::Slliw, shamt),
                (1, 0x04 | 0x05) => (Op::SlliUw, shamt6),
                (1, 0x30) => match rs2 {
                    0 => (Op::Clzw, 0),
                    1 => (Op::Ctzw, 0),
                    2 => (Op::Cpopw, 0),
                    _ => (Op::Illegal, 0),
                },
                (5, 0) => (Op::Srliw, shamt),
                (5, 0x20) => (Op::Sraiw, shamt),
                (5, 0x30) => (Op::Roriw, shamt),
                _ => (Op::Illegal, 0),
            }
        }
        0x33 => {
            let op = match (f7, f3) {
                (0, 0) => Op::Add,
                (0x20, 0) => Op::Sub,
                (0, 1) => Op::Sll,
                (0, 2) => Op::Slt,
                (0, 3) => Op::Sltu,
                (0, 4) => Op::Xor,
                (0, 5) => Op::Srl,
                (0x20, 5) => Op::Sra,
                (0, 6) => Op::Or,
                (0, 7) => Op::And,
                (1, 0) => Op::Mul,
                (1, 1) => Op::Mulh,
                (1, 2) => Op::Mulhsu,
                (1, 3) => Op::Mulhu,
                (1, 4) => Op::Div,
                (1, 5) => Op::Divu,
                (1, 6) => Op::Rem,
                (1, 7) => Op::Remu,
                // Zba
                (0x10, 2) => Op::Sh1add,
                (0x10, 4) => Op::Sh2add,
                (0x10, 6) => Op::Sh3add,
                // Zbb / Zbc (f7 == 0x05)
                (0x05, 1) => Op::Clmul,
                (0x05, 2) => Op::Clmulr,
                (0x05, 3) => Op::Clmulh,
                (0x05, 4) => Op::Min,
                (0x05, 5) => Op::Minu,
                (0x05, 6) => Op::Max,
                (0x05, 7) => Op::Maxu,
                // Zbb logical-with-negate & rotates
                (0x20, 4) => Op::Xnor,
                (0x20, 6) => Op::Orn,
                (0x20, 7) => Op::Andn,
                (0x30, 1) => Op::Rol,
                (0x30, 5) => Op::Ror,
                // Zbs
                (0x14, 1) => Op::Bset,
                (0x24, 1) => Op::Bclr,
                (0x24, 5) => Op::Bext,
                (0x34, 1) => Op::Binv,
                _ => Op::Illegal,
            };
            (op, 0)
        }
        0x3b => {
            let op = match (f7, f3) {
                (0, 0) => Op::Addw,
                (0x20, 0) => Op::Subw,
                (0, 1) => Op::Sllw,
                (0, 5) => Op::Srlw,
                (0x20, 5) => Op::Sraw,
                (1, 0) => Op::Mulw,
                (1, 4) => Op::Divw,
                (1, 5) => Op::Divuw,
                (1, 6) => Op::Remw,
                (1, 7) => Op::Remuw,
                // Zba (.uw) & Zbb (zext.h, rolw, rorw)
                (0x04, 0) => Op::AddUw,
                (0x04, 4) if rs2 == 0 => Op::ZextH,
                (0x10, 2) => Op::Sh1addUw,
                (0x10, 4) => Op::Sh2addUw,
                (0x10, 6) => Op::Sh3addUw,
                (0x30, 1) => Op::Rolw,
                (0x30, 5) => Op::Rorw,
                _ => Op::Illegal,
            };
            (op, 0)
        }
        0x0f => match f3 {
            0 => (Op::Fence, 0),
            1 => (Op::FenceI, 0),
            _ => (Op::Illegal, 0),
        },
        0x73 => match f3 {
            0 => {
                let op = match raw {
                    0x0000_0073 => Op::Ecall,
                    0x0010_0073 => Op::Ebreak,
                    0x3020_0073 => Op::Mret,
                    0x1020_0073 => Op::Sret,
                    0x1050_0073 => Op::Wfi,
                    _ if f7 == 0x09 && rd == 0 => Op::SfenceVma,
                    _ => Op::Illegal,
                };
                (op, 0)
            }
            4 => (Op::Illegal, 0),
            _ => {
                let op = match f3 {
                    1 => Op::Csrrw,
                    2 => Op::Csrrs,
                    3 => Op::Csrrc,
                    5 => Op::Csrrwi,
                    6 => Op::Csrrsi,
                    _ => Op::Csrrci,
                };
                (op, (raw >> 20) as i32)
            }
        },
        0x2f if f3 == 2 || f3 == 3 => {
            insn.dbl = f3 == 3;
            let f5 = (raw >> 27) as u8;
            insn.funct = f5;
            let op = match f5 {
                0x02 if rs2 == 0 => Op::Lr,
                0x03 => Op::Sc,
                amo::ADD
                | amo::SWAP
                | amo::XOR
                | amo::OR
                | amo::AND
                | amo::MIN
                | amo::MAX
                | amo::MINU
                | amo::MAXU => Op::Amo,
                _ => Op::Illegal,
            };
            (op, 0)
        }
        0x07 if f3 == 2 || f3 == 3 => {
            insn.dbl = f3 == 3;
            (Op::Fload, imm_i(raw))
        }
        0x27 if f3 == 2 || f3 == 3 => {
            insn.dbl = f3 == 3;
            (Op::Fstore, imm_s(raw))
        }
        opc @ (0x43 | 0x47 | 0x4b | 0x4f) => {
            let fmt = (raw >> 25) & 3;
            insn.dbl = fmt == 1;
            insn.rs3 = (raw >> 27) as u8;
            insn.funct = f3 as u8;
            let op = match (fmt, opc) {
                (2 | 3, _) => Op::Illegal,
                (_, 0x43) => Op::Fmadd,
                (_, 0x47) => Op::Fmsub,
                (_, 0x4b) => Op::Fnmsub,
                _ => Op::Fnmadd,
            };
            (op, 0)
        }
        0x53 => {
            let fmt = f7 & 3;
            insn.dbl = fmt == 1;
            insn.funct = f3 as u8;
            let op = if fmt > 1 {
                Op::Illegal
            } else {
                match (f7 >> 2, f3, rs2) {
                    (0x00, _, _) => Op::Fadd,
                    (0x01, _, _) => Op::Fsub,
                    (0x02, _, _) => Op::Fmul,
                    (0x03, _, _) => Op::Fdiv,
                    (0x0b, _, 0) => Op::Fsqrt,
                    (0x04, 0, _) => Op::Fsgnj,
                    (0x04, 1, _) => Op::Fsgnjn,
                    (0x04, 2, _) => Op::Fsgnjx,
                    (0x05, 0, _) => Op::Fmin,
                    (0x05, 1, _) => Op::Fmax,
                    // fcvt.s.d: fmt=S, rs2=1; fcvt.d.s: fmt=D, rs2=0
                    (0x08, _, 1) if fmt == 0 => Op::FcvtFF,
                    (0x08, _, 0) if fmt == 1 => Op::FcvtFF,
                    (0x14, 2, _) => Op::Feq,
                    (0x14, 1, _) => Op::Flt,
                    (0x14, 0, _) => Op::Fle,
                    (0x18, _, 0..=3) => Op::FcvtIntF,
                    (0x1a, _, 0..=3) => Op::FcvtFInt,
                    (0x1c, 0, 0) => Op::FmvXF,
                    (0x1c, 1, 0) => Op::Fclass,
                    (0x1e, 0, 0) => Op::FmvFX,
                    _ => Op::Illegal,
                }
            };
            (op, 0)
        }
        _ => (Op::Illegal, 0),
    };
    insn.op = op;
    insn.imm = imm;
    if op == Op::Illegal {
        insn.funct = 0;
    }
    insn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immediates() {
        // addi a0, a0, -1
        let i = decode(0xfff5_0513);
        assert_eq!((i.op, i.rd, i.rs1, i.imm), (Op::Addi, 10, 10, -1));
        // sd ra, -8(sp)
        let i = decode(0xfe11_3c23);
        assert_eq!((i.op, i.rs1, i.rs2, i.imm), (Op::Sd, 2, 1, -8));
        // beq x0, x0, -4
        let i = decode(0xfe00_0ee3);
        assert_eq!((i.op, i.imm), (Op::Beq, -4));
        // jal ra, 2048
        let i = decode(0x0010_00ef);
        assert_eq!((i.op, i.rd, i.imm), (Op::Jal, 1, 2048));
        // lui a0, 0x80000
        let i = decode(0x8000_0537);
        assert_eq!((i.op, i.imm), (Op::Lui, -0x8000_0000));
        // srai a0, a0, 63
        let i = decode(0x43f5_5513);
        assert_eq!((i.op, i.imm), (Op::Srai, 63));
    }

    #[test]
    fn system_and_csr() {
        assert_eq!(decode(0x0000_0073).op, Op::Ecall);
        assert_eq!(decode(0x3020_0073).op, Op::Mret);
        assert_eq!(decode(0x1200_0073).op, Op::SfenceVma);
        // csrrs a0, mstatus, zero
        let i = decode(0x3000_2573);
        assert_eq!((i.op, i.rd, i.imm), (Op::Csrrs, 10, 0x300));
    }

    #[test]
    fn illegal() {
        assert_eq!(decode(0).op, Op::Illegal);
        assert_eq!(decode(0xffff_ffff).op, Op::Illegal);
        // slliw with shamt[5] set is reserved
        assert_eq!(decode(0x0205_151b).op, Op::Illegal);
        assert_eq!(decode_compressed(0).op, Op::Illegal);
    }

    #[test]
    fn compressed_keeps_raw_and_len() {
        let i = decode_compressed(0x4505); // c.li a0, 1
        assert_eq!(
            (i.op, i.rd, i.imm, i.len, i.raw),
            (Op::Addi, 10, 1, 2, 0x4505)
        );
    }

    #[test]
    fn bitmanip_zba_zbb_zbc_zbs() {
        use crate::hart::Hart;
        use rivemu_core::HostTimebase;
        use rivemu_mem::{AddressSpace, Ram};
        use std::sync::Arc;

        // Decode checks for Zba, Zbb, Zbc, Zbs.
        assert_eq!(decode(0x20b5_2533).op, Op::Sh1add); // sh1add a0, a0, a1
        assert_eq!(decode(0x20b5_4533).op, Op::Sh2add); // sh2add a0, a0, a1
        assert_eq!(decode(0x20b5_6533).op, Op::Sh3add); // sh3add a0, a0, a1
        assert_eq!(decode(0x08b5_053b).op, Op::AddUw); // add.uw a0, a0, a1
        assert_eq!(decode(0x0845_151b).op, Op::SlliUw); // slli.uw a0, a0, 4
        assert_eq!(decode(0x6005_1513).op, Op::Clz); // clz a0, a0
        assert_eq!(decode(0x6015_1513).op, Op::Ctz); // ctz a0, a0
        assert_eq!(decode(0x6025_1513).op, Op::Cpop); // cpop a0, a0
        assert_eq!(decode(0x6045_1513).op, Op::SextB); // sext.b a0, a0
        assert_eq!(decode(0x6055_1513).op, Op::SextH); // sext.h a0, a0
        assert_eq!(decode(0x0805_453b).op, Op::ZextH); // zext.h a0, a0
        assert_eq!(decode(0x2875_5513).op, Op::OrcB); // orc.b a0, a0
        assert_eq!(decode(0x6b85_5513).op, Op::Rev8); // rev8 a0, a0
        assert_eq!(decode(0x0ab5_1533).op, Op::Clmul); // clmul a0, a0, a1
        assert_eq!(decode(0x0ab5_3533).op, Op::Clmulh); // clmulh a0, a0, a1
        assert_eq!(decode(0x28b5_1533).op, Op::Bset); // bset a0, a0, a1
        assert_eq!(decode(0x48b5_1533).op, Op::Bclr); // bclr a0, a0, a1
        assert_eq!(decode(0x48b5_5533).op, Op::Bext); // bext a0, a0, a1
        assert_eq!(decode(0x68b5_1533).op, Op::Binv); // binv a0, a0, a1

        // Execute a block exercising Zba + Zbb + Zbc + Zbs.
        let mut bus = AddressSpace::new();
        bus.add_ram("ram", 0x8000_0000, Arc::new(Ram::new(4096).unwrap()))
            .unwrap();
        let prog: &[u32] = &[
            0x20b5_2633, // sh1add a2, a0, a1      (a2 = a1 + (a0 << 1))
            0x6015_1693, // ctz    a3, a0          (a3 = trailing_zeros(a0))
            0x6b85_5713, // rev8   a4, a0          (a4 = swap_bytes(a0))
            0x2875_5793, // orc.b  a5, a0          (a5 = orc_b(a0))
            0x28b5_1833, // bset   a6, a0, a1      (a6 = a0 | (1 << a1))
        ];
        for (idx, w) in prog.iter().enumerate() {
            bus.load_bytes(0x8000_0000 + idx as u64 * 4, &w.to_le_bytes())
                .unwrap();
        }
        let mut h = Hart::new(0, Arc::new(HostTimebase::new(10_000_000)));
        h.reset(0x8000_0000);
        h.x[10] = 0x0100_0000_0000_0008; // a0
        h.x[11] = 5; // a1
        for _ in 0..prog.len() {
            h.step(&bus);
        }
        assert_eq!(h.x[12], 5 + (0x0100_0000_0000_0008 << 1)); // sh1add
        assert_eq!(h.x[13], 3); // ctz(8) == 3
        assert_eq!(h.x[14], 0x0800_0000_0000_0001); // rev8
        assert_eq!(h.x[15], 0xff00_0000_0000_00ff); // orc.b
        assert_eq!(h.x[16], 0x0100_0000_0000_0028); // bset bit 5
    }
}
