//! F, D, Zfh, Zfhmin, Zfbfmin and Zfa extension execution.

use rivemu_core::AccessSize;
use rivemu_mem::AddressSpace;
use rivemu_softfloat::{
    self as sf, BF16_CANONICAL_NAN, F16_CANONICAL_NAN, F32_CANONICAL_NAN, Flags, RoundingMode,
};

use crate::decode::{Insn, Op};
use crate::hart::{Exception, Hart, cause};

const BOX16: u64 = 0xffff_ffff_ffff_0000;
const BOX: u64 = 0xffff_ffff_0000_0000;
const SIGN16: u16 = 1 << 15;
const SIGN32: u32 = 1 << 31;
const SIGN64: u64 = 1 << 63;

impl Hart {
    /// Reads an f register as a half, unboxing (improperly boxed → canonical NaN).
    #[inline]
    pub(crate) fn f16(&self, r: u8) -> u16 {
        let v = self.f[r as usize];
        if v & BOX16 == BOX16 {
            v as u16
        } else {
            F16_CANONICAL_NAN
        }
    }

    /// Reads an f register as a BF16, unboxing (improperly boxed → canonical BF16 NaN).
    #[inline]
    fn bf16(&self, r: u8) -> u16 {
        let v = self.f[r as usize];
        if v & BOX16 == BOX16 {
            v as u16
        } else {
            BF16_CANONICAL_NAN
        }
    }

    #[inline]
    pub(crate) fn set_f16(&mut self, r: u8, v: u16) {
        self.f[r as usize] = BOX16 | u64::from(v);
        self.mark_fs_dirty();
    }

    /// Reads an f register as a single, unboxing (improperly boxed → canonical NaN).
    #[inline]
    pub(crate) fn f32(&self, r: u8) -> u32 {
        let v = self.f[r as usize];
        if v & BOX == BOX {
            v as u32
        } else {
            F32_CANONICAL_NAN
        }
    }

    #[inline]
    pub(crate) fn set_f32(&mut self, r: u8, v: u32) {
        self.f[r as usize] = BOX | u64::from(v);
        self.mark_fs_dirty();
    }

    #[inline]
    pub(crate) fn set_f64(&mut self, r: u8, v: u64) {
        self.f[r as usize] = v;
        self.mark_fs_dirty();
    }

    #[inline]
    pub(crate) fn accrue(&mut self, fl: Flags) {
        if fl.0 != 0 {
            self.csr.fflags |= u64::from(fl.0);
            self.mark_fs_dirty();
        }
    }

