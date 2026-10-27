//! Expansion of 16-bit compressed (RVC) instructions into their 32-bit
//! equivalents, so the rest of the pipeline only deals with one encoding.

/// Encodes an R-type instruction.
const fn r(f7: u32, rs2: u32, rs1: u32, f3: u32, rd: u32, opc: u32) -> u32 {
    (f7 << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opc
}

/// Encodes an I-type instruction.
const fn i(imm: i32, rs1: u32, f3: u32, rd: u32, opc: u32) -> u32 {
    ((imm as u32 & 0xfff) << 20) | (rs1 << 15) | (f3 << 12) | (rd << 7) | opc
}

/// Encodes an S-type instruction.
const fn s(imm: i32, rs2: u32, rs1: u32, f3: u32, opc: u32) -> u32 {
    let imm = imm as u32;
    (((imm >> 5) & 0x7f) << 25) | (rs2 << 20) | (rs1 << 15) | (f3 << 12) | ((imm & 0x1f) << 7) | opc
}

/// Encodes a B-type instruction.
const fn b(imm: i32, rs2: u32, rs1: u32, f3: u32) -> u32 {
    let imm = imm as u32;
    (((imm >> 12) & 1) << 31)
        | (((imm >> 5) & 0x3f) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (f3 << 12)
        | (((imm >> 1) & 0xf) << 8)
        | (((imm >> 11) & 1) << 7)
        | 0x63
}

/// Encodes a J-type instruction.
const fn j(imm: i32, rd: u32) -> u32 {
    let imm = imm as u32;
    (((imm >> 20) & 1) << 31)
        | (((imm >> 1) & 0x3ff) << 21)
        | (((imm >> 11) & 1) << 20)
        | (((imm >> 12) & 0xff) << 12)
        | (rd << 7)
        | 0x6f
}

#[inline]
fn bit(x: u32, n: u32) -> u32 {
    (x >> n) & 1
}

#[inline]
fn bits(x: u32, hi: u32, lo: u32) -> u32 {
    (x >> lo) & ((1 << (hi - lo + 1)) - 1)
}

/// Sign-extends the low `width` bits of `v`.
#[inline]
fn sext(v: u32, width: u32) -> i32 {
    ((v << (32 - width)) as i32) >> (32 - width)
}

const OP_LOAD: u32 = 0x03;
const OP_LOAD_FP: u32 = 0x07;
const OP_IMM: u32 = 0x13;
const OP_IMM32: u32 = 0x1b;
const OP_STORE: u32 = 0x23;
const OP_STORE_FP: u32 = 0x27;
const OP: u32 = 0x33;
const OP_LUI: u32 = 0x37;
const OP32: u32 = 0x3b;
const OP_JALR: u32 = 0x67;

/// Expands a compressed instruction. Returns `None` for illegal/reserved encodings.
pub(crate) fn expand(c: u16) -> Option<u32> {
    let c = u32::from(c);
    let rd = bits(c, 11, 7);
    let rs2 = bits(c, 6, 2);
    let rdp = bits(c, 4, 2) + 8; // rd' / rs2'
    let rs1p = bits(c, 9, 7) + 8; // rs1' / rd'
    let f3 = bits(c, 15, 13);
    let imm6 = sext((bit(c, 12) << 5) | bits(c, 6, 2), 6);

    let insn = match (c & 3, f3) {
        // Quadrant 0.
        (0, 0) => {
            // C.ADDI4SPN
            let imm = (bits(c, 12, 11) << 4)
                | (bits(c, 10, 7) << 6)
                | (bit(c, 6) << 2)
                | (bit(c, 5) << 3);
            if imm == 0 {
                return None; // includes the all-zero illegal instruction
            }
            i(imm as i32, 2, 0, rdp, OP_IMM)
        }
        (0, 1) | (0, 3) => {
            // C.FLD / C.LD
            let imm = (bits(c, 12, 10) << 3) | (bits(c, 6, 5) << 6);
            let opc = if f3 == 1 { OP_LOAD_FP } else { OP_LOAD };
            i(imm as i32, rs1p, 3, rdp, opc)
        }
        (0, 2) => {
            // C.LW
            let imm = (bits(c, 12, 10) << 3) | (bit(c, 6) << 2) | (bit(c, 5) << 6);
            i(imm as i32, rs1p, 2, rdp, OP_LOAD)
        }
        (0, 5) | (0, 7) => {
            // C.FSD / C.SD
            let imm = (bits(c, 12, 10) << 3) | (bits(c, 6, 5) << 6);
            let opc = if f3 == 5 { OP_STORE_FP } else { OP_STORE };
            s(imm as i32, rdp, rs1p, 3, opc)
        }
        (0, 6) => {
            // C.SW
            let imm = (bits(c, 12, 10) << 3) | (bit(c, 6) << 2) | (bit(c, 5) << 6);
            s(imm as i32, rdp, rs1p, 2, OP_STORE)
        }

        // Quadrant 1.
        (1, 0) => i(imm6, rd, 0, rd, OP_IMM), // C.ADDI (rd=0: hint/nop)
        (1, 1) => {
            // C.ADDIW
            if rd == 0 {
                return None;
            }
            i(imm6, rd, 0, rd, OP_IMM32)
        }
        (1, 2) => i(imm6, 0, 0, rd, OP_IMM), // C.LI
        (1, 3) if rd == 2 => {
            // C.ADDI16SP
            let imm = sext(
                (bit(c, 12) << 9)
                    | (bit(c, 6) << 4)
                    | (bit(c, 5) << 6)
                    | (bits(c, 4, 3) << 7)
                    | (bit(c, 2) << 5),
                10,
            );
            if imm == 0 {
                return None;
            }
            i(imm, 2, 0, 2, OP_IMM)
        }
        (1, 3) => {
            // C.LUI
            if imm6 == 0 {
                return None;
            }
            ((imm6 as u32) << 12) | (rd << 7) | OP_LUI
        }
        (1, 4) => match bits(c, 11, 10) {
            0 | 1 => {
                // C.SRLI / C.SRAI
                let shamt = (bit(c, 12) << 5) | bits(c, 6, 2);
                let f6 = if bits(c, 11, 10) == 0 { 0 } else { 0x10 };
                i(((f6 << 6) | shamt) as i32, rs1p, 5, rs1p, OP_IMM)
            }
            2 => i(imm6, rs1p, 7, rs1p, OP_IMM), // C.ANDI
            _ => match (bit(c, 12), bits(c, 6, 5)) {
                (0, 0) => r(0x20, rdp, rs1p, 0, rs1p, OP),   // C.SUB
                (0, 1) => r(0, rdp, rs1p, 4, rs1p, OP),      // C.XOR
                (0, 2) => r(0, rdp, rs1p, 6, rs1p, OP),      // C.OR
                (0, 3) => r(0, rdp, rs1p, 7, rs1p, OP),      // C.AND
                (1, 0) => r(0x20, rdp, rs1p, 0, rs1p, OP32), // C.SUBW
                (1, 1) => r(0, rdp, rs1p, 0, rs1p, OP32),    // C.ADDW
                _ => return None,
            },
        },
        (1, 5) => {
            // C.J
            let imm = sext(
                (bit(c, 12) << 11)
                    | (bit(c, 11) << 4)
                    | (bits(c, 10, 9) << 8)
                    | (bit(c, 8) << 10)
                    | (bit(c, 7) << 6)
                    | (bit(c, 6) << 7)
                    | (bits(c, 5, 3) << 1)
                    | (bit(c, 2) << 5),
                12,
            );
            j(imm, 0)
        }
        (1, 6) | (1, 7) => {
            // C.BEQZ / C.BNEZ
            let imm = sext(
                (bit(c, 12) << 8)
                    | (bits(c, 11, 10) << 3)
                    | (bits(c, 6, 5) << 6)
                    | (bits(c, 4, 3) << 1)
                    | (bit(c, 2) << 5),
                9,
            );
            b(imm, 0, rs1p, f3 - 6)
        }

        // Quadrant 2.
        (2, 0) => {
            // C.SLLI
            let shamt = (bit(c, 12) << 5) | bits(c, 6, 2);
            i(shamt as i32, rd, 1, rd, OP_IMM)
        }
        (2, 1) | (2, 3) => {
            // C.FLDSP / C.LDSP
            if f3 == 3 && rd == 0 {
                return None;
            }
            let imm = (bit(c, 12) << 5) | (bits(c, 6, 5) << 3) | (bits(c, 4, 2) << 6);
            let opc = if f3 == 1 { OP_LOAD_FP } else { OP_LOAD };
            i(imm as i32, 2, 3, rd, opc)
        }
        (2, 2) => {
            // C.LWSP
            if rd == 0 {
                return None;
            }
            let imm = (bit(c, 12) << 5) | (bits(c, 6, 4) << 2) | (bits(c, 3, 2) << 6);
            i(imm as i32, 2, 2, rd, OP_LOAD)
        }
        (2, 4) => match (bit(c, 12), rd, rs2) {
            (0, 0, 0) => return None,
            (0, _, 0) => i(0, rd, 0, 0, OP_JALR),  // C.JR
            (0, _, _) => r(0, rs2, 0, 0, rd, OP),  // C.MV
            (1, 0, 0) => 0x0010_0073,              // C.EBREAK
            (1, _, 0) => i(0, rd, 0, 1, OP_JALR),  // C.JALR
            (1, _, _) => r(0, rs2, rd, 0, rd, OP), // C.ADD
            _ => unreachable!(),
        },
        (2, 5) | (2, 7) => {
            // C.FSDSP / C.SDSP
            let imm = (bits(c, 12, 10) << 3) | (bits(c, 9, 7) << 6);
            let opc = if f3 == 5 { OP_STORE_FP } else { OP_STORE };
            s(imm as i32, rs2, 2, 3, opc)
        }
        (2, 6) => {
            // C.SWSP
            let imm = (bits(c, 12, 9) << 2) | (bits(c, 8, 7) << 6);
            s(imm as i32, rs2, 2, 2, OP_STORE)
        }
        _ => return None,
    };
    Some(insn)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference pairs: compressed encoding from `riscv64-elf-as -march=rv64gc`,
    // expected expansion from assembling the equivalent instruction with `-march=rv64g`.
    #[test]
    fn known_expansions() {
        let cases: &[(u16, u32)] = &[
            (0x0040, 0x00410413), // c.addi4spn s0,sp,4
            (0x6398, 0x0007b703), // c.ld a4,0(a5)
            (0x4398, 0x0007a703), // c.lw a4,0(a5)
            (0xe398, 0x00e7b023), // c.sd a4,0(a5)
            (0xc3d8, 0x00e7a223), // c.sw a4,4(a5)
            (0x2798, 0x0087b707), // c.fld fa4,8(a5)
            (0xa798, 0x00e7b427), // c.fsd fa4,8(a5)
            (0x1141, 0xff010113), // c.addi sp,-16
            (0x2785, 0x0017879b), // c.addiw a5,1
            (0x4505, 0x00100513), // c.li a0,1
            (0x7179, 0xfd010113), // c.addi16sp sp,-48
            (0x6785, 0x000017b7), // c.lui a5,0x1
            (0x77fd, 0xfffff7b7), // c.lui a5,0xfffff
            (0x8385, 0x0017d793), // c.srli a5,0x1
            (0x9785, 0x4217d793), // c.srai a5,0x21
            (0x9bf9, 0xffe7f793), // c.andi a5,-2
            (0x8f99, 0x40e787b3), // c.sub a5,a4
            (0x8fb9, 0x00e7c7b3), // c.xor a5,a4
            (0x8fd9, 0x00e7e7b3), // c.or a5,a4
            (0x8ff9, 0x00e7f7b3), // c.and a5,a4
            (0x9f99, 0x40e787bb), // c.subw a5,a4
            (0x9fb9, 0x00e787bb), // c.addw a5,a4
            (0xa001, 0x0000006f), // c.j 2c
            (0xb001, 0x801ff06f), // c.j fffffffffffff82e
            (0xc391, 0x00078263), // c.beqz a5,34
            (0xf381, 0xf00790e3), // c.bnez a5,ffffffffffffff32
            (0x0786, 0x00179793), // c.slli a5,0x1
            (0x60a2, 0x00813083), // c.ldsp ra,8(sp)
            (0x57fe, 0x0fc12783), // c.lwsp a5,252(sp)
            (0x357e, 0x1f813507), // c.fldsp fa0,504(sp)
            (0x8082, 0x00008067), // c.jr ra
            (0x853e, 0x00f00533), // c.mv a0,a5
            (0x9002, 0x00100073), // c.ebreak
            (0x9782, 0x000780e7), // c.jalr a5
            (0x953e, 0x00f50533), // c.add a0,a5
            (0xe406, 0x00113423), // c.sdsp ra,8(sp)
            (0xc03e, 0x00f12023), // c.swsp a5,0(sp)
            (0xbfaa, 0x1ea13c27), // c.fsdsp fa0,504(sp)
        ];
        for &(c, want) in cases {
            assert_eq!(expand(c), Some(want), "c={c:#06x}");
        }
    }

    #[test]
    fn reserved_encodings() {
        assert_eq!(expand(0x0000), None); // all-zero
        assert_eq!(expand(0x6101), None); // c.addi16sp with imm 0
        assert_eq!(expand(0x6081), None); // c.lui x1,0
        assert_eq!(expand(0x2001), None); // c.addiw x0
        assert_eq!(expand(0x8002), None); // c.jr x0
        assert_eq!(expand(0x4002), None); // c.lwsp x0
    }
}
