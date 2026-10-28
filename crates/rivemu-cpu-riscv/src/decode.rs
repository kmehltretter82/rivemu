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
            match f3 {
                0 => (Op::Addi, imm_i(raw)),
                2 => (Op::Slti, imm_i(raw)),
                3 => (Op::Sltiu, imm_i(raw)),
                4 => (Op::Xori, imm_i(raw)),
                6 => (Op::Ori, imm_i(raw)),
                7 => (Op::Andi, imm_i(raw)),
                1 if raw >> 26 == 0 => (Op::Slli, shamt),
                5 if raw >> 26 == 0 => (Op::Srli, shamt),
                5 if raw >> 26 == 0x10 => (Op::Srai, shamt),
                _ => (Op::Illegal, 0),
            }
        }
        0x1b => {
            let shamt = ((raw >> 20) & 0x1f) as i32;
            match (f3, f7) {
                (0, _) => (Op::Addiw, imm_i(raw)),
                (1, 0) => (Op::Slliw, shamt),
                (5, 0) => (Op::Srliw, shamt),
                (5, 0x20) => (Op::Sraiw, shamt),
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
}