    pub(crate) fn exec_fp(&mut self, bus: &AddressSpace, insn: &Insn) -> Result<(), Exception> {
        let illegal = Exception::new(cause::ILLEGAL_INSN, u64::from(insn.raw));
        if !self.fs_enabled() {
            return Err(illegal);
        }
        let rm_bits = if insn.funct == 7 {
            self.csr.frm
        } else {
            u64::from(insn.funct)
        };
        let uses_rm = matches!(
            insn.op,
            Op::Fmadd
                | Op::Fmsub
                | Op::Fnmsub
                | Op::Fnmadd
                | Op::Fadd
                | Op::Fsub
                | Op::Fmul
                | Op::Fdiv
                | Op::Fsqrt
                | Op::FcvtIntF
                | Op::FcvtFInt
                | Op::FcvtFF
                | Op::Fround
                | Op::Froundnx
        );
        let rm = match RoundingMode::from_bits(rm_bits) {
            Some(rm) => rm,
            None if uses_rm => return Err(illegal),
            None => RoundingMode::NearestEven,
        };
        let mut fl = Flags::default();
        let (rd, rs1, rs2, rs3) = (insn.rd, insn.rs1, insn.rs2, insn.rs3);
        let xs1 = self.x[rs1 as usize];
        let fmt = insn.imm;

        match insn.op {
            Op::Fload => {
                let addr = xs1.wrapping_add(insn.imm as u64);
                match insn.funct {
                    1 => {
                        let v = self.load(bus, addr, AccessSize::Half)?;
                        self.set_f16(rd, v as u16);
                    }
                    3 => {
                        let v = self.load(bus, addr, AccessSize::Double)?;
                        self.set_f64(rd, v);
                    }
                    _ => {
                        let v = self.load(bus, addr, AccessSize::Word)?;
                        self.set_f32(rd, v as u32);
                    }
                }
            }
            Op::Fstore => {
                let addr = xs1.wrapping_add(insn.imm as u64);
                let size = match insn.funct {
                    1 => AccessSize::Half,
                    3 => AccessSize::Double,
                    _ => AccessSize::Word,
                };
                self.store(bus, addr, size, self.f[rs2 as usize])?;
            }
            Op::Fmadd | Op::Fmsub | Op::Fnmsub | Op::Fnmadd => {
                let (neg_prod, neg_add) = match insn.op {
                    Op::Fmadd => (false, false),
                    Op::Fmsub => (false, true),
                    Op::Fnmsub => (true, false),
                    _ => (true, true),
                };
                match fmt {
                    1 => {
                        let mut a = self.f[rs1 as usize];
                        let b = self.f[rs2 as usize];
                        let mut c = self.f[rs3 as usize];
                        if neg_prod {
                            a ^= SIGN64;
                        }
                        if neg_add {
                            c ^= SIGN64;
                        }
                        let v = sf::f64_mul_add(a, b, c, rm, &mut fl);
                        self.set_f64(rd, v);
                    }
                    2 => {
                        let mut a = self.f16(rs1);
                        let b = self.f16(rs2);
                        let mut c = self.f16(rs3);
                        if neg_prod {
                            a ^= SIGN16;
                        }
                        if neg_add {
                            c ^= SIGN16;
                        }
                        let v = sf::f16_mul_add(a, b, c, rm, &mut fl);
                        self.set_f16(rd, v);
                    }
                    _ => {
                        let mut a = self.f32(rs1);
                        let b = self.f32(rs2);
                        let mut c = self.f32(rs3);
                        if neg_prod {
                            a ^= SIGN32;
                        }
                        if neg_add {
                            c ^= SIGN32;
                        }
                        let v = sf::f32_mul_add(a, b, c, rm, &mut fl);
                        self.set_f32(rd, v);
                    }
                }
            }
            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv => match fmt {
                1 => {
                    let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                    let f = match insn.op {
                        Op::Fadd => sf::f64_add,
                        Op::Fsub => sf::f64_sub,
                        Op::Fmul => sf::f64_mul,
                        _ => sf::f64_div,
                    };
                    let v = f(a, b, rm, &mut fl);
                    self.set_f64(rd, v);
                }
                2 => {
                    let (a, b) = (self.f16(rs1), self.f16(rs2));
                    let f = match insn.op {
                        Op::Fadd => sf::f16_add,
                        Op::Fsub => sf::f16_sub,
                        Op::Fmul => sf::f16_mul,
                        _ => sf::f16_div,
                    };
                    let v = f(a, b, rm, &mut fl);
                    self.set_f16(rd, v);
                }
                _ => {
                    let (a, b) = (self.f32(rs1), self.f32(rs2));
                    let f = match insn.op {
                        Op::Fadd => sf::f32_add,
                        Op::Fsub => sf::f32_sub,
                        Op::Fmul => sf::f32_mul,
                        _ => sf::f32_div,
                    };
                    let v = f(a, b, rm, &mut fl);
                    self.set_f32(rd, v);
                }
            },
            Op::Fsqrt => match fmt {
                1 => {
                    let v = sf::f64_sqrt(self.f[rs1 as usize], rm, &mut fl);
                    self.set_f64(rd, v);
                }
                2 => {
                    let v = sf::f16_sqrt(self.f16(rs1), rm, &mut fl);
                    self.set_f16(rd, v);
                }
                _ => {
                    let v = sf::f32_sqrt(self.f32(rs1), rm, &mut fl);
                    self.set_f32(rd, v);
                }
            },
            Op::Fsgnj | Op::Fsgnjn | Op::Fsgnjx => match fmt {
                1 => {
                    let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                    let s = match insn.op {
                        Op::Fsgnj => b,
                        Op::Fsgnjn => !b,
                        _ => a ^ b,
                    } & SIGN64;
                    self.set_f64(rd, (a & !SIGN64) | s);
                }
                2 => {
                    let (a, b) = (self.f16(rs1), self.f16(rs2));
                    let s = match insn.op {
                        Op::Fsgnj => b,
                        Op::Fsgnjn => !b,
                        _ => a ^ b,
                    } & SIGN16;
                    self.set_f16(rd, (a & !SIGN16) | s);
                }
                _ => {
                    let (a, b) = (self.f32(rs1), self.f32(rs2));
                    let s = match insn.op {
                        Op::Fsgnj => b,
                        Op::Fsgnjn => !b,
                        _ => a ^ b,
                    } & SIGN32;
                    self.set_f32(rd, (a & !SIGN32) | s);
                }
            },
            Op::Fmin | Op::Fmax | Op::Fminm | Op::Fmaxm => {
                let max = matches!(insn.op, Op::Fmax | Op::Fmaxm);
                let minm = matches!(insn.op, Op::Fminm | Op::Fmaxm);
                match fmt {
                    1 => {
                        let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                        let v = match (minm, max) {
                            (false, false) => sf::f64_min(a, b, &mut fl),
                            (false, true) => sf::f64_max(a, b, &mut fl),
                            (true, false) => sf::f64_minm(a, b, &mut fl),
                            (true, true) => sf::f64_maxm(a, b, &mut fl),
                        };
                        self.set_f64(rd, v);
                    }
                    2 => {
                        let (a, b) = (self.f16(rs1), self.f16(rs2));
                        let v = match (minm, max) {
                            (false, false) => sf::f16_min(a, b, &mut fl),
                            (false, true) => sf::f16_max(a, b, &mut fl),
                            (true, false) => sf::f16_minm(a, b, &mut fl),
                            (true, true) => sf::f16_maxm(a, b, &mut fl),
                        };
                        self.set_f16(rd, v);
                    }
                    _ => {
                        let (a, b) = (self.f32(rs1), self.f32(rs2));
                        let v = match (minm, max) {
                            (false, false) => sf::f32_min(a, b, &mut fl),
                            (false, true) => sf::f32_max(a, b, &mut fl),
                            (true, false) => sf::f32_minm(a, b, &mut fl),
                            (true, true) => sf::f32_maxm(a, b, &mut fl),
                        };
                        self.set_f32(rd, v);
                    }
                }
            }
            Op::Fli => match fmt {
                1 => self.set_f64(rd, sf::FLI_D_TABLE[rs1 as usize]),
                2 => self.set_f16(rd, sf::FLI_H_TABLE[rs1 as usize]),
                _ => self.set_f32(rd, sf::FLI_S_TABLE[rs1 as usize]),
            },
            Op::Fround | Op::Froundnx => {
                let nx = insn.op == Op::Froundnx;
                match fmt {
                    1 => {
                        let a = self.f[rs1 as usize];
                        let v = if nx {
                            sf::f64_roundnx(a, rm, &mut fl)
                        } else {
                            sf::f64_round(a, rm, &mut fl)
                        };
                        self.set_f64(rd, v);
                    }
                    2 => {
                        let a = self.f16(rs1);
                        let v = if nx {
                            sf::f16_roundnx(a, rm, &mut fl)
                        } else {
                            sf::f16_round(a, rm, &mut fl)
                        };
                        self.set_f16(rd, v);
                    }
                    _ => {
                        let a = self.f32(rs1);
                        let v = if nx {
                            sf::f32_roundnx(a, rm, &mut fl)
                        } else {
                            sf::f32_round(a, rm, &mut fl)
                        };
                        self.set_f32(rd, v);
                    }
                }
            }
            Op::FcvtmodWD => {
                let v = sf::f64_to_i32_mod(self.f[rs1 as usize], &mut fl) as i64 as u64;
                self.set_x(rd, v);
            }
            Op::FcvtIntF => {
                let v = match fmt {
                    1 => {
                        let a = self.f[rs1 as usize];
                        match rs2 {
                            0 => sf::f64_to_i32(a, rm, &mut fl) as i64 as u64,
                            1 => sf::f64_to_u32(a, rm, &mut fl) as i32 as i64 as u64,
                            2 => sf::f64_to_i64(a, rm, &mut fl) as u64,
                            _ => sf::f64_to_u64(a, rm, &mut fl),
                        }
                    }
                    2 => {
                        let a = self.f16(rs1);
                        match rs2 {
                            0 => sf::f16_to_i32(a, rm, &mut fl) as i64 as u64,
                            1 => sf::f16_to_u32(a, rm, &mut fl) as i32 as i64 as u64,
                            2 => sf::f16_to_i64(a, rm, &mut fl) as u64,
                            _ => sf::f16_to_u64(a, rm, &mut fl),
                        }
                    }
                    _ => {
                        let a = self.f32(rs1);
                        match rs2 {
                            0 => sf::f32_to_i32(a, rm, &mut fl) as i64 as u64,
                            1 => sf::f32_to_u32(a, rm, &mut fl) as i32 as i64 as u64,
                            2 => sf::f32_to_i64(a, rm, &mut fl) as u64,
                            _ => sf::f32_to_u64(a, rm, &mut fl),
                        }
                    }
                };
                self.set_x(rd, v);
            }
            Op::FcvtFInt => match fmt {
                1 => {
                    let v = match rs2 {
                        0 => sf::i32_to_f64(xs1 as i32, rm, &mut fl),
                        1 => sf::u32_to_f64(xs1 as u32, rm, &mut fl),
                        2 => sf::i64_to_f64(xs1 as i64, rm, &mut fl),
                        _ => sf::u64_to_f64(xs1, rm, &mut fl),
                    };
                    self.set_f64(rd, v);
                }
                2 => {
                    let v = match rs2 {
                        0 => sf::i32_to_f16(xs1 as i32, rm, &mut fl),
                        1 => sf::u32_to_f16(xs1 as u32, rm, &mut fl),
                        2 => sf::i64_to_f16(xs1 as i64, rm, &mut fl),
                        _ => sf::u64_to_f16(xs1, rm, &mut fl),
                    };
                    self.set_f16(rd, v);
                }
                _ => {
                    let v = match rs2 {
                        0 => sf::i32_to_f32(xs1 as i32, rm, &mut fl),
                        1 => sf::u32_to_f32(xs1 as u32, rm, &mut fl),
                        2 => sf::i64_to_f32(xs1 as i64, rm, &mut fl),
                        _ => sf::u64_to_f32(xs1, rm, &mut fl),
                    };
                    self.set_f32(rd, v);
                }
            },
            Op::FcvtFF => match (fmt, rs2) {
                (0, 1) => {
                    let v = sf::f64_to_f32(self.f[rs1 as usize], rm, &mut fl);
                    self.set_f32(rd, v);
                }
                (1, 0) => {
                    let v = sf::f32_to_f64(self.f32(rs1), &mut fl);
                    self.set_f64(rd, v);
                }
                (0, 2) => {
                    let v = sf::f16_to_f32(self.f16(rs1), &mut fl);
                    self.set_f32(rd, v);
                }
                (2, 0) => {
                    let v = sf::f32_to_f16(self.f32(rs1), rm, &mut fl);
                    self.set_f16(rd, v);
                }
                (1, 2) => {
                    let v = sf::f16_to_f64(self.f16(rs1), &mut fl);
                    self.set_f64(rd, v);
                }
                (2, 1) => {
                    let v = sf::f64_to_f16(self.f[rs1 as usize], rm, &mut fl);
                    self.set_f16(rd, v);
                }
                (0, 6) => {
                    let v = sf::bf16_to_f32(self.bf16(rs1), &mut fl);
                    self.set_f32(rd, v);
                }
                (2, 8) => {
                    let v = sf::f32_to_bf16(self.f32(rs1), rm, &mut fl);
                    self.set_f16(rd, v);
                }
                _ => return Err(illegal),
            },
            Op::FmvXF => {
                let v = self.f[rs1 as usize];
                let v = match fmt {
                    1 => v,
                    2 => v as i16 as i64 as u64,
                    _ => v as i32 as i64 as u64,
                };
                self.set_x(rd, v);
            }
            Op::FmvFX => match fmt {
                1 => self.set_f64(rd, xs1),
                2 => self.set_f16(rd, xs1 as u16),
                _ => self.set_f32(rd, xs1 as u32),
            },
            Op::Feq | Op::Flt | Op::Fle | Op::Fleq | Op::Fltq => {
                let r = match fmt {
                    1 => {
                        let (a, b) = (self.f[rs1 as usize], self.f[rs2 as usize]);
                        match insn.op {
                            Op::Feq => sf::f64_eq(a, b, &mut fl),
                            Op::Flt => sf::f64_lt(a, b, &mut fl),
                            Op::Fle => sf::f64_le(a, b, &mut fl),
                            Op::Fleq => sf::f64_leq(a, b, &mut fl),
                            _ => sf::f64_ltq(a, b, &mut fl),
                        }
                    }
                    2 => {
                        let (a, b) = (self.f16(rs1), self.f16(rs2));
                        match insn.op {
                            Op::Feq => sf::f16_eq(a, b, &mut fl),
                            Op::Flt => sf::f16_lt(a, b, &mut fl),
                            Op::Fle => sf::f16_le(a, b, &mut fl),
                            Op::Fleq => sf::f16_leq(a, b, &mut fl),
                            _ => sf::f16_ltq(a, b, &mut fl),
                        }
                    }
                    _ => {
                        let (a, b) = (self.f32(rs1), self.f32(rs2));
                        match insn.op {
                            Op::Feq => sf::f32_eq(a, b, &mut fl),
                            Op::Flt => sf::f32_lt(a, b, &mut fl),
                            Op::Fle => sf::f32_le(a, b, &mut fl),
                            Op::Fleq => sf::f32_leq(a, b, &mut fl),
                            _ => sf::f32_ltq(a, b, &mut fl),
                        }
                    }
                };
                self.set_x(rd, u64::from(r));
            }
            Op::Fclass => {
                let v = match fmt {
                    1 => sf::f64_classify(self.f[rs1 as usize]),
                    2 => sf::f16_classify(self.f16(rs1)),
                    _ => sf::f32_classify(self.f32(rs1)),
                };
                self.set_x(rd, v);
            }
            _ => return Err(illegal),
        }
        self.accrue(fl);
        Ok(())
    }
}
